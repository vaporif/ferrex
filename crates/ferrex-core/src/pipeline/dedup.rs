use ferrex_store::{MemoryType, VectorStore};
use qdrant_client::qdrant::{Condition, Filter};

use crate::error::CoreError;
use crate::pipeline::StoreContext;

const TOP_K: usize = 5;

pub async fn run(ctx: &StoreContext<'_>, vectors: &VectorStore) -> Result<(), CoreError> {
    if ctx.memory_type == MemoryType::Semantic {
        return Ok(());
    }
    let embedding = ctx
        .embedding
        .as_ref()
        .ok_or_else(|| CoreError::Validation("dedup stage requires embedding".into()))?;
    let search_text = ctx.search_text.as_deref().unwrap_or("");

    let filter = Filter::must(vec![
        Condition::matches(
            ferrex_store::POINT_TYPE_FIELD,
            ferrex_store::POINT_TYPE_MEMORY.to_string(),
        ),
        Condition::matches("memory_type", ctx.memory_type.as_str().to_string()),
    ]);

    let results = vectors
        .search(
            &ctx.namespace,
            embedding.clone(),
            search_text,
            TOP_K,
            Some(filter),
        )
        .await?;

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
