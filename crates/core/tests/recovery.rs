//! Crash-recovery integration tests for the dual-write journal.
//!
//! These exercise `MemoryService::recover_on_startup` (invoked automatically
//! by `from_config`) by injecting a stuck `pending_ops` row that simulates a
//! crash mid-operation, then re-opening the service against the same
//! filesystem-backed SQLite DB and Qdrant collection.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use ferrex_core::{
    CacheConfig, DedupConfig, FerrexConfig, MemoryService, ModelTier, RerankerTier,
    StalenessConfig, StoreRequest,
};
use ferrex_embed::init_embed_env;
use ferrex_store::{MemoryType, MetadataStore, PendingOp, PendingOpKind, QdrantSidecar};
use uuid::Uuid;

mod common;

struct PersistentRecoveryEnv {
    qdrant_url: String,
    namespace: String,
    db_path: PathBuf,
    _sidecar: Arc<QdrantSidecar>,
    _temp_dir: tempfile::TempDir,
}

impl PersistentRecoveryEnv {
    async fn new() -> Self {
        init_embed_env();
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let base_dir = temp_dir.path().to_path_buf();

        let port = portpicker::pick_unused_port().expect("no free port");
        let sidecar = Arc::new(
            QdrantSidecar::start("qdrant", port, Some(base_dir.clone()))
                .await
                .expect("failed to start sidecar"),
        );

        Self {
            qdrant_url: sidecar.url(),
            namespace: format!("test_{}", Uuid::now_v7()),
            db_path: base_dir.join("ferrex.sqlite"),
            _sidecar: sidecar,
            _temp_dir: temp_dir,
        }
    }

    fn config(&self) -> FerrexConfig {
        FerrexConfig {
            qdrant_url: Some(self.qdrant_url.clone()),
            qdrant_bin: "qdrant".into(),
            qdrant_port: 0,
            model_tier: ModelTier::Small,
            reranker_tier: RerankerTier::Default,
            namespace: self.namespace.clone(),
            db_path: self.db_path.clone(),
            config_path: None,
            deduplication: DedupConfig { threshold: 0.90 },
            conflict: ferrex_core::ConflictConfig::default(),
            predicates: ferrex_core::PredicatesConfig::default(),
            reconciliation: ferrex_core::ReconciliationConfig::default(),
            staleness: StalenessConfig::default(),
            reader_pool_size: 2,
            cache: CacheConfig::default(),
        }
    }
}

fn semantic_req(namespace: &str, subject: &str, predicate: &str, object: &str) -> StoreRequest {
    StoreRequest {
        content: None,
        memory_type: Some(MemoryType::Semantic),
        subject: Some(subject.into()),
        predicate: Some(predicate.into()),
        object: Some(object.into()),
        confidence: None,
        source: None,
        context: None,
        entities: vec![],
        namespace: Some(namespace.into()),
        supersedes: None,
    }
}

#[tokio::test]
async fn forget_crash_window_rolls_forward_on_recovery() {
    let env = PersistentRecoveryEnv::new().await;

    // Boot service A, store one memory, then simulate the crash window in
    // `forget`: Qdrant point already deleted, but the journal flag never got
    // flipped to `qdrant_written = true` before the crash.
    let svc_a = MemoryService::from_config(env.config())
        .await
        .expect("failed to start service A");

    let stored = svc_a
        .store(semantic_req(
            &env.namespace,
            "ferrex",
            "uses",
            "qdrant for vectors",
        ))
        .await
        .expect("store failed");
    let memory_id = stored.id.clone();
    let memory_uuid = Uuid::parse_str(&memory_id).expect("stored id should be a uuid");

    let pending = PendingOp {
        op_id: Uuid::now_v7().to_string(),
        kind: PendingOpKind::Forget,
        memory_id: memory_id.clone(),
        namespace: env.namespace.clone(),
        target_id: None,
        qdrant_written: false,
        started_at: Utc::now(),
    };
    svc_a
        .metadata_store()
        .insert_pending_op(&pending)
        .await
        .expect("failed to insert simulated pending op");
    svc_a
        .vector_store()
        .delete_by_ids(&env.namespace, &[memory_uuid])
        .await
        .expect("failed to delete qdrant point in simulated crash window");

    // SQLite still has the row; Qdrant point is gone; journal flag is `false`.
    // Pre-fix recovery would just clear the journal and leave the SQLite row
    // as a phantom. Post-fix recovery rolls forward on Forget regardless of
    // the flag because both deletes are idempotent.
    assert!(
        svc_a
            .metadata_store()
            .get_memory(&memory_id)
            .await
            .unwrap()
            .is_some(),
        "phantom row should still exist before recovery runs"
    );

    svc_a.shutdown().await;
    drop(svc_a);

    // Boot service B against the same DB + Qdrant. `recover_on_startup` runs
    // inside `from_config` and should compensate the stuck op.
    let svc_b = MemoryService::from_config(env.config())
        .await
        .expect("failed to start service B");

    assert!(
        svc_b
            .metadata_store()
            .get_memory(&memory_id)
            .await
            .unwrap()
            .is_none(),
        "recovery should have rolled the Forget forward and removed the SQLite row"
    );
    assert!(
        svc_b
            .metadata_store()
            .list_pending_ops()
            .await
            .unwrap()
            .is_empty(),
        "recovery should have cleared the pending op"
    );

    svc_b.shutdown().await;
}

#[tokio::test]
async fn store_crash_after_qdrant_write_rolls_back_on_recovery() {
    // Sanity check: the existing rollback semantics for (Store, qdrant_written=true)
    // still work — the user never received an ACK so we drop the orphan Qdrant point.
    let env = PersistentRecoveryEnv::new().await;
    let svc_a = MemoryService::from_config(env.config())
        .await
        .expect("failed to start service A");

    // Manually insert a `pending_ops` row claiming a Store op completed in
    // Qdrant but never reached SQLite. We don't actually upsert the Qdrant
    // point — recovery should issue an idempotent delete regardless, which
    // is a no-op when the point is missing.
    let orphan_id = Uuid::now_v7().to_string();
    let pending = PendingOp {
        op_id: Uuid::now_v7().to_string(),
        kind: PendingOpKind::Store,
        memory_id: orphan_id.clone(),
        namespace: env.namespace.clone(),
        target_id: None,
        qdrant_written: true,
        started_at: Utc::now(),
    };
    svc_a
        .metadata_store()
        .insert_pending_op(&pending)
        .await
        .expect("failed to insert simulated pending op");
    svc_a.shutdown().await;
    drop(svc_a);

    let svc_b = MemoryService::from_config(env.config())
        .await
        .expect("failed to start service B");

    assert!(
        svc_b
            .metadata_store()
            .list_pending_ops()
            .await
            .unwrap()
            .is_empty(),
        "recovery should have cleared the orphan store op"
    );
    assert!(
        svc_b
            .metadata_store()
            .get_memory(&orphan_id)
            .await
            .unwrap()
            .is_none(),
        "orphan id should not exist in SQLite after recovery"
    );

    svc_b.shutdown().await;
}
