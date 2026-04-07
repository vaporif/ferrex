use ferrex_core::{FreshnessLabel, RecallRequest, ReflectRequest, StatsRequest, StoreRequest};
use ferrex_store::MemoryType;

mod common;
use common::{episodic, recall_query, semantic};

#[tokio::test]
async fn test_recall_returns_freshness_metadata() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
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
    assert_eq!(r.freshness_label, FreshnessLabel::Fresh);
}

#[tokio::test]
async fn test_validate_ids_updates_last_validated() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    let resp = svc
        .store(episodic("the CI pipeline runs nightly at 2am UTC"))
        .await
        .unwrap();

    let mut req = recall_query("CI pipeline nightly");
    req.validate_ids = Some(vec![resp.id.clone()]);
    let results = svc.recall(req).await.unwrap();
    assert!(!results.is_empty(), "should recall the memory");

    let results2 = svc
        .recall(recall_query("CI pipeline nightly"))
        .await
        .unwrap();
    let r = results2.iter().find(|r| r.memory.id == resp.id).unwrap();
    assert!(
        r.staleness_score < 0.5,
        "validated memory should have low staleness"
    );
}

#[tokio::test]
async fn test_reflect_returns_empty_for_fresh_memories() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(episodic("just stored this fresh memory"))
        .await
        .unwrap();

    let resp = svc
        .reflect(ReflectRequest {
            namespace: ctx.namespace.clone(),
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
async fn test_reflect_contradiction_exact_predicate() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    let first = svc
        .store(semantic("database", "version", "postgres 15"))
        .await
        .unwrap();
    // Explicitly supersede the first to avoid ConflictAmbiguous.
    let mut req = semantic("database", "version", "postgres 16");
    req.supersedes = Some(first.id);
    svc.store(req).await.unwrap();

    let resp = svc
        .reflect(ReflectRequest {
            namespace: ctx.namespace.clone(),
            limit: None,
            include_contradictions: true,
            include_stale: false,
        })
        .await
        .unwrap();

    assert_eq!(resp.summary.stale_count, 0);
}

#[tokio::test]
async fn test_stats_brief_has_real_needs_attention() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(episodic("first event happened")).await.unwrap();
    svc.store(episodic("a completely different second event"))
        .await
        .unwrap();

    let resp = svc
        .stats(StatsRequest {
            namespace: ctx.namespace.clone(),
            detailed: Some(false),
        })
        .await
        .unwrap();

    assert_eq!(resp.total_memories, 2);
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
async fn test_stats_detailed_mode() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(episodic("deployed the API to production"))
        .await
        .unwrap();
    svc.store(semantic("api-server", "runs-on", "kubernetes"))
        .await
        .unwrap();

    let resp = svc
        .stats(StatsRequest {
            namespace: ctx.namespace.clone(),
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
    assert!(
        details.by_type.contains_key("episodic") || details.by_type.contains_key("semantic"),
        "by_type should have entries"
    );
}
