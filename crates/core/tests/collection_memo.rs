use ferrex_core::StoreRequest;
use ferrex_store::VectorStore;

mod common;
use common::episodic;

fn in_namespace(ns: &str, content: &str) -> StoreRequest {
    StoreRequest {
        namespace: Some(ns.into()),
        ..episodic(content)
    }
}

#[tokio::test]
async fn store_does_not_recheck_a_known_collection() {
    let ctx = common::TestContext::new().await;
    let other = format!("{}_other", ctx.namespace);
    ctx.service
        .store(in_namespace(&other, "first memory"))
        .await
        .unwrap();

    // A second handle drops the collection behind the service's back; a store that
    // re-ran ensure_collection would recreate it and succeed.
    let outside = VectorStore::new(&ctx.qdrant_url, 384).unwrap();
    outside
        .delete_collection(&format!("ferrex_{other}"))
        .await
        .unwrap();

    let result = ctx
        .service
        .store(in_namespace(&other, "an unrelated second memory"))
        .await;
    assert!(
        result.is_err(),
        "store re-checked the collection: {result:?}"
    );
}

#[tokio::test]
async fn deleting_a_collection_forgets_it() {
    let ctx = common::TestContext::new().await;
    let other = format!("{}_other", ctx.namespace);
    ctx.service
        .store(in_namespace(&other, "first memory"))
        .await
        .unwrap();

    ctx.service
        .vector_store()
        .delete_collection(&format!("ferrex_{other}"))
        .await
        .unwrap();

    ctx.service
        .store(in_namespace(&other, "an unrelated second memory"))
        .await
        .unwrap();
}
