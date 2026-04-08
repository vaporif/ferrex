use ferrex_store::{Memory, MemoryType, MetadataStore};

use super::MemoryService;
use crate::error::CoreError;
use crate::pipeline::{self, StoreContext};
use crate::types::{StoreRequest, StoreResponse};

impl MemoryService {
    #[tracing::instrument(name = "store", skip_all, fields(memory_type, namespace))]
    pub async fn store(&self, req: StoreRequest) -> Result<StoreResponse, CoreError> {
        let op_start = std::time::Instant::now();
        let memory_type = detect_memory_type(&req);
        let namespace = req
            .namespace
            .clone()
            .unwrap_or_else(|| self.config.namespace.clone());
        tracing::Span::current().record("memory_type", memory_type.as_str());
        tracing::Span::current().record("namespace", &*namespace);
        let normalizer = self
            .normalizers
            .get(&namespace)
            .cloned()
            .unwrap_or_else(|| std::sync::Arc::clone(&self.default_normalizer));

        if namespace != self.config.namespace {
            self.vector_store.ensure_collection(&namespace).await?;
        }

        let mut ctx = StoreContext::new(
            req,
            namespace,
            memory_type,
            &normalizer,
            &self.config.deduplication,
            &self.config.conflict,
        );

        pipeline::validate::run(&ctx)?;

        let supersedes_target = ctx.req.supersedes.clone();
        let is_supersede = supersedes_target.is_some();

        if memory_type == MemoryType::Semantic {
            pipeline::normalize_predicate::run(&mut ctx)?;
        }
        pipeline::embed::run(&mut ctx, &self.embedder).await?;
        if !is_supersede {
            pipeline::dedup::run(&ctx, &self.vector_store).await?;
            pipeline::conflict::run(&mut ctx, &self.metadata_store).await?;
        }
        pipeline::resolve_entities::run(
            &mut ctx,
            &self.metadata_store,
            &self.vector_store,
            &self.embedder,
        )
        .await?;

        let memory = if let Some(ref target_id) = supersedes_target {
            self.supersede_write(&ctx, target_id).await?
        } else {
            pipeline::write::run(&ctx, &self.metadata_store, &self.vector_store).await?
        };

        let superseded = if let Some(id) = supersedes_target {
            vec![id]
        } else {
            ctx.superseded_ids.clone()
        };

        self.cache.bump_generation(&ctx.namespace).await;

        self.ops_buffer
            .record("store", memory_type.as_str(), op_start.elapsed(), "ok");

        Ok(StoreResponse {
            id: memory.id,
            memory_type: memory_type.as_str().to_string(),
            superseded,
        })
    }

    async fn supersede_write(
        &self,
        ctx: &StoreContext<'_>,
        target_id: &str,
    ) -> Result<Memory, CoreError> {
        let target = self
            .metadata_store
            .get_memory(target_id)
            .await?
            .ok_or_else(|| {
                CoreError::Validation(format!("supersedes target not found: {target_id}"))
            })?;
        if target.namespace != ctx.namespace {
            return Err(CoreError::Validation(format!(
                "supersedes target {target_id} is in namespace {}, not {}",
                target.namespace, ctx.namespace
            )));
        }
        if target.t_invalid.is_some() {
            return Err(CoreError::Validation(format!(
                "supersedes target {target_id} is already invalidated"
            )));
        }

        pipeline::write::run_supersede(ctx, &self.metadata_store, &self.vector_store, target_id)
            .await
    }
}

const fn detect_memory_type(req: &StoreRequest) -> MemoryType {
    match req.memory_type {
        Some(t) => t,
        None if req.subject.is_some() && req.predicate.is_some() && req.object.is_some() => {
            MemoryType::Semantic
        }
        None => MemoryType::Episodic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_detect_semantic() {
        let req = StoreRequest {
            content: None,
            memory_type: None,
            subject: Some("api-server".into()),
            predicate: Some("uses".into()),
            object: Some("tokio 1.38".into()),
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Semantic);
    }

    #[test]
    fn test_auto_detect_episodic() {
        let req = StoreRequest {
            content: Some("something happened".into()),
            memory_type: None,
            subject: None,
            predicate: None,
            object: None,
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Episodic);
    }

    #[test]
    fn test_auto_detect_explicit_procedural() {
        let req = StoreRequest {
            content: Some("step 1: do this".into()),
            memory_type: Some(MemoryType::Procedural),
            subject: None,
            predicate: None,
            object: None,
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Procedural);
    }

    #[test]
    fn test_semantic_triple_missing_predicate_detects_as_episodic() {
        let req = StoreRequest {
            content: None,
            memory_type: None,
            subject: Some("foo".into()),
            predicate: None,
            object: Some("bar".into()),
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        assert_eq!(detect_memory_type(&req), MemoryType::Episodic);
    }
}
