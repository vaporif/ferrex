use ferrex_embed::Embedder;
use ferrex_store::{SqliteStore, VectorStore};

use crate::entity::EntityResolver;
use crate::error::CoreError;
use crate::pipeline::StoreContext;

pub async fn run(
    ctx: &mut StoreContext<'_>,
    metadata: &SqliteStore,
    vectors: &VectorStore,
    embedder: &Embedder,
) -> Result<(), CoreError> {
    if ctx.req.entities.is_empty() {
        return Ok(());
    }
    vectors.ensure_collection(&ctx.namespace).await?;
    let resolver = EntityResolver {
        metadata_store: metadata,
        vector_store: vectors,
        embedder,
    };
    ctx.resolved_entities = resolver.resolve(&ctx.req.entities, &ctx.namespace).await?;
    Ok(())
}
