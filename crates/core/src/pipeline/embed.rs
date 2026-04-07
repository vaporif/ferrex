use ferrex_embed::Embedder;
use ferrex_store::MemoryType;

use crate::error::CoreError;
use crate::pipeline::StoreContext;

#[tracing::instrument(name = "embed", skip_all, fields(dims))]
pub async fn run(ctx: &mut StoreContext<'_>, embedder: &Embedder) -> Result<(), CoreError> {
    let search_text = build_searchable_text(ctx);
    let embedding = embedder.embed(&search_text).await?;
    ctx.search_text = Some(search_text);
    ctx.embedding = Some(embedding);
    tracing::Span::current().record("dims", ctx.embedding.as_ref().map_or(0, Vec::len));
    Ok(())
}

fn build_searchable_text(ctx: &StoreContext<'_>) -> String {
    match ctx.memory_type {
        MemoryType::Semantic => format!(
            "{} {} {}",
            ctx.req.subject.as_deref().unwrap_or(""),
            ctx.req.predicate.as_deref().unwrap_or(""),
            ctx.req.object.as_deref().unwrap_or(""),
        ),
        _ => ctx.req.content.clone().unwrap_or_default(),
    }
}
