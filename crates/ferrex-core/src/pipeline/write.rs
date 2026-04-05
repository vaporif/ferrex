// FOLLOW-UP: the finalization step below is not a single SQLite transaction.
// The spec calls for (insert_memory + link_memory_entity + invalidate_memory +
// clear_pending_op) to run atomically. The current MetadataStore trait doesn't
// expose a multi-statement transaction helper; adding one would ripple through
// every call site. The journal still catches crashes — a crash mid-finalization
// leaves a row with qdrant_written=1 that recovery (Task 19) reconciles via
// orphan cleanup. Revisit after Phase 3 lands.

use chrono::Utc;
use ferrex_store::{
    Memory, MetadataStore, POINT_TYPE_FIELD, POINT_TYPE_MEMORY, PendingOp, PendingOpKind,
    SqliteStore, VectorStore,
};
use qdrant_client::Payload;
use uuid::Uuid;

use crate::error::CoreError;
use crate::pipeline::StoreContext;

pub async fn run(
    ctx: &StoreContext<'_>,
    metadata: &SqliteStore,
    vectors: &VectorStore,
) -> Result<Memory, CoreError> {
    let embedding = ctx
        .embedding
        .clone()
        .ok_or_else(|| CoreError::Validation("write stage requires embedding".into()))?;
    let search_text = ctx.search_text.clone().unwrap_or_default();

    let memory = build_memory(ctx);

    // 1. Pre-write journal.
    let op = PendingOp {
        op_id: Uuid::now_v7().to_string(),
        kind: PendingOpKind::Store,
        memory_id: memory.id.clone(),
        namespace: memory.namespace.clone(),
        target_id: None,
        qdrant_written: false,
        started_at: Utc::now(),
    };
    metadata.insert_pending_op(&op).await?;

    // 2. Qdrant step.
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

    let id_uuid =
        Uuid::parse_str(&memory.id).map_err(|e| CoreError::Validation(e.to_string()))?;
    vectors
        .upsert(&memory.namespace, id_uuid, embedding, &search_text, payload)
        .await?;
    metadata.mark_pending_op_qdrant_written(&op.op_id).await?;

    // 3. Finalization (non-atomic; see file header).
    metadata.insert_memory(&memory).await?;
    for entity in &ctx.resolved_entities {
        metadata.link_memory_entity(&memory.id, &entity.id).await?;
    }
    for sup in &ctx.superseded_ids {
        metadata.invalidate_memory(sup, Utc::now()).await?;
    }
    metadata.clear_pending_op(&op.op_id).await?;

    Ok(memory)
}

fn build_memory(ctx: &StoreContext<'_>) -> Memory {
    let confidence = ctx.req.confidence.map_or(1.0, |c| c.clamp(0.0, 1.0));
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
        subject: ctx.req.subject.clone(),
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
