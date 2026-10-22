use chrono::{Duration, Utc};
use ferrex_core::RecallRequest;
use ferrex_store::{MemoryType, MetadataStore, SqliteStore};
use pretty_assertions::assert_eq;

mod common;
use common::{TestContext, episodic, recall_query, semantic};

fn ids(results: &[ferrex_core::RecallResult]) -> Vec<&str> {
    results.iter().map(|r| r.memory.id.as_str()).collect()
}

#[tokio::test]
async fn recall_filters_by_type_and_entity() {
    let ctx = TestContext::new().await;
    let mut tagged = episodic("postgres replication lag spiked during the backup");
    tagged.entities = vec!["postgres".into()];
    let tagged = ctx.service.store(tagged).await.unwrap();
    let untagged = ctx
        .service
        .store(episodic("replication lag on the redis replica"))
        .await
        .unwrap();
    let fact = ctx
        .service
        .store(semantic("replication", "lag_threshold", "5s"))
        .await
        .unwrap();

    let only_semantic = ctx
        .service
        .recall(RecallRequest {
            types: Some(vec![MemoryType::Semantic]),
            ..recall_query("replication lag")
        })
        .await
        .unwrap();
    assert_eq!(ids(&only_semantic), vec![fact.id.as_str()]);

    let by_entity = ctx
        .service
        .recall(RecallRequest {
            entities: Some(vec!["postgres".into()]),
            ..recall_query("replication lag")
        })
        .await
        .unwrap();
    let found = ids(&by_entity);
    assert!(found.contains(&tagged.id.as_str()), "{found:?}");
    assert!(!found.contains(&untagged.id.as_str()), "{found:?}");
}

#[tokio::test]
async fn recall_as_of_includes_invalidated_when_asked() {
    let ctx = TestContext::new().await;
    let old = ctx
        .service
        .store(episodic("the primary region is us-east-1"))
        .await
        .unwrap();
    let mut req = episodic("the primary region is eu-west-1");
    req.supersedes = Some(old.id.clone());
    ctx.service.store(req).await.unwrap();

    let later = Utc::now() + Duration::hours(1);
    let as_of_default = ctx
        .service
        .recall(RecallRequest {
            as_of: Some(later),
            ..recall_query("primary region")
        })
        .await
        .unwrap();
    assert!(!ids(&as_of_default).contains(&old.id.as_str()));

    let as_of_all = ctx
        .service
        .recall(RecallRequest {
            as_of: Some(later),
            include_invalidated: Some(true),
            ..recall_query("primary region")
        })
        .await
        .unwrap();
    assert!(ids(&as_of_all).contains(&old.id.as_str()));
}

#[tokio::test]
async fn recall_validate_ids_marks_only_returned_ids() {
    let ctx = TestContext::with_file_db().await;
    let hit = ctx
        .service
        .store(episodic("the staging database password rotates weekly"))
        .await
        .unwrap();
    let miss = ctx
        .service
        .store(episodic("lunch is at noon on fridays"))
        .await
        .unwrap();

    ctx.service
        .recall(RecallRequest {
            validate_ids: Some(vec![hit.id.clone(), miss.id.clone(), "bogus".into()]),
            limit: Some(1),
            ..recall_query("staging database password rotation")
        })
        .await
        .unwrap();

    let sqlite = SqliteStore::open(&ctx.config.db_path).unwrap();
    let hit_row = sqlite.get_memory(&hit.id).await.unwrap().unwrap();
    let miss_row = sqlite.get_memory(&miss.id).await.unwrap().unwrap();
    assert!(hit_row.last_validated.is_some());
    assert!(miss_row.last_validated.is_none());
}
