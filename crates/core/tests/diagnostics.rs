mod common;

use ferrex_core::{RecallRequest, StatsRequest};
use pretty_assertions::assert_eq;

#[tokio::test]
async fn diagnostics_returns_nonzero_after_operations() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;

    svc.store(common::episodic("deployed api-server v2.3 to production"))
        .await
        .unwrap();
    svc.store(common::episodic(
        "ran database migration for user table schema",
    ))
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
        as_of: None,
        validate_ids: None,
        explain: false,
    })
    .await
    .unwrap();

    let report = svc.diagnostics(&ctx.namespace).await.unwrap();
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

#[tokio::test]
async fn stats_diagnostics_follow_requested_namespace() {
    let ctx = common::TestContext::new().await;
    let svc = &ctx.service;
    let other = format!("{}_other", ctx.namespace);

    let mut in_a = common::episodic("alice reviewed the deploy plan");
    in_a.entities = vec!["alice".into()];
    svc.store(in_a).await.unwrap();

    let mut in_b = common::episodic("bob and carol paired on the parser");
    in_b.entities = vec!["bob".into(), "carol".into()];
    in_b.namespace = Some(other.clone());
    svc.store(in_b).await.unwrap();

    let stats = svc
        .stats(StatsRequest {
            namespace: other.clone(),
            detailed: Some(false),
            diagnostics: Some(true),
        })
        .await
        .unwrap();
    let diag = stats.diagnostics.expect("diagnostics requested");
    assert_eq!(diag.collection, other);
    assert_eq!(diag.entity_count, 2);
    assert_eq!(diag.memory_count, 2, "memory_count stays database-wide");
}
