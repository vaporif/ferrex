use ferrex_store::MetadataStore;

use super::{DEFAULT_RECALL_LIMIT, MAX_RECALL_LIMIT, MemoryService};
use crate::entity::normalize;
use crate::error::CoreError;
use crate::types::{TimelineEntry, TimelineRequest, TimelineResponse};

impl MemoryService {
    #[tracing::instrument(name = "timeline", skip_all, fields(entity = %req.entity, namespace))]
    pub async fn timeline(&self, req: TimelineRequest) -> Result<TimelineResponse, CoreError> {
        let namespace = req
            .namespace
            .unwrap_or_else(|| self.config.namespace.clone());
        tracing::Span::current().record("namespace", namespace.as_str());

        let normalized = normalize(&req.entity);
        if normalized.is_empty() {
            return Err(CoreError::Validation("empty entity name".into()));
        }

        let limit = req
            .limit
            .unwrap_or(DEFAULT_RECALL_LIMIT)
            .min(MAX_RECALL_LIMIT);
        let include_invalidated = req.include_invalidated.unwrap_or(false);

        let Some(entity) = self.metadata_store.get_entity_by_name(&normalized).await? else {
            return Ok(TimelineResponse {
                entity: req.entity,
                resolved_entity_id: None,
                entries: vec![],
            });
        };

        #[allow(clippy::cast_possible_wrap)]
        let memories = self
            .metadata_store
            .timeline_by_entity(
                &entity.id,
                &namespace,
                limit as i64,
                include_invalidated,
                req.types.as_deref(),
            )
            .await?;

        let entries = memories
            .into_iter()
            .map(|m| TimelineEntry {
                occurred_at: m.t_valid.unwrap_or(m.created_at),
                memory: m,
            })
            .collect();

        Ok(TimelineResponse {
            entity: req.entity,
            resolved_entity_id: Some(entity.id),
            entries,
        })
    }
}
