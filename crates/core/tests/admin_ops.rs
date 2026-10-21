use assert_matches::assert_matches;
use chrono::{Duration, Utc};
use ferrex_core::{CoreError, ReflectRequest};
use ferrex_store::{Payload, PendingOpKind, VectorStore};
use pretty_assertions::assert_eq;

mod common;
use common::{TestContext, episodic, semantic};

fn sql(ctx: &TestContext) -> rusqlite::Connection {
    rusqlite::Connection::open(&ctx.config.db_path).unwrap()
}

fn age_memory(ctx: &TestContext, id: &str, days: i64) {
    let then = (Utc::now() - Duration::days(days)).to_rfc3339();
    sql(ctx)
        .execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1, last_accessed = ?1, \
             last_validated = NULL WHERE id = ?2",
            rusqlite::params![then, id],
        )
        .unwrap();
}

fn vectors(ctx: &TestContext) -> VectorStore {
    VectorStore::new(&ctx.qdrant_url, 384).unwrap()
}

#[tokio::test]
async fn audit_reports_and_fixes_qdrant_orphans() {
    let ctx = TestContext::new().await;
    ctx.service
        .store(episodic("the queue runs on rabbitmq"))
        .await
        .unwrap();

    let orphan = uuid::Uuid::now_v7();
    let payload = Payload::try_from(serde_json::json!({
        ferrex_store::QdrantField::POINT_TYPE: ferrex_store::PointType::MEMORY,
    }))
    .unwrap();
    vectors(&ctx)
        .upsert(&ctx.namespace, orphan, vec![0.1; 384], "orphan", payload)
        .await
        .unwrap();

    let report = ctx.service.audit_reconcile(false, 1000).await.unwrap();
    assert_eq!(report.qdrant_only, vec![orphan.to_string()]);
    assert!(report.sqlite_only.is_empty());
    assert!(report.fixed.is_empty());

    let err = ctx.service.audit_reconcile(true, 0).await.unwrap_err();
    assert_matches!(err, CoreError::Validation(ref m) if m.contains("limit 0"));

    let fixed = ctx.service.audit_reconcile(true, 1000).await.unwrap();
    assert_eq!(fixed.fixed, vec![orphan.to_string()]);
    assert!(fixed.qdrant_only.is_empty());

    let after = ctx.service.audit_reconcile(false, 1000).await.unwrap();
    assert!(after.qdrant_only.is_empty());
}

#[tokio::test]
async fn audit_reports_sqlite_rows_missing_their_point() {
    let ctx = TestContext::new().await;
    let stored = ctx
        .service
        .store(episodic("ci builds run on buildkite"))
        .await
        .unwrap();
    let id = uuid::Uuid::parse_str(&stored.id).unwrap();
    vectors(&ctx).delete(&ctx.namespace, id).await.unwrap();

    let report = ctx.service.audit_reconcile(false, 1000).await.unwrap();
    assert_eq!(report.sqlite_only, vec![stored.id]);
    assert!(report.qdrant_only.is_empty());
}

#[tokio::test]
async fn backfill_normalizes_missing_predicates() {
    let ctx = TestContext::with_file_db().await;
    let changed = ctx
        .service
        .store(semantic("api", "Written_By", "alice"))
        .await
        .unwrap();
    let canonical = ctx
        .service
        .store(semantic("api", "uses", "tokio"))
        .await
        .unwrap();
    sql(&ctx)
        .execute(
            "UPDATE memories SET normalized_predicate = NULL WHERE id IN (?1, ?2)",
            rusqlite::params![changed.id, canonical.id],
        )
        .unwrap();

    let dry = ctx
        .service
        .backfill_normalized_predicates(Some(&ctx.namespace), true)
        .await
        .unwrap();
    assert_eq!((dry.scanned, dry.updated), (2, 1));

    let real = ctx
        .service
        .backfill_normalized_predicates(None, false)
        .await
        .unwrap();
    assert_eq!(real.updated, 1);
    let normalized: String = sql(&ctx)
        .query_row(
            "SELECT normalized_predicate FROM memories WHERE id = ?1",
            [&changed.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(normalized, "written by");

    let again = ctx
        .service
        .backfill_normalized_predicates(None, false)
        .await
        .unwrap();
    assert_eq!(again.updated, 0);
}

#[tokio::test]
async fn journal_listing_and_prune() {
    let ctx = TestContext::new().await;
    let stored = ctx
        .service
        .store(episodic("the cdn is cloudfront"))
        .await
        .unwrap();

    assert!(ctx.service.list_pending_ops().await.unwrap().is_empty());
    let done = ctx
        .service
        .list_completed_ops(None, 10, Some(Utc::now() - Duration::hours(1)))
        .await
        .unwrap();
    let op = done.iter().find(|o| o.memory_id == stored.id).unwrap();
    assert_eq!(op.kind, PendingOpKind::Store);

    ctx.service.prune_journal().await.unwrap();
    // Fresh ops survive the 7-day prune.
    assert!(
        !ctx.service
            .list_completed_ops(None, 10, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reflect_ranks_stale_memories() {
    let ctx = TestContext::with_file_db().await;
    let never_read = ctx
        .service
        .store(episodic("the old billing service ran on php"))
        .await
        .unwrap();
    let fact = ctx
        .service
        .store(semantic("billing", "language", "php"))
        .await
        .unwrap();
    let fresh = ctx
        .service
        .store(episodic("the new billing service runs on rust"))
        .await
        .unwrap();
    age_memory(&ctx, &never_read.id, 400);
    age_memory(&ctx, &fact.id, 2000);

    let resp = ctx
        .service
        .reflect(ReflectRequest {
            namespace: ctx.namespace.clone(),
            limit: Some(10),
            include_contradictions: false,
            include_stale: true,
        })
        .await
        .unwrap();

    let ids: Vec<&str> = resp.stale.iter().map(|c| c.memory.id.as_str()).collect();
    assert_eq!(ids, vec![never_read.id.as_str(), fact.id.as_str()]);
    assert!(!ids.contains(&fresh.id.as_str()));
    assert_eq!(resp.stale[0].eviction_rank, 1);
    assert!(resp.stale[0].reason.contains("never validated"));
    assert_eq!(resp.summary.stale_count, 2);
    assert!(resp.contradictions.is_empty());

    let limited = ctx
        .service
        .reflect(ReflectRequest {
            namespace: ctx.namespace.clone(),
            limit: Some(1),
            include_contradictions: true,
            include_stale: true,
        })
        .await
        .unwrap();
    assert_eq!(limited.stale.len(), 1);
}

#[tokio::test]
async fn reflect_with_nothing_requested_is_empty() {
    let ctx = TestContext::new().await;
    ctx.service.store(episodic("anything")).await.unwrap();
    let resp = ctx
        .service
        .reflect(ReflectRequest {
            namespace: ctx.namespace.clone(),
            limit: None,
            include_contradictions: false,
            include_stale: false,
        })
        .await
        .unwrap();
    assert!(resp.stale.is_empty() && resp.contradictions.is_empty());
    assert_eq!(resp.summary.total_scanned, 0);
}

#[tokio::test]
async fn stats_counts_stale_memories_per_type() {
    let ctx = TestContext::with_file_db().await;
    let old = ctx
        .service
        .store(episodic("the legacy cron host was decommissioned"))
        .await
        .unwrap();
    ctx.service
        .store(episodic("the new scheduler runs on nomad"))
        .await
        .unwrap();
    age_memory(&ctx, &old.id, 400);

    let stats = ctx
        .service
        .stats(ferrex_core::StatsRequest {
            namespace: ctx.namespace.clone(),
            detailed: Some(true),
            diagnostics: None,
        })
        .await
        .unwrap();

    assert_eq!(stats.total_memories, 2);
    assert_eq!(stats.needs_attention.stale_count, 1);
    let details = stats.details.unwrap();
    assert_eq!(details.staleness_distribution.stale, 1);
    let episodic_stats = &details.by_type["episodic"];
    assert_eq!((episodic_stats.count, episodic_stats.stale_count), (2, 1));
    assert!(episodic_stats.oldest < episodic_stats.newest);
}
