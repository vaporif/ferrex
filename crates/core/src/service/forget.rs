use chrono::Utc;
use ferrex_store::{MetadataStore, PendingOp, PendingOpKind};
use uuid::Uuid;

use super::MemoryService;
use crate::error::CoreError;
use crate::types::{ForgetRequest, ForgetResponse};

impl MemoryService {
    pub async fn forget(&self, req: ForgetRequest) -> Result<ForgetResponse, CoreError> {
        let op_start = std::time::Instant::now();

        let uuids: Vec<Uuid> = req
            .ids
            .iter()
            .map(|id| {
                Uuid::parse_str(id)
                    .map_err(|_| CoreError::Validation(format!("invalid UUID: {id}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if req.cascade.is_some() {
            tracing::warn!("forget: `cascade` field is deprecated and ignored");
        }

        let mut deleted = Vec::new();
        let mut not_found = Vec::new();

        for (id, uuid) in req.ids.iter().zip(uuids.iter()) {
            let Some(memory) = self.metadata_store.get_memory(id).await? else {
                not_found.push(id.clone());
                continue;
            };

            let op = PendingOp {
                op_id: Uuid::now_v7().to_string(),
                kind: PendingOpKind::Forget,
                memory_id: id.clone(),
                namespace: memory.namespace.clone(),
                target_id: None,
                qdrant_written: false,
                started_at: Utc::now(),
            };
            self.metadata_store.insert_pending_op(&op).await?;

            self.vector_store
                .delete_by_ids(&memory.namespace, &[*uuid])
                .await?;
            self.metadata_store
                .mark_pending_op_qdrant_written(&op.op_id)
                .await?;

            self.metadata_store
                .delete_memories(std::slice::from_ref(id))
                .await?;

            let completed = op.into_completed(id.clone(), memory.namespace.clone());
            self.metadata_store.complete_op(&completed).await?;

            deleted.push(id.clone());
            self.cache.bump_generation(&memory.namespace).await;
        }

        let outcome = if not_found.is_empty() {
            "ok".to_string()
        } else {
            format!("{} deleted, {} not found", deleted.len(), not_found.len())
        };
        self.ops_buffer.record(
            "forget",
            &format!("{} ids", req.ids.len()),
            op_start.elapsed(),
            &outcome,
        );

        Ok(ForgetResponse {
            message: format!("deleted {} memories", deleted.len()),
            deleted,
            not_found,
        })
    }
}
