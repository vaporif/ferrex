use chrono::Utc;
use ferrex_store::{MetadataStore, PendingOp, PendingOpKind, SqliteStore};

mod common;
use common::episodic;

/// A crash inside the old two-statement `complete_op` left a finished op in both
/// journal tables; recovery must not compensate (delete) its memory.
#[tokio::test]
async fn recovery_keeps_memory_of_op_already_completed() {
    let ctx = common::TestContext::with_file_db().await;
    let stored = ctx
        .service
        .store(episodic("the staging cluster runs on k3s"))
        .await
        .unwrap();

    let sqlite = SqliteStore::open(&ctx.config.db_path).unwrap();
    let done = sqlite.list_completed_ops(None, 10, None).await.unwrap();
    let op = done.iter().find(|o| o.memory_id == stored.id).unwrap();
    sqlite
        .insert_pending_op(&PendingOp {
            op_id: op.op_id.clone(),
            kind: PendingOpKind::Store,
            memory_id: stored.id.clone(),
            namespace: ctx.namespace.clone(),
            target_id: None,
            qdrant_written: true,
            started_at: Utc::now(),
        })
        .await
        .unwrap();

    let _reopened = ctx.reopen().await;

    assert!(sqlite.get_memory(&stored.id).await.unwrap().is_some());
    assert!(sqlite.list_pending_ops().await.unwrap().is_empty());
}

#[tokio::test]
async fn supersede_commits_new_memory_and_invalidation_together() {
    let ctx = common::TestContext::with_file_db().await;
    let old = ctx
        .service
        .store(episodic("the deploy target is us-east-1"))
        .await
        .unwrap();
    let mut req = episodic("the deploy target is eu-west-1");
    req.supersedes = Some(old.id.clone());
    let new = ctx.service.store(req).await.unwrap();

    let sqlite = SqliteStore::open(&ctx.config.db_path).unwrap();
    assert!(sqlite.get_memory(&new.id).await.unwrap().is_some());
    let old_row = sqlite.get_memory(&old.id).await.unwrap().unwrap();
    assert!(old_row.t_invalid.is_some());
    assert!(sqlite.list_pending_ops().await.unwrap().is_empty());
}

#[tokio::test]
async fn audit_fix_leaves_entity_points_alone() {
    let ctx = common::TestContext::new().await;
    let mut req = episodic("the cache layer moved to redis");
    req.entities = vec!["redis".into()];
    ctx.service.store(req).await.unwrap();

    let report = ctx.service.audit_reconcile(true, 1000).await.unwrap();

    assert!(report.fixed.is_empty(), "deleted: {:?}", report.fixed);
    assert!(report.sqlite_only.is_empty());
}

/// Superseded memories keep their vectors for `include_invalidated` and `as_of` recall.
#[tokio::test]
async fn audit_fix_keeps_vectors_of_invalidated_memories() {
    let ctx = common::TestContext::new().await;
    let old = ctx
        .service
        .store(episodic("the deploy target is us-east-1"))
        .await
        .unwrap();
    let mut req = episodic("the deploy target is eu-west-1");
    req.supersedes = Some(old.id.clone());
    ctx.service.store(req).await.unwrap();

    let report = ctx.service.audit_reconcile(true, 1000).await.unwrap();

    assert!(report.fixed.is_empty(), "deleted: {:?}", report.fixed);
}
