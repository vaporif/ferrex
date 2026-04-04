mod entity;
mod error;
mod retrieval;
mod types;

pub use entity::EntityResolver;
pub use error::CoreError;
pub use retrieval::compute_recency_boost;
use retrieval::age_in_days;
pub use types::*;

pub use ferrex_embed::{ModelTier, RerankerTier};
pub use ferrex_store::{Entity, Memory, MemoryType};

use std::collections::HashMap;

use chrono::Utc;
use ferrex_embed::{Embedder, Reranker};
use ferrex_store::{MetadataStore, QdrantSidecar, SqliteStore, VectorStore};
use qdrant_client::Payload;
use qdrant_client::qdrant::{Condition, Filter};
use uuid::Uuid;

const MAX_CONTENT_LENGTH: usize = 4096;
const DEFAULT_RECALL_LIMIT: usize = 10;
const MIN_RERANK_POOL_SIZE: usize = 20;
const STATS_RECENT_COUNT: usize = 5;

pub struct MemoryService {
    embedder: Embedder,
    reranker: Reranker,
    metadata_store: SqliteStore,
    vector_store: VectorStore,
    sidecar: Option<QdrantSidecar>,
    config: FerrexConfig,
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

        let metadata_store = SqliteStore::open(&config.db_path)?;

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

        Ok(Self {
            embedder,
            reranker,
            metadata_store,
            vector_store,
            sidecar,
            config,
        })
    }

    pub async fn store(&self, req: StoreRequest) -> Result<Memory, CoreError> {
        let memory_type = detect_memory_type(&req);
        validate_store_request(&req, memory_type)?;

        let namespace = req.namespace.as_deref().unwrap_or(&self.config.namespace);
        let now = Utc::now();
        let id = Uuid::now_v7();
        let confidence = clamp_confidence(req.confidence);

        let resolved_entities = if req.entities.is_empty() {
            vec![]
        } else {
            self.vector_store.ensure_collection(namespace).await?;
            let resolver = EntityResolver {
                metadata_store: &self.metadata_store,
                vector_store: &self.vector_store,
                embedder: &self.embedder,
            };
            resolver.resolve(&req.entities, namespace).await?
        };
        let entity_names = resolved_entities.iter().map(|e| e.name.clone()).collect();

        let memory = Memory {
            id: id.to_string(),
            namespace: namespace.to_string(),
            memory_type,
            content: req.content,
            subject: req.subject,
            predicate: req.predicate,
            object: req.object,
            confidence,
            source: req.source,
            context: req.context,
            entities: entity_names,
            created_at: now,
            updated_at: now,
            t_valid: None,
            t_invalid: None,
            last_accessed: now,
            last_validated: None,
            access_count: 0,
        };

        let embed_text = memory.searchable_text();
        let embedding = self.embedder.embed(&embed_text).await?;

        // Metadata first: easier to roll back than a vector upsert
        self.metadata_store.insert_memory(&memory).await?;

        for entity in &resolved_entities {
            self.metadata_store
                .link_memory_entity(&memory.id, &entity.id)
                .await?;
        }

        let payload = Payload::try_from(serde_json::json!({
            "memory_id": id.to_string(),
            "memory_type": memory_type.as_str(),
            "namespace": namespace,
            "searchable_text": embed_text,
            "entities": &memory.entities,
            "created_at": now.to_rfc3339(),
            ferrex_store::POINT_TYPE_FIELD: ferrex_store::POINT_TYPE_MEMORY,
        }))
        .map_err(|e| CoreError::Validation(e.to_string()))?;

        self.vector_store
            .upsert(namespace, id, embedding, &embed_text, payload)
            .await?;

        Ok(memory)
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

        let memory_map: HashMap<&str, &Memory> =
            memories.iter().map(|m| (m.id.as_str(), m)).collect();

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

        let mut scored: Vec<(Memory, f32)> = reranked
            .iter()
            .filter_map(|r| {
                let memory = ordered.get(r.index)?;
                let recency = compute_recency_boost(memory.memory_type, age_in_days(memory.created_at));
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

    pub fn forget(&self, req: &ForgetRequest) -> Result<ForgetResponse, CoreError> {
        for id in &req.ids {
            id.parse::<Uuid>()
                .map_err(|_| CoreError::Validation(format!("invalid UUID: {id}")))?;
        }
        Ok(ForgetResponse {
            message: "forget is not yet implemented".to_string(),
            deleted: vec![],
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

fn validate_store_request(req: &StoreRequest, memory_type: MemoryType) -> Result<(), CoreError> {
    match memory_type {
        MemoryType::Episodic | MemoryType::Procedural => {
            let Some(content) = req.content.as_deref().filter(|c| !c.is_empty()) else {
                return Err(CoreError::Validation(format!(
                    "{memory_type} memory requires content"
                )));
            };
            if content.len() > MAX_CONTENT_LENGTH {
                return Err(CoreError::Validation(format!(
                    "content exceeds {MAX_CONTENT_LENGTH} byte limit"
                )));
            }
        }
        MemoryType::Semantic => {
            let non_empty = |opt: &Option<String>| opt.as_deref().is_some_and(|s| !s.is_empty());
            if !non_empty(&req.subject) || !non_empty(&req.predicate) || !non_empty(&req.object) {
                return Err(CoreError::Validation(
                    "semantic memory requires non-empty subject, predicate, and object".into(),
                ));
            }
        }
    }
    Ok(())
}

fn clamp_confidence(confidence: Option<f64>) -> f64 {
    confidence.map_or(1.0, |c| c.clamp(0.0, 1.0))
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
    fn test_validate_episodic_missing_content() {
        let req = StoreRequest {
            content: None,
            memory_type: Some(MemoryType::Episodic),
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
        let result = validate_store_request(&req, MemoryType::Episodic);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_content_too_long() {
        let long_content = "x".repeat(4097);
        let req = StoreRequest {
            content: Some(long_content),
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
        let result = validate_store_request(&req, MemoryType::Episodic);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_semantic_missing_triple() {
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
        let mem_type = detect_memory_type(&req);
        assert_eq!(mem_type, MemoryType::Episodic);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_clamp_confidence() {
        assert_eq!(clamp_confidence(None), 1.0);
        assert_eq!(clamp_confidence(Some(0.5)), 0.5);
        assert_eq!(clamp_confidence(Some(-0.1)), 0.0);
        assert_eq!(clamp_confidence(Some(1.5)), 1.0);
    }
}
