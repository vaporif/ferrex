// Phase 3 integration tests. All require a running Qdrant (`#[ignore]`).
// Run manually: `cargo test -p ferrex-core --test phase3_integration -- --ignored`
#![allow(clippy::needless_collect)]

use std::path::PathBuf;

use ferrex_core::{
    CoreError, DedupConfig, FerrexConfig, ForgetRequest, MemoryService, ModelTier, RecallRequest,
    RerankerTier, StoreRequest,
};
use ferrex_store::MemoryType;

async fn test_service() -> MemoryService {
    let config = FerrexConfig {
        qdrant_url: Some("http://localhost:6334".into()),
        qdrant_bin: "qdrant".into(),
        qdrant_port: 6334,
        model_tier: ModelTier::Small,
        reranker_tier: RerankerTier::Default,
        namespace: format!("test_phase3_{}", uuid::Uuid::now_v7()),
        db_path: PathBuf::from(format!(
            "file:memdb_p3_{}?mode=memory&cache=shared",
            uuid::Uuid::now_v7()
        )),
        config_path: None,
        deduplication: DedupConfig { threshold: 0.90 },
        conflict: ferrex_core::ConflictConfig::default(),
        predicates: ferrex_core::PredicatesConfig::default(),
        reconciliation: ferrex_core::ReconciliationConfig::default(),
        reader_pool_size: 2,
    };
    MemoryService::from_config(config).await.unwrap()
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

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn dedup_rejects_near_duplicate_episodic() {
    let svc = test_service().await;
    svc.store(episodic("The deployment happened at 3pm"))
        .await
        .unwrap();
    let result = svc.store(episodic("The deployment happened at 3 PM")).await;
    assert!(
        matches!(result, Err(CoreError::Duplicate { .. })),
        "expected Duplicate, got {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn conflict_update_invalidates_old_fact() {
    let svc = test_service().await;
    let first = svc
        .store(semantic("api", "uses", "tokio 1.38"))
        .await
        .unwrap();
    let second = svc
        .store(semantic("api", "uses", "tokio 1.40"))
        .await
        .unwrap();
    assert!(
        second.superseded.contains(&first.id),
        "second should supersede first: {:?}",
        second.superseded
    );
    let results = svc
        .recall(RecallRequest {
            query: "api uses".into(),
            types: None,
            entities: None,
            namespace: None,
            limit: Some(10),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|(m, _)| m.id.as_str()).collect();
    assert!(
        ids.contains(&second.id.as_str()),
        "recall should return new fact"
    );
    assert!(
        !ids.contains(&first.id.as_str()),
        "recall should not return old fact"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn conflict_ambiguous_surfaces_error() {
    let svc = test_service().await;
    svc.store(semantic("api", "uses", "tokio 1.38"))
        .await
        .unwrap();
    let result = svc
        .store(semantic("api", "uses", "tokio 1.38 with patches"))
        .await;
    assert!(
        matches!(result, Err(CoreError::ConflictAmbiguous { .. })),
        "expected ConflictAmbiguous, got {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn supersedes_skips_dedup_and_conflict() {
    let svc = test_service().await;
    let first = svc
        .store(semantic("api", "uses", "tokio 1.38"))
        .await
        .unwrap();

    let mut req = semantic("api", "uses", "tokio 1.38");
    req.supersedes = Some(first.id.clone());
    let second = svc.store(req).await.unwrap();

    assert!(
        second.superseded.contains(&first.id),
        "supersedes should list the target: {:?}",
        second.superseded
    );

    let results = svc
        .recall(RecallRequest {
            query: "api uses tokio".into(),
            types: None,
            entities: None,
            namespace: None,
            limit: Some(10),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|(m, _)| m.id.as_str()).collect();
    assert!(
        !ids.contains(&first.id.as_str()),
        "old fact should not appear"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn forget_removes_from_both_stores() {
    let svc = test_service().await;
    let resp = svc.store(episodic("to be forgotten")).await.unwrap();
    let forget_resp = svc
        .forget(ForgetRequest {
            ids: vec![resp.id.clone()],
            cascade: None,
        })
        .await
        .unwrap();
    assert!(forget_resp.deleted.contains(&resp.id));
    assert!(forget_resp.not_found.is_empty());

    let results = svc
        .recall(RecallRequest {
            query: "to be forgotten".into(),
            types: None,
            entities: None,
            namespace: None,
            limit: Some(10),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|(m, _)| m.id.as_str()).collect();
    assert!(
        !ids.contains(&resp.id.as_str()),
        "forgotten memory should not be recalled"
    );
}
