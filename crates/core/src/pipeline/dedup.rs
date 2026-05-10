use ferrex_store::{MemorySearch, MemoryType, VectorStore};

use crate::error::CoreError;
use crate::pipeline::StoreContext;

const TOP_K: usize = 5;

#[tracing::instrument(name = "dedup_check", skip_all, fields(max_similarity, rejected))]
pub async fn run(ctx: &StoreContext<'_>, vectors: &VectorStore) -> Result<(), CoreError> {
    if ctx.memory_type == MemoryType::Semantic {
        return Ok(());
    }
    let embedding = ctx
        .embedding
        .as_ref()
        .ok_or_else(|| CoreError::Validation("dedup stage requires embedding".into()))?;

    let search = MemorySearch::new().only_type(ctx.memory_type);

    let results = vectors
        .search_memories_dense(&ctx.namespace, embedding.clone(), TOP_K, &search)
        .await?;

    let (max_sim, is_rejected) = results.first().map_or((0.0_f32, false), |(_, s)| {
        (*s, *s >= ctx.dedup_config.threshold)
    });
    tracing::Span::current().record("max_similarity", max_sim);
    tracing::Span::current().record("rejected", is_rejected);

    if let Some((id, score)) = results.first()
        && *score >= ctx.dedup_config.threshold
    {
        return Err(CoreError::Duplicate {
            existing_id: id.clone(),
            similarity: *score,
        });
    }
    Ok(())
}
