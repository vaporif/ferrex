#![allow(clippy::needless_collect)]

use std::collections::HashMap;
use std::path::PathBuf;

use ferrex_core::{
    CoreError, DedupConfig, FerrexConfig, ForgetRequest, MemoryService, ModelTier,
    PredicatesConfig, RecallRequest, RerankerTier, StalenessConfig, StatsRequest, StoreRequest,
};
use ferrex_store::MemoryType;

fn base_config() -> FerrexConfig {
    FerrexConfig {
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
        staleness: StalenessConfig::default(),
        reader_pool_size: 2,
    }
}

async fn test_service() -> MemoryService {
    MemoryService::from_config(base_config()).await.unwrap()
}

async fn test_service_with_predicates(groups: HashMap<String, Vec<String>>) -> MemoryService {
    let mut config = base_config();
    config.predicates = PredicatesConfig {
        groups,
        namespaces: HashMap::new(),
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
            validate_ids: None,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.memory.id.as_str()).collect();
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
            validate_ids: None,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.memory.id.as_str()).collect();
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
            validate_ids: None,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.memory.id.as_str()).collect();
    assert!(
        !ids.contains(&resp.id.as_str()),
        "forgotten memory should not be recalled"
    );
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
async fn forget_nonexistent_returns_not_found() {
    let svc = test_service().await;
    let fake_id = uuid::Uuid::now_v7().to_string();
    let resp = svc
        .forget(ForgetRequest {
            ids: vec![fake_id.clone()],
            cascade: None,
        })
        .await
        .unwrap();
    assert!(resp.deleted.is_empty());
    assert!(resp.not_found.contains(&fake_id));
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn validation_rejects_episodic_without_content() {
    let svc = test_service().await;
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
    assert!(matches!(
        svc.store(req).await,
        Err(CoreError::Validation(_))
    ));
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn validation_rejects_semantic_missing_object() {
    let svc = test_service().await;
    let req = StoreRequest {
        content: None,
        memory_type: Some(MemoryType::Semantic),
        subject: Some("api".into()),
        predicate: Some("uses".into()),
        object: None,
        confidence: None,
        source: None,
        context: None,
        entities: vec![],
        namespace: None,
        supersedes: None,
    };
    assert!(matches!(
        svc.store(req).await,
        Err(CoreError::Validation(_))
    ));
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn store_and_recall_episodic_round_trip() {
    let svc = test_service().await;
    let resp = svc
        .store(episodic("deployed v2.3.1 to staging at 3pm"))
        .await
        .unwrap();
    let results = svc
        .recall(recall_query("deployment staging"))
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.memory.id.as_str()).collect();
    assert!(
        ids.contains(&resp.id.as_str()),
        "stored memory should be recallable"
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn stats_reflects_stored_count() {
    let svc = test_service().await;
    let config = base_config();
    let before = svc
        .stats(StatsRequest {
            namespace: config.namespace.clone(),
            detailed: None,
        })
        .await
        .unwrap();
    assert_eq!(before.total_memories, 0);

    svc.store(episodic("first memory")).await.unwrap();
    svc.store(episodic("a completely different second memory"))
        .await
        .unwrap();

    let after = svc
        .stats(StatsRequest {
            namespace: config.namespace,
            detailed: None,
        })
        .await
        .unwrap();
    assert_eq!(after.total_memories, 2);
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn dedup_allows_distinct_episodic_memories() {
    let svc = test_service().await;
    svc.store(episodic("the build failed because of a flaky test"))
        .await
        .unwrap();
    svc.store(episodic("deployed the new auth service to production"))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn predicate_normalization_triggers_conflict() {
    let mut groups = HashMap::new();
    groups.insert("depends_on".into(), vec!["uses".into(), "requires".into()]);
    let svc = test_service_with_predicates(groups).await;

    let first = svc
        .store(semantic("api", "uses", "tokio 1.38"))
        .await
        .unwrap();
    // "requires" normalizes to "depends_on" (same as "uses"), so this is a conflict update
    let second = svc
        .store(semantic("api", "requires", "tokio 1.40"))
        .await
        .unwrap();
    assert!(
        second.superseded.contains(&first.id),
        "synonym predicates should trigger conflict: {:?}",
        second.superseded
    );
}

#[tokio::test]
#[ignore = "requires Qdrant"]
async fn supersedes_nonexistent_target_fails() {
    let svc = test_service().await;
    let fake_id = uuid::Uuid::now_v7().to_string();
    let mut req = semantic("api", "uses", "tokio 1.40");
    req.supersedes = Some(fake_id);
    assert!(matches!(
        svc.store(req).await,
        Err(CoreError::Validation(_))
    ));
}
