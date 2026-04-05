mod config;
mod entity;
mod error;
mod pipeline;
mod predicate;
mod retrieval;
mod types;

pub use config::{ConfigError, LoadedConfig, load_or_init, resolve_namespace_groups};
pub use entity::EntityResolver;
pub use error::CoreError;
pub use predicate::PredicateNormalizer;
use retrieval::SECONDS_PER_DAY;
pub use retrieval::compute_recency_boost;
pub use types::*;

pub use ferrex_embed::{ModelTier, RerankerTier};
pub use ferrex_store::{Entity, Memory, MemoryType};

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use ferrex_embed::{Embedder, Reranker};
use ferrex_store::{MetadataStore, QdrantSidecar, SqliteStore, VectorStore};
use qdrant_client::qdrant::{Condition, Filter};
use uuid::Uuid;

use crate::pipeline::StoreContext;

const DEFAULT_RECALL_LIMIT: usize = 10;
const MIN_RERANK_POOL_SIZE: usize = 20;
const STATS_RECENT_COUNT: usize = 5;

#[derive(Debug, Default)]
pub struct AuditReport {
    pub sqlite_only: u64,
    pub qdrant_only: u64,
    pub fixed: u64,
}

#[derive(Debug, Default)]
pub struct BackfillReport {
    pub scanned: u64,
    pub updated: u64,
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub compensated_stores: u64,
    pub rolled_forward_forgets: u64,
    pub cleared_pre_qdrant: u64,
}

pub struct MemoryService {
    embedder: Embedder,
    reranker: Reranker,
    metadata_store: SqliteStore,
    vector_store: VectorStore,
    sidecar: Option<QdrantSidecar>,
    config: FerrexConfig,
    normalizers: HashMap<String, Arc<PredicateNormalizer>>,
    default_normalizer: Arc<PredicateNormalizer>,
}

impl MemoryService {
    pub async fn from_config(config: FerrexConfig) -> Result<Self, CoreError> {
        if let Some(parent) = config.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CoreError::Validation(format!("failed to create db directory: {e}"))
            })?;
        }

        let embedder = Embedder::new(config.model_tier)?;
        let reranker = Reranker::new(config.reranker_tier)?;

        let (vector_store, sidecar) = if let Some(ref url) = config.qdrant_url {
            let vs = VectorStore::new(url, embedder.dimension())?;
            (vs, None)
        } else {
            let sc = QdrantSidecar::start(&config.qdrant_bin, config.qdrant_port).await?;
            let vs = VectorStore::new(&sc.url(), embedder.dimension())?;
            (vs, Some(sc))
        };

        let metadata_store =
            SqliteStore::open_with_pool_size(&config.db_path, config.reader_pool_size)?;

        let model_key = "embedding_model";
        let current_model = config.model_tier.model_name();
        let stored_model = metadata_store.get_metadata(model_key).await?;
        if let Some(stored) = stored_model {
            if stored != current_model {
                return Err(CoreError::Validation(format!(
                    "embedding model mismatch: stored={stored}, current={current_model}. \
                     Changing models would corrupt vector similarity. \
                     Use the same model or start with a fresh database."
                )));
            }
        } else {
            metadata_store
                .set_metadata(model_key, current_model)
                .await?;
        }

        vector_store.ensure_collection(&config.namespace).await?;

        let default_normalizer =
            Arc::new(PredicateNormalizer::new(config.predicates.groups.clone()));
        let mut normalizers = HashMap::new();
        for ns_name in config.predicates.namespaces.keys() {
            let groups = resolve_namespace_groups(&config.predicates, ns_name);
            normalizers.insert(ns_name.clone(), Arc::new(PredicateNormalizer::new(groups)));
        }

        let service = Self {
            embedder,
            reranker,
            metadata_store,
            vector_store,
            sidecar,
            config,
            normalizers,
            default_normalizer,
        };

        let report = service.recover_on_startup().await?;
        if report.compensated_stores + report.rolled_forward_forgets + report.cleared_pre_qdrant > 0
        {
            tracing::info!(
                compensated_stores = report.compensated_stores,
                rolled_forward_forgets = report.rolled_forward_forgets,
                cleared_pre_qdrant = report.cleared_pre_qdrant,
                "recovered pending ops on startup"
            );
        }

        Ok(service)
    }

    pub async fn recover_on_startup(&self) -> Result<RecoveryReport, CoreError> {
        use ferrex_store::PendingOpKind;

        let pending = self.metadata_store.list_pending_ops().await?;
        let mut report = RecoveryReport::default();
        for op in pending {
            match (op.kind, op.qdrant_written) {
                (_, false) => {
                    self.metadata_store.clear_pending_op(&op.op_id).await?;
                    report.cleared_pre_qdrant += 1;
                }
                (PendingOpKind::Store | PendingOpKind::Supersede, true) => {
                    let Ok(uuid) = Uuid::parse_str(&op.memory_id) else {
                        tracing::warn!(op_id = %op.op_id, "invalid memory_id uuid in journal");
                        self.metadata_store.clear_pending_op(&op.op_id).await?;
                        continue;
                    };
                    self.vector_store
                        .delete_by_ids(&op.namespace, &[uuid])
                        .await?;
                    // SQLite insert may or may not have landed. Either way, the
                    // memory is unreachable without its Qdrant point; drop it.
                    self.metadata_store
                        .delete_memories(std::slice::from_ref(&op.memory_id))
                        .await?;
                    self.metadata_store.clear_pending_op(&op.op_id).await?;
                    report.compensated_stores += 1;
                }
                (PendingOpKind::Forget, true) => {
                    self.metadata_store
                        .delete_memories(std::slice::from_ref(&op.memory_id))
                        .await?;
                    self.metadata_store.clear_pending_op(&op.op_id).await?;
                    report.rolled_forward_forgets += 1;
                }
            }
        }
        Ok(report)
    }

    pub async fn audit_reconcile(
        &self,
        fix: bool,
        fix_limit: u64,
    ) -> Result<AuditReport, CoreError> {
        use std::collections::HashSet;

        let qdrant_ids = self
            .vector_store
            .scroll_all_ids(&self.config.namespace)
            .await?;
        let qdrant_set: HashSet<String> = qdrant_ids.iter().map(ToString::to_string).collect();

        let sqlite_ids = self
            .metadata_store
            .list_all_memory_ids(&self.config.namespace)
            .await?;
        let sqlite_set: HashSet<String> = sqlite_ids.into_iter().collect();

        let qdrant_only: Vec<String> = qdrant_set.difference(&sqlite_set).cloned().collect();
        let sqlite_only_count = sqlite_set.difference(&qdrant_set).count() as u64;

        let mut report = AuditReport {
            sqlite_only: sqlite_only_count,
            qdrant_only: qdrant_only.len() as u64,
            fixed: 0,
        };
        if fix {
            if qdrant_only.len() as u64 > fix_limit {
                return Err(CoreError::Validation(format!(
                    "audit refuses to auto-fix {} qdrant-only ids (limit {fix_limit})",
                    qdrant_only.len(),
                )));
            }
            let uuids: Vec<uuid::Uuid> = qdrant_only
                .iter()
                .filter_map(|s| uuid::Uuid::parse_str(s).ok())
                .collect();
            self.vector_store
                .delete_by_ids(&self.config.namespace, &uuids)
                .await?;
            report.fixed = uuids.len() as u64;
        }
        Ok(report)
    }

    pub async fn backfill_normalized_predicates(
        &self,
        namespace: Option<&str>,
        dry_run: bool,
    ) -> Result<BackfillReport, CoreError> {
        let rows = self
            .metadata_store
            .semantic_rows_missing_normalized_predicate(namespace)
            .await?;
        let mut report = BackfillReport {
            scanned: rows.len() as u64,
            updated: 0,
        };
        for row in rows {
            let normalizer = self
                .normalizers
                .get(&row.namespace)
                .cloned()
                .unwrap_or_else(|| Arc::clone(&self.default_normalizer));
            let Some(pred) = row.predicate.as_deref() else {
                continue;
            };
            let normalized = normalizer.normalize(pred);
            if dry_run {
                report.updated += 1;
                continue;
            }
            self.metadata_store
                .set_normalized_predicate(&row.id, &normalized)
                .await?;
            report.updated += 1;
        }
        Ok(report)
    }

    pub async fn store(&self, req: StoreRequest) -> Result<StoreResponse, CoreError> {
        let memory_type = detect_memory_type(&req);
        let namespace = req
            .namespace
            .clone()
            .unwrap_or_else(|| self.config.namespace.clone());
        let normalizer = self
            .normalizers
            .get(&namespace)
            .cloned()
            .unwrap_or_else(|| Arc::clone(&self.default_normalizer));

        if namespace != self.config.namespace {
            self.vector_store.ensure_collection(&namespace).await?;
        }

        let mut ctx = StoreContext::new(
            req,
            namespace,
            memory_type,
            &normalizer,
            &self.config.deduplication,
            &self.config.conflict,
        );

        pipeline::validate::run(&ctx)?;

        let supersedes_target = ctx.req.supersedes.clone();
        let is_supersede = supersedes_target.is_some();

        if memory_type == MemoryType::Semantic {
            pipeline::normalize_predicate::run(&mut ctx)?;
        }
        pipeline::embed::run(&mut ctx, &self.embedder).await?;
        if !is_supersede {
            pipeline::dedup::run(&ctx, &self.vector_store).await?;
            pipeline::conflict::run(&mut ctx, &self.metadata_store).await?;
        }
        pipeline::resolve_entities::run(
            &mut ctx,
            &self.metadata_store,
            &self.vector_store,
            &self.embedder,
        )
        .await?;

        let memory = if let Some(ref target_id) = supersedes_target {
            self.supersede_write(&ctx, target_id).await?
        } else {
            pipeline::write::run(&ctx, &self.metadata_store, &self.vector_store).await?
        };

        let superseded = if let Some(id) = supersedes_target {
            vec![id]
        } else {
            ctx.superseded_ids.clone()
        };

        Ok(StoreResponse {
            id: memory.id,
            memory_type: memory_type.as_str().to_string(),
            superseded,
        })
    }

    async fn supersede_write(
        &self,
        ctx: &StoreContext<'_>,
        target_id: &str,
    ) -> Result<Memory, CoreError> {
        let target = self
            .metadata_store
            .get_memory(target_id)
            .await?
            .ok_or_else(|| {
                CoreError::Validation(format!("supersedes target not found: {target_id}"))
            })?;
        if target.namespace != ctx.namespace {
            return Err(CoreError::Validation(format!(
                "supersedes target {target_id} is in namespace {}, not {}",
                target.namespace, ctx.namespace
            )));
        }
        if target.t_invalid.is_some() {
            return Err(CoreError::Validation(format!(
                "supersedes target {target_id} is already invalidated"
            )));
        }

        pipeline::write::run_supersede(ctx, &self.metadata_store, &self.vector_store, target_id)
            .await
    }

    pub async fn recall(&self, req: RecallRequest) -> Result<Vec<(Memory, f32)>, CoreError> {
        if req.time_range.is_some() {
            return Err(CoreError::Validation(
                "time_range filtering is not yet implemented".into(),
            ));
        }
        if req.include_stale.is_some() {
            return Err(CoreError::Validation(
                "include_stale filtering is not yet implemented".into(),
            ));
        }
        if req.include_invalidated.is_some() {
            return Err(CoreError::Validation(
                "include_invalidated filtering is not yet implemented".into(),
            ));
        }

        let namespace = req.namespace.as_deref().unwrap_or(&self.config.namespace);
        let limit = req.limit.unwrap_or(DEFAULT_RECALL_LIMIT);
        let candidate_pool_size = limit.max(MIN_RERANK_POOL_SIZE);

        let embedding = self.embedder.embed(&req.query).await?;

        let mut must_conditions = vec![Condition::matches(
            ferrex_store::POINT_TYPE_FIELD,
            ferrex_store::POINT_TYPE_MEMORY.to_string(),
        )];

        if let Some(ref types) = req.types {
            let type_strings: Vec<String> = types.iter().map(|t| t.as_str().to_string()).collect();
            must_conditions.push(Condition::matches("memory_type", type_strings));
        }

        let mut filter = Filter::must(must_conditions);

        if let Some(ref entities) = req.entities
            && !entities.is_empty()
        {
            let should = entities
                .iter()
                .map(|e| Condition::matches("entities", e.clone()))
                .collect();
            filter = Filter { should, ..filter };
        }

        let results = self
            .vector_store
            .search(
                namespace,
                embedding,
                &req.query,
                candidate_pool_size,
                Some(filter),
            )
            .await?;

        if results.is_empty() {
            return Ok(vec![]);
        }

        let ids: Vec<String> = results.iter().map(|(id, _)| id.clone()).collect();
        let memories = self.metadata_store.get_memories_by_ids(&ids).await?;

        let memory_map: HashMap<&str, &Memory> = memories
            .iter()
            .filter(|m| m.t_invalid.is_none())
            .map(|m| (m.id.as_str(), m))
            .collect();

        let ordered: Vec<&Memory> = results
            .iter()
            .filter_map(|(id, _)| memory_map.get(id.as_str()).copied())
            .collect();

        if ordered.is_empty() {
            return Ok(vec![]);
        }

        let doc_texts: Vec<String> = ordered.iter().map(|m| m.searchable_text()).collect();
        let doc_refs: Vec<&str> = doc_texts.iter().map(String::as_str).collect();

        let reranked = self.reranker.rerank(&req.query, &doc_refs, limit).await?;

        let now = Utc::now();
        let mut scored: Vec<(Memory, f32)> = reranked
            .iter()
            .filter_map(|r| {
                let memory = ordered.get(r.index)?;
                #[allow(clippy::cast_precision_loss)]
                let age_days = (now - memory.created_at).num_seconds() as f64 / SECONDS_PER_DAY;
                let recency = compute_recency_boost(memory.memory_type, age_days);
                #[allow(clippy::cast_possible_truncation)]
                let final_score = (f64::from(r.score) * recency) as f32;
                Some(((*memory).clone(), final_score))
            })
            .collect();

        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);

        let accessed_ids: Vec<String> = scored.iter().map(|(m, _)| m.id.clone()).collect();
        self.metadata_store
            .update_last_accessed(&accessed_ids)
            .await?;

        Ok(scored)
    }

    pub async fn stats(&self, _req: StatsRequest) -> Result<StatsResponse, CoreError> {
        let total = self.metadata_store.memory_count().await?;
        let recent = self
            .metadata_store
            .recent_memories(STATS_RECENT_COUNT)
            .await?;
        Ok(StatsResponse {
            total_memories: total,
            recent_memories: recent,
            needs_attention: NeedsAttention {
                stale_count: 0,
                conflict_count: 0,
                unvalidated_count: 0,
            },
        })
    }

    pub async fn forget(&self, req: ForgetRequest) -> Result<ForgetResponse, CoreError> {
        use ferrex_store::{PendingOp, PendingOpKind};

        for id in &req.ids {
            Uuid::parse_str(id)
                .map_err(|_| CoreError::Validation(format!("invalid UUID: {id}")))?;
        }
        if req.cascade.is_some() {
            tracing::warn!("forget: `cascade` field is deprecated and ignored");
        }

        let mut deleted = Vec::new();
        let mut not_found = Vec::new();

        for id in &req.ids {
            let Some(memory) = self.metadata_store.get_memory(id).await? else {
                not_found.push(id.clone());
                continue;
            };

            let op = PendingOp {
                op_id: Uuid::now_v7().to_string(),
                kind: PendingOpKind::Forget,
                memory_id: id.clone(),
                namespace: memory.namespace.clone(),
                target_id: None,
                qdrant_written: false,
                started_at: Utc::now(),
            };
            self.metadata_store.insert_pending_op(&op).await?;

            let uuid = Uuid::parse_str(id).map_err(|e| CoreError::Validation(e.to_string()))?;
            self.vector_store
                .delete_by_ids(&memory.namespace, &[uuid])
                .await?;
            self.metadata_store
                .mark_pending_op_qdrant_written(&op.op_id)
                .await?;

            self.metadata_store
                .delete_memories(std::slice::from_ref(id))
                .await?;
            self.metadata_store.clear_pending_op(&op.op_id).await?;

            deleted.push(id.clone());
        }

        Ok(ForgetResponse {
            message: format!("deleted {} memories", deleted.len()),
            deleted,
            not_found,
        })
    }

    pub fn reflect(&self, _req: ReflectRequest) -> Result<ReflectResponse, CoreError> {
        Ok(ReflectResponse {
            message: "reflect is not yet implemented".to_string(),
            stale: vec![],
            contradictions: vec![],
            low_access: vec![],
        })
    }

    pub const fn into_parts(mut self) -> (Self, Option<QdrantSidecar>) {
        let sidecar = self.sidecar.take();
        (self, sidecar)
    }
}

const fn detect_memory_type(req: &StoreRequest) -> MemoryType {
    match req.memory_type {
        Some(t) => t,
        None if req.subject.is_some() && req.predicate.is_some() && req.object.is_some() => {
            MemoryType::Semantic
        }
        None => MemoryType::Episodic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_detect_semantic() {
        let req = StoreRequest {
            content: None,
            memory_type: None,
            subject: Some("api-server".into()),
            predicate: Some("uses".into()),
            object: Some("tokio 1.38".into()),
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Semantic);
    }

    #[test]
    fn test_auto_detect_episodic() {
        let req = StoreRequest {
            content: Some("something happened".into()),
            memory_type: None,
            subject: None,
            predicate: None,
            object: None,
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Episodic);
    }

    #[test]
    fn test_auto_detect_explicit_procedural() {
        let req = StoreRequest {
            content: Some("step 1: do this".into()),
            memory_type: Some(MemoryType::Procedural),
            subject: None,
            predicate: None,
            object: None,
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Procedural);
    }

    #[test]
    fn test_semantic_triple_missing_predicate_detects_as_episodic() {
        let req = StoreRequest {
            content: None,
            memory_type: None,
            subject: Some("foo".into()),
            predicate: None,
            object: Some("bar".into()),
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Episodic);
    }
}
