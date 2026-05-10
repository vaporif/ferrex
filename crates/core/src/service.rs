mod admin;
mod forget;
mod recall;
mod reflect_op;
mod stats;
mod store;
mod taxonomy;
mod timeline;

use std::collections::HashMap;
use std::sync::Arc;

use ferrex_embed::{Embedder, Reranker};
use ferrex_store::{MetadataStore, QdrantSidecar, SqliteStore, VectorStore};
use tokio_util::sync::CancellationToken;

use crate::access_tracker::AccessTracker;
use crate::cache;
use crate::config::resolve_namespace_groups;
use crate::error::CoreError;
use crate::ops_buffer::{OpsBuffer, RecoveryReport};
use crate::predicate::PredicateNormalizer;
use crate::types::FerrexConfig;

const DEFAULT_RECALL_LIMIT: usize = 10;
const MAX_RECALL_LIMIT: usize = 200;
const MIN_RERANK_POOL_SIZE: usize = 20;
const STATS_RECENT_COUNT: i64 = 5;
const MAX_SCAN_MEMORIES: usize = 10_000;
const ACCESS_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

pub struct MemoryService {
    pub(crate) embedder: Embedder,
    pub(crate) reranker: Reranker,
    pub(crate) metadata_store: Arc<SqliteStore>,
    pub(crate) vector_store: VectorStore,
    sidecar: Option<QdrantSidecar>,
    pub(crate) config: FerrexConfig,
    pub(crate) normalizers: HashMap<String, Arc<PredicateNormalizer>>,
    pub(crate) default_normalizer: Arc<PredicateNormalizer>,
    pub(crate) access_tracker: Arc<AccessTracker>,
    shutdown_token: CancellationToken,
    flush_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(crate) cache: cache::RecallCache,
    pub(crate) ops_buffer: OpsBuffer,
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
            let sc = QdrantSidecar::start(&config.qdrant_bin, config.qdrant_port, None).await?;
            let vs = VectorStore::new(&sc.url(), embedder.dimension())?;
            (vs, Some(sc))
        };

        let metadata_store = Arc::new(SqliteStore::open_with_pool_size(
            &config.db_path,
            config.reader_pool_size,
        )?);

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

        tracing::info!(
            startup_phase = "sqlite",
            startup_status = "ready",
            "metadata store ready"
        );
        vector_store.ensure_collection(&config.namespace).await?;

        let default_normalizer =
            Arc::new(PredicateNormalizer::new(config.predicates.groups.clone()));
        let mut normalizers = HashMap::new();
        for ns_name in config.predicates.namespaces.keys() {
            let groups = resolve_namespace_groups(&config.predicates, ns_name);
            normalizers.insert(ns_name.clone(), Arc::new(PredicateNormalizer::new(groups)));
        }

        let cache = cache::RecallCache::new(&config.cache);
        let access_tracker = Arc::new(AccessTracker::new());
        let shutdown_token = CancellationToken::new();

        let flush_handle = {
            let tracker = Arc::clone(&access_tracker);
            let store = Arc::clone(&metadata_store);
            let token = shutdown_token.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = token.cancelled() => break,
                        () = tokio::time::sleep(ACCESS_FLUSH_INTERVAL) => {
                            let ids = tracker.drain();
                            if !ids.is_empty()
                                && let Err(e) = store.update_last_accessed(&ids).await {
                                tracing::warn!("background access flush failed: {e}");
                            }
                        }
                    }
                }
            })
        };

        let service = Self {
            embedder,
            reranker,
            metadata_store,
            vector_store,
            sidecar,
            config,
            normalizers,
            default_normalizer,
            access_tracker,
            shutdown_token,
            flush_handle: std::sync::Mutex::new(Some(flush_handle)),
            cache,
            ops_buffer: OpsBuffer::new(),
        };

        let report = service.recover_on_startup().await?;
        let pending_ops =
            report.compensated_stores + report.rolled_forward_forgets + report.cleared_pre_qdrant;
        if pending_ops > 0 {
            tracing::info!(
                compensated_stores = report.compensated_stores,
                rolled_forward_forgets = report.rolled_forward_forgets,
                cleared_pre_qdrant = report.cleared_pre_qdrant,
                "recovered pending ops on startup"
            );
        }
        tracing::info!(
            startup_phase = "recovery",
            startup_status = "complete",
            pending_ops,
            "startup recovery complete"
        );

        let pruned = service
            .metadata_store
            .prune_completed_ops(1000, chrono::Duration::days(7))
            .await?;
        if pruned > 0 {
            tracing::info!(pruned, "pruned old completed_ops entries");
        }

        Ok(service)
    }

    async fn recover_on_startup(&self) -> Result<RecoveryReport, CoreError> {
        use ferrex_store::PendingOpKind;

        let pending = self.metadata_store.list_pending_ops().await?;
        let mut report = RecoveryReport::default();
        for op in pending {
            match (op.kind, op.qdrant_written) {
                (PendingOpKind::Store | PendingOpKind::Supersede, false) => {
                    // Crash before Qdrant write: nothing to compensate.
                    self.metadata_store.clear_pending_op(&op.op_id).await?;
                    report.cleared_pre_qdrant += 1;
                }
                (PendingOpKind::Store | PendingOpKind::Supersede, true) => {
                    // Crash after Qdrant write but before SQLite commit:
                    // the caller never received a success ACK, so roll back
                    // the Qdrant point. The Memory body is not in the journal,
                    // so we cannot roll forward.
                    let Ok(uuid) = uuid::Uuid::parse_str(&op.memory_id) else {
                        tracing::warn!(op_id = %op.op_id, "invalid memory_id uuid in journal");
                        self.metadata_store.clear_pending_op(&op.op_id).await?;
                        continue;
                    };
                    self.vector_store
                        .delete_by_ids(&op.namespace, &[uuid])
                        .await?;
                    self.metadata_store
                        .delete_memories(std::slice::from_ref(&op.memory_id))
                        .await?;
                    self.metadata_store.clear_pending_op(&op.op_id).await?;
                    report.compensated_stores += 1;
                }
                (PendingOpKind::Forget, _) => {
                    // Forget is idempotent on both stores (delete-by-id is a
                    // no-op when missing), so always roll forward regardless
                    // of `qdrant_written`. This closes the crash window
                    // between the Qdrant delete and the journal flag update.
                    let Ok(uuid) = uuid::Uuid::parse_str(&op.memory_id) else {
                        tracing::warn!(op_id = %op.op_id, "invalid memory_id uuid in journal");
                        self.metadata_store.clear_pending_op(&op.op_id).await?;
                        continue;
                    };
                    self.vector_store
                        .delete_by_ids(&op.namespace, &[uuid])
                        .await?;
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

    pub async fn shutdown(&self) {
        self.shutdown_token.cancel();
        let handle = self.flush_handle.lock().expect("poisoned").take();
        if let Some(h) = handle {
            let _ = h.await;
        }
        let ids = self.access_tracker.drain();
        if !ids.is_empty()
            && let Err(e) = self.metadata_store.update_last_accessed(&ids).await
        {
            tracing::warn!("shutdown access flush failed: {e}");
        }
    }

    pub fn embedder(&self) -> &Embedder {
        &self.embedder
    }

    pub fn metadata_store(&self) -> &SqliteStore {
        &self.metadata_store
    }

    pub fn vector_store(&self) -> &VectorStore {
        &self.vector_store
    }

    pub fn reranker(&self) -> &Reranker {
        &self.reranker
    }

    pub const fn into_parts(mut self) -> (Self, Option<QdrantSidecar>) {
        let sidecar = self.sidecar.take();
        (self, sidecar)
    }
}
