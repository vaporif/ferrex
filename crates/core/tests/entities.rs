use ferrex_store::{MetadataStore, SqliteStore};
use pretty_assertions::assert_eq;

mod common;
use common::episodic;

/// Fuzzy matching misses this pair (Jaro-Winkler 0.79) but their embeddings are
/// near-identical (cosine 0.96), so the embedding stage must merge them.
#[tokio::test]
async fn embedding_stage_merges_near_identical_entity_names() {
    let ctx = common::TestContext::with_file_db().await;
    let mut first = episodic("the kubernetes cluster was upgraded");
    first.entities = vec!["kubernetes cluster".into()];
    ctx.service.store(first).await.unwrap();

    let mut second = episodic("node pool added to the cluster");
    second.entities = vec!["the kubernetes cluster".into()];
    let stored = ctx.service.store(second).await.unwrap();

    let sqlite = SqliteStore::open(&ctx.config.db_path).unwrap();
    let memory = sqlite.get_memory(&stored.id).await.unwrap().unwrap();
    assert_eq!(memory.entities, vec!["kubernetes cluster".to_string()]);
    assert_eq!(sqlite.get_all_entities().await.unwrap().len(), 1);
}

/// Sharing one word puts an entity at the top of both the dense and sparse lists;
/// that must not be enough to merge unrelated entities.
#[tokio::test]
async fn embedding_stage_keeps_distinct_entities_sharing_a_word() {
    let ctx = common::TestContext::with_file_db().await;
    let mut first = episodic("the kubernetes cluster was upgraded");
    first.entities = vec!["kubernetes cluster".into()];
    ctx.service.store(first).await.unwrap();

    let mut second = episodic("the redis cluster lost a replica");
    second.entities = vec!["redis cluster".into()];
    let stored = ctx.service.store(second).await.unwrap();

    let sqlite = SqliteStore::open(&ctx.config.db_path).unwrap();
    let memory = sqlite.get_memory(&stored.id).await.unwrap().unwrap();
    assert_eq!(memory.entities, vec!["redis cluster".to_string()]);
}
