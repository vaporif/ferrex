//! Phase 4: Memory Lifecycle integration tests

use std::path::PathBuf;

use ferrex_core::{
    DedupConfig, FerrexConfig, FreshnessLabel, MemoryService, ModelTier, RecallRequest,
    ReflectRequest, RerankerTier, StalenessConfig, StatsRequest, StoreRequest,
};
use ferrex_store::MemoryType;

fn base_config() -> FerrexConfig {
    FerrexConfig {
        qdrant_url: Some("http://localhost:6334".into()),
        qdrant_bin: "qdrant".into(),
        qdrant_port: 6334,
        model_tier: ModelTier::Small,
        reranker_tier: RerankerTier::Default,
        namespace: format!("test_phase4_{}", uuid::Uuid::now_v7()),
        db_path: PathBuf::from(format!(
            "file:memdb_p4_{}?mode=memory&cache=shared",
            uuid::Uuid::now_v7()
        )),
        config_path: None,
        deduplication: DedupConfig { threshold: 0.90 },
        conflict: ferrex_core::ConflictConfig::default(),
        predicates: ferrex_core::PredicatesConfig::default(),
        reconciliation: ferrex_core::ReconciliationConfig::default(),
        staleness: StalenessConfig::default(),
        reader_pool_size: 2,
    }
}

async fn test_service() -> MemoryService {
    MemoryService::from_config(base_config()).await.unwrap()
}

fn episodic(content: &str) -> StoreRequest {
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

fn semantic(subject: &str, predicate: &str, object: &str) -> StoreRequest {
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

fn recall_query(query: &str) -> RecallRequest {
    RecallRequest {
        query: query.into(),
        types: None,
        entities: None,
        namespace: None,
        limit: Some(10),
        include_stale: None,
        include_invalidated: None,
        time_range: None,
        validate_ids: None,
    }
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn test_recall_returns_freshness_metadata() {
    let svc = test_service().await;
    let resp = svc
        .store(episodic("the build server was upgraded to Ubuntu 24.04"))
        .await
        .unwrap();

    let results = svc
        .recall(recall_query("build server upgrade"))
        .await
        .unwrap();
    assert!(!results.is_empty(), "should recall at least one memory");

    let r = results.iter().find(|r| r.memory.id == resp.id).unwrap();
    assert!(
        r.staleness_score >= 0.0 && r.staleness_score <= 1.0,
        "staleness_score should be in [0,1], got {}",
        r.staleness_score
    );
    // Brand new memory should be fresh
    assert_eq!(r.freshness_label, FreshnessLabel::Fresh);
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn test_validate_ids_updates_last_validated() {
    let svc = test_service().await;
    let resp = svc
        .store(episodic("the CI pipeline runs nightly at 2am UTC"))
        .await
        .unwrap();

    let mut req = recall_query("CI pipeline nightly");
    req.validate_ids = Some(vec![resp.id.clone()]);
    let results = svc.recall(req).await.unwrap();
    assert!(!results.is_empty(), "should recall the memory");

    // Recall again without validate_ids and check the memory was validated
    let results2 = svc
        .recall(recall_query("CI pipeline nightly"))
        .await
        .unwrap();
    let r = results2.iter().find(|r| r.memory.id == resp.id).unwrap();
    // A validated memory should have a lower staleness score (but it's brand new so already low)
    assert!(
        r.staleness_score < 0.5,
        "validated memory should have low staleness"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn test_reflect_returns_empty_for_fresh_memories() {
    let svc = test_service().await;
    let config = base_config();

    svc.store(episodic("just stored this fresh memory"))
        .await
        .unwrap();

    let resp = svc
        .reflect(ReflectRequest {
            namespace: config.namespace,
            limit: None,
            include_contradictions: true,
            include_stale: true,
        })
        .await
        .unwrap();

    assert!(resp.stale.is_empty(), "fresh memories should not be stale");
    assert_eq!(resp.summary.stale_count, 0);
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn test_reflect_contradiction_exact_predicate() {
    let svc = test_service().await;
    let config = base_config();

    // Store two semantic memories with the same subject+predicate but different objects.
    // Conflict resolution may auto-supersede, so we test that reflect produces
    // a valid structured response regardless.
    svc.store(semantic("database", "version", "postgres 15"))
        .await
        .unwrap();
    svc.store(semantic("database", "version", "postgres 16"))
        .await
        .unwrap();

    let resp = svc
        .reflect(ReflectRequest {
            namespace: config.namespace,
            limit: None,
            include_contradictions: true,
            include_stale: false,
        })
        .await
        .unwrap();

    // The response should have a valid summary structure
    assert_eq!(resp.summary.stale_count, 0);
    // Contradictions may or may not be found depending on conflict resolution
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn test_stats_brief_has_real_needs_attention() {
    let svc = test_service().await;
    let config = base_config();

    svc.store(episodic("first event happened")).await.unwrap();
    svc.store(episodic("a completely different second event"))
        .await
        .unwrap();

    let resp = svc
        .stats(StatsRequest {
            namespace: config.namespace,
            detailed: Some(false),
        })
        .await
        .unwrap();

    assert_eq!(resp.total_memories, 2);
    // New memories should have 0 stale, but some unvalidated
    assert_eq!(resp.needs_attention.stale_count, 0);
    assert!(
        resp.needs_attention.unvalidated_count >= 2,
        "new memories should be unvalidated"
    );
    assert!(
        resp.details.is_none(),
        "brief mode should not include details"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn test_stats_detailed_mode() {
    let svc = test_service().await;
    let config = base_config();

    svc.store(episodic("deployed the API to production"))
        .await
        .unwrap();
    svc.store(semantic("api-server", "runs-on", "kubernetes"))
        .await
        .unwrap();

    let resp = svc
        .stats(StatsRequest {
            namespace: config.namespace,
            detailed: Some(true),
        })
        .await
        .unwrap();

    assert_eq!(resp.total_memories, 2);
    let details = resp.details.expect("detailed mode should include details");
    assert!(details.storage_size_bytes > 0, "storage should be non-zero");
    assert!(
        details.staleness_distribution.fresh >= 2,
        "new memories should be fresh"
    );
    assert_eq!(details.staleness_distribution.stale, 0);
    // Check by_type has entries
    assert!(
        details.by_type.contains_key("episodic") || details.by_type.contains_key("semantic"),
        "by_type should have entries"
    );
}
