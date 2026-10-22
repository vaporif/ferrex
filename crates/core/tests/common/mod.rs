#![allow(dead_code)]

use std::path::PathBuf;

use ferrex_core::{
    CacheConfig, DedupConfig, FerrexConfig, MemoryService, ModelTier, RecallRequest, RerankerTier,
    StalenessConfig, StoreRequest,
};
use ferrex_store::{MemoryType, QdrantSidecar, SidecarPolicy};

pub struct TestContext {
    pub service: MemoryService,
    pub namespace: String,
    pub qdrant_url: String,
    pub config: FerrexConfig,
    _sidecar: QdrantSidecar,
    _temp_dir: tempfile::TempDir,
}

impl TestContext {
    pub async fn new() -> Self {
        Self::with_config(|c| c).await
    }

    pub async fn with_config(f: impl FnOnce(FerrexConfig) -> FerrexConfig) -> Self {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        Self::in_dir(temp_dir, f).await
    }

    /// Like [`Self::with_config`], with a file-backed SQLite DB that a second service can reopen.
    pub async fn with_file_db() -> Self {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let db_path = temp_dir.path().join("ferrex.db");
        Self::in_dir(temp_dir, |mut c| {
            c.db_path = db_path;
            c
        })
        .await
    }

    /// A second service over the same DB and Qdrant; runs startup recovery.
    pub async fn reopen(&self) -> MemoryService {
        MemoryService::from_config(self.config.clone())
            .await
            .expect("failed to reopen MemoryService")
    }

    async fn in_dir(
        temp_dir: tempfile::TempDir,
        f: impl FnOnce(FerrexConfig) -> FerrexConfig,
    ) -> Self {
        let base_dir = temp_dir.path().to_path_buf();

        let port = ferrex_store::testing::free_port();

        let sidecar =
            QdrantSidecar::start("qdrant", port, Some(base_dir), SidecarPolicy::KillOnDrop)
                .await
                .expect("failed to start test sidecar");

        let namespace = format!("test_{}", uuid::Uuid::now_v7());
        let db_path = PathBuf::from(format!(
            "file:memdb_{}?mode=memory&cache=shared",
            uuid::Uuid::now_v7()
        ));

        let config = FerrexConfig {
            qdrant_url: Some(sidecar.url()),
            qdrant_bin: "qdrant".into(),
            qdrant_port: port,
            model_tier: ModelTier::Small,
            reranker_tier: RerankerTier::Default,
            namespace: namespace.clone(),
            db_path,
            config_path: None,
            deduplication: DedupConfig { threshold: 0.90 },
            conflict: ferrex_core::ConflictConfig::default(),
            predicates: ferrex_core::PredicatesConfig::default(),
            reconciliation: ferrex_core::ReconciliationConfig::default(),
            staleness: StalenessConfig::default(),
            reader_pool_size: 2,
            cache: CacheConfig::default(),
        };

        let config = f(config);
        let service = MemoryService::from_config(config.clone())
            .await
            .expect("failed to create test MemoryService");

        Self {
            service,
            namespace,
            qdrant_url: sidecar.url(),
            config,
            _sidecar: sidecar,
            _temp_dir: temp_dir,
        }
    }
}

pub fn episodic(content: &str) -> StoreRequest {
    StoreRequest {
        content: Some(content.into()),
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
    }
}

pub fn semantic(subject: &str, predicate: &str, object: &str) -> StoreRequest {
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
        namespace: None,
        supersedes: None,
    }
}

pub fn recall_query(query: &str) -> RecallRequest {
    RecallRequest {
        query: query.into(),
        types: None,
        entities: None,
        namespace: None,
        limit: Some(10),
        include_stale: None,
        include_invalidated: None,
        time_range: None,
        as_of: None,
        validate_ids: None,
        explain: false,
    }
}
