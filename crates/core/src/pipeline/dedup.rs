use ferrex_store::{MemoryType, VectorStore};
use qdrant_client::qdrant::{Condition, Filter};

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

    let filter = Filter::must(vec![
        Condition::matches(
            ferrex_store::QdrantField::POINT_TYPE,
            ferrex_store::PointType::MEMORY.to_string(),
        ),
        Condition::matches(
            ferrex_store::QdrantField::MEMORY_TYPE,
            ctx.memory_type.as_str().to_string(),
        ),
    ]);

    let results = vectors
        .search_dense(&ctx.namespace, embedding.clone(), TOP_K, Some(filter))
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
