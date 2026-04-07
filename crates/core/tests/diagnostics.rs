mod common;

use ferrex_core::{RecallRequest, StatsRequest};

#[tokio::test]
async fn diagnostics_returns_nonzero_after_operations() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(common::episodic("deployed api-server v2.3 to production"))
        .await
        .unwrap();
    svc.store(common::episodic("ran database migration for user table schema"))
        .await
        .unwrap();

    svc.recall(RecallRequest {
        query: "diagnostics".into(),
        types: None,
        entities: None,
        namespace: None,
        limit: Some(5),
        include_stale: None,
        include_invalidated: None,
        time_range: None,
        validate_ids: None,
        explain: false,
    })
    .await
    .unwrap();

    let report = svc.diagnostics().await.unwrap();
    assert!(report.memory_count >= 2, "should have at least 2 memories");
    assert!(!report.version.is_empty(), "version should be set");
    assert!(
        report.cache.embedding_hits + report.cache.embedding_misses > 0,
        "cache should have been accessed"
    );
}

#[tokio::test]
async fn stats_diagnostics_flag_returns_report() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(common::episodic("Stats diagnostics test"))
        .await
        .unwrap();

    let stats = svc
        .stats(StatsRequest {
            namespace: ctx.namespace.clone(),
            detailed: Some(false),
            diagnostics: Some(true),
        })
        .await
        .unwrap();

    assert!(stats.diagnostics.is_some(), "diagnostics should be present");
    let diag = stats.diagnostics.unwrap();
    assert!(diag.memory_count >= 1);
}
