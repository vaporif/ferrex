use chrono::Utc;
use ferrex_store::{
    Memory, MetadataStore, POINT_TYPE_FIELD, POINT_TYPE_MEMORY, PendingOp, PendingOpKind,
    SqliteStore, VectorStore,
};
use qdrant_client::Payload;
use uuid::Uuid;

use crate::error::CoreError;
use crate::pipeline::StoreContext;

#[tracing::instrument(name = "store_write", skip_all)]
pub async fn run(
    ctx: &StoreContext<'_>,
    metadata: &SqliteStore,
    vectors: &VectorStore,
) -> Result<Memory, CoreError> {
    write_impl(ctx, metadata, vectors, PendingOpKind::Store, None).await
}

#[tracing::instrument(name = "supersede_write", skip_all)]
pub async fn run_supersede(
    ctx: &StoreContext<'_>,
    metadata: &SqliteStore,
    vectors: &VectorStore,
    target_id: &str,
) -> Result<Memory, CoreError> {
    write_impl(
        ctx,
        metadata,
        vectors,
        PendingOpKind::Supersede,
        Some(target_id),
    )
    .await
}

#[tracing::instrument(name = "dual_write", skip_all, fields(qdrant_ok, sqlite_ok))]
async fn write_impl(
    ctx: &StoreContext<'_>,
    metadata: &SqliteStore,
    vectors: &VectorStore,
    op_kind: PendingOpKind,
    target_id: Option<&str>,
) -> Result<Memory, CoreError> {
    let embedding = ctx
        .embedding
        .clone()
        .ok_or_else(|| CoreError::Validation("write stage requires embedding".into()))?;
    let search_text = ctx.search_text.clone().unwrap_or_default();

    let memory = build_memory(ctx);

    let op = PendingOp {
        op_id: Uuid::now_v7().to_string(),
        kind: op_kind,
        memory_id: memory.id.clone(),
        namespace: memory.namespace.clone(),
        target_id: target_id.map(String::from),
        qdrant_written: false,
        started_at: Utc::now(),
    };
    metadata.insert_pending_op(&op).await?;

    let payload = Payload::try_from(serde_json::json!({
        "memory_id": memory.id,
        "memory_type": memory.memory_type.as_str(),
        "namespace": memory.namespace,
        "searchable_text": search_text,
        "entities": &memory.entities,
        "created_at": memory.created_at.to_rfc3339(),
        POINT_TYPE_FIELD: POINT_TYPE_MEMORY,
    }))
    .map_err(|e| CoreError::Validation(e.to_string()))?;

    vectors
        .upsert(&memory.namespace, ctx.id, embedding, &search_text, payload)
        .await?;
    metadata.mark_pending_op_qdrant_written(&op.op_id).await?;
    tracing::Span::current().record("qdrant_ok", true);

    metadata.insert_memory(&memory).await?;
    tracing::Span::current().record("sqlite_ok", true);
    for entity in &ctx.resolved_entities {
        metadata.link_memory_entity(&memory.id, &entity.id).await?;
    }
    for sup in &ctx.superseded_ids {
        metadata.invalidate_memory(sup, ctx.now).await?;
    }
    if let Some(tid) = target_id {
        metadata.invalidate_memory(tid, ctx.now).await?;
    }
    let completed = op.into_completed(memory.id.clone(), memory.namespace.clone());
    metadata.complete_op(&completed).await?;

    Ok(memory)
}

fn build_memory(ctx: &StoreContext<'_>) -> Memory {
    const DEFAULT_CONFIDENCE: f64 = 1.0;
    let confidence = ctx
        .req
        .confidence
        .map_or(DEFAULT_CONFIDENCE, |c| c.clamp(0.0, 1.0));
    let entity_names = ctx
        .resolved_entities
        .iter()
        .map(|e| e.name.clone())
        .collect();
    Memory {
        id: ctx.id.to_string(),
        namespace: ctx.namespace.clone(),
        memory_type: ctx.memory_type,
        content: ctx.req.content.clone(),
        subject: ctx.req.subject.as_deref().map(|s| s.trim().to_lowercase()),
        predicate: ctx.req.predicate.clone(),
        object: ctx.req.object.clone(),
        confidence,
        source: ctx.req.source.clone(),
        context: ctx.req.context.clone(),
        entities: entity_names,
        created_at: ctx.now,
        updated_at: ctx.now,
        t_valid: None,
        t_invalid: None,
        last_accessed: ctx.now,
        last_validated: None,
        access_count: 0,
        normalized_predicate: ctx.normalized_predicate.clone(),
    }
}
