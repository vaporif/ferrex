use std::collections::HashMap;

use ferrex_core::{
    CoreError, ForgetRequest, PredicatesConfig, RecallRequest, StatsRequest, StoreRequest,
};
use ferrex_store::MemoryType;

mod common;
use common::{episodic, recall_query, semantic};

#[tokio::test]
async fn dedup_rejects_near_duplicate_episodic() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
async fn conflict_update_invalidates_old_fact() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    // Objects must be very different (jaro_winkler < object_fuzzy_update=0.50)
    // for auto-supersede rather than ConflictAmbiguous.
    let first = svc
        .store(semantic("api", "uses", "Python 3.9"))
        .await
        .unwrap();
    let second = svc
        .store(semantic("api", "uses", "Rust nightly"))
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
            explain: false,
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
async fn conflict_ambiguous_surfaces_error() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
async fn supersedes_skips_dedup_and_conflict() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
            explain: false,
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
async fn forget_removes_from_both_stores() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
            explain: false,
        })
        .await
        .unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.memory.id.as_str()).collect();
    assert!(
        !ids.contains(&resp.id.as_str()),
        "forgotten memory should not be recalled"
    );
}

#[tokio::test]
async fn forget_nonexistent_returns_not_found() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
async fn validation_rejects_episodic_without_content() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
async fn validation_rejects_semantic_missing_object() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
async fn store_and_recall_episodic_round_trip() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
async fn stats_reflects_stored_count() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    let before = svc
        .stats(StatsRequest {
            namespace: ctx.namespace.clone(),
            detailed: None,
            diagnostics: None,
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
            namespace: ctx.namespace.clone(),
            detailed: None,
            diagnostics: None,
        })
        .await
        .unwrap();
    assert_eq!(after.total_memories, 2);
}

#[tokio::test]
async fn dedup_allows_distinct_episodic_memories() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    svc.store(episodic("the build failed because of a flaky test"))
        .await
        .unwrap();
    svc.store(episodic("deployed the new auth service to production"))
        .await
        .unwrap();
}

#[tokio::test]
async fn predicate_normalization_triggers_conflict() {
    let mut groups = HashMap::new();
    groups.insert("depends_on".into(), vec!["uses".into(), "requires".into()]);
    let ctx = common::TestContext::with_config(|mut c| {
        c.predicates = PredicatesConfig {
            groups,
            namespaces: HashMap::new(),
        };
        c
    })
    .await;
    let svc = &ctx.service;

    let first = svc
        .store(semantic("api", "uses", "Python 3.9"))
        .await
        .unwrap();
    // "requires" normalizes to "depends_on" (same as "uses"), and the object
    // is very different (jaro_winkler < 0.50) to auto-supersede.
    let second = svc
        .store(semantic("api", "requires", "Rust nightly"))
        .await
        .unwrap();
    assert!(
        second.superseded.contains(&first.id),
        "synonym predicates should trigger conflict: {:?}",
        second.superseded
    );
}

#[tokio::test]
async fn supersedes_nonexistent_target_fails() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    let fake_id = uuid::Uuid::now_v7().to_string();
    let mut req = semantic("api", "uses", "tokio 1.40");
    req.supersedes = Some(fake_id);
    assert!(matches!(
        svc.store(req).await,
        Err(CoreError::Validation(_))
    ));
}

#[tokio::test]
async fn recall_time_range_filters_by_date() {
    use chrono::Utc;
    use ferrex_core::TimeRange;

    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(episodic("old event from the distant past"))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let boundary = Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    svc.store(episodic("recent event just happened"))
        .await
        .unwrap();

    let req = RecallRequest {
        query: "event".into(),
        time_range: Some(TimeRange {
            start: Some(boundary),
            end: None,
        }),
        ..recall_query("event")
    };
    let results = svc.recall(req).await.unwrap();
    assert!(
        results.iter().all(|r| r.memory.created_at >= boundary),
        "expected only memories after boundary, got: {:?}",
        results
            .iter()
            .map(|r| &r.memory.created_at)
            .collect::<Vec<_>>()
    );
    assert!(
        !results.is_empty(),
        "should return at least the recent event"
    );
}

#[tokio::test]
async fn recall_include_invalidated_returns_superseded() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    let original = svc
        .store(semantic("Rust", "version", "1.70"))
        .await
        .unwrap();

    let mut update = semantic("Rust", "version", "1.80");
    update.supersedes = Some(original.id.clone());
    svc.store(update).await.unwrap();

    let results = svc.recall(recall_query("Rust version")).await.unwrap();
    assert!(
        results.iter().all(|r| r.memory.id != original.id),
        "invalidated memory should be excluded by default"
    );

    let req = RecallRequest {
        include_invalidated: Some(true),
        ..recall_query("Rust version")
    };
    let results = svc.recall(req).await.unwrap();
    let has_original = results.iter().any(|r| r.memory.id == original.id);
    assert!(
        has_original,
        "invalidated memory should be included when include_invalidated=true"
    );
}

#[tokio::test]
async fn recall_include_stale_false_excludes_stale() {
    use ferrex_core::FreshnessLabel;

    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(episodic("something that happened today"))
        .await
        .unwrap();

    let results = svc
        .recall(recall_query("something that happened"))
        .await
        .unwrap();
    assert!(!results.is_empty());

    let req = RecallRequest {
        include_stale: Some(false),
        ..recall_query("something that happened")
    };
    let results = svc.recall(req).await.unwrap();
    for r in &results {
        assert_ne!(
            r.freshness_label,
            FreshnessLabel::Stale,
            "stale memories should be excluded when include_stale=false"
        );
    }
}

#[tokio::test]
async fn recall_explain_includes_scoring_breakdown() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    svc.store(episodic("The server crashed at midnight due to OOM"))
        .await
        .unwrap();

    let results = svc
        .recall(RecallRequest {
            query: "server crash".into(),
            types: None,
            entities: None,
            namespace: None,
            limit: Some(5),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
            validate_ids: None,
            explain: true,
        })
        .await
        .unwrap();

    assert!(!results.is_empty(), "should find at least one result");
    let first = &results[0];
    let scoring = first
        .scoring
        .as_ref()
        .expect("scoring should be Some when explain=true");
    assert!(
        scoring.rerank_score > 0.0,
        "rerank_score should be positive"
    );
    assert!(
        scoring.recency_boost >= 1.0,
        "recency_boost should be >= 1.0"
    );
    assert!(scoring.staleness >= 0.0, "staleness should be non-negative");
    assert_eq!(
        scoring.final_rank, 1,
        "first result should have final_rank=1"
    );
}

#[tokio::test]
async fn recall_no_explain_omits_scoring() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    svc.store(episodic("Testing without explain flag"))
        .await
        .unwrap();

    let results = svc.recall(recall_query("testing")).await.unwrap();
    assert!(!results.is_empty());
    assert!(
        results[0].scoring.is_none(),
        "scoring should be None when explain=false"
    );
}
