use ferrex_store::MemoryType;

use crate::error::CoreError;
use crate::pipeline::StoreContext;

const MAX_CONTENT_LENGTH: usize = 4096;
const MAX_SUBJECT_LENGTH: usize = 512;
const MAX_PREDICATE_LENGTH: usize = 256;
const MAX_OBJECT_LENGTH: usize = 4096;
const MAX_ENTITIES_PER_REQUEST: usize = 50;

pub fn run(ctx: &StoreContext<'_>) -> Result<(), CoreError> {
    if ctx.req.entities.len() > MAX_ENTITIES_PER_REQUEST {
        return Err(CoreError::Validation(format!(
            "too many entities: {} exceeds limit of {MAX_ENTITIES_PER_REQUEST}",
            ctx.req.entities.len()
        )));
    }

    match ctx.memory_type {
        MemoryType::Episodic | MemoryType::Procedural => {
            let Some(content) = ctx.req.content.as_deref().filter(|c| !c.is_empty()) else {
                return Err(CoreError::Validation(format!(
                    "{} memory requires content",
                    ctx.memory_type
                )));
            };
            if content.len() > MAX_CONTENT_LENGTH {
                return Err(CoreError::Validation(format!(
                    "content exceeds {MAX_CONTENT_LENGTH} byte limit"
                )));
            }
        }
        MemoryType::Semantic => {
            let non_empty = |opt: &Option<String>| opt.as_deref().is_some_and(|s| !s.is_empty());
            if !non_empty(&ctx.req.subject)
                || !non_empty(&ctx.req.predicate)
                || !non_empty(&ctx.req.object)
            {
                return Err(CoreError::Validation(
                    "semantic memory requires non-empty subject, predicate, and object".into(),
                ));
            }
            if let Some(ref s) = ctx.req.subject
                && s.len() > MAX_SUBJECT_LENGTH
            {
                return Err(CoreError::Validation(format!(
                    "subject exceeds {MAX_SUBJECT_LENGTH} byte limit"
                )));
            }
            if let Some(ref p) = ctx.req.predicate
                && p.len() > MAX_PREDICATE_LENGTH
            {
                return Err(CoreError::Validation(format!(
                    "predicate exceeds {MAX_PREDICATE_LENGTH} byte limit"
                )));
            }
            if let Some(ref o) = ctx.req.object
                && o.len() > MAX_OBJECT_LENGTH
            {
                return Err(CoreError::Validation(format!(
                    "object exceeds {MAX_OBJECT_LENGTH} byte limit"
                )));
            }
        }
    }
    if let Some(ref s) = ctx.req.supersedes {
        uuid::Uuid::parse_str(s)
            .map_err(|_| CoreError::Validation(format!("invalid supersedes UUID: {s}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::predicate::PredicateNormalizer;
    use crate::types::{ConflictConfig, DedupConfig, StoreRequest};

    fn req_episodic(content: Option<&str>) -> StoreRequest {
        StoreRequest {
            content: content.map(str::to_string),
            memory_type: Some(MemoryType::Episodic),
            subject: None,
            predicate: None,
            object: None,
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        }
    }

    fn req_semantic(subject: &str, predicate: &str, object: &str) -> StoreRequest {
        StoreRequest {
            content: None,
            memory_type: Some(MemoryType::Semantic),
            subject: Some(subject.into()),
            predicate: Some(predicate.into()),
            object: Some(object.into()),
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        }
    }

    fn ctx<'a>(
        req: StoreRequest,
        mt: MemoryType,
        norm: &'a PredicateNormalizer,
        dedup: &'a DedupConfig,
        conflict: &'a ConflictConfig,
    ) -> StoreContext<'a> {
        StoreContext::new(req, "default".into(), mt, norm, dedup, conflict)
    }

    #[test]
    fn test_episodic_missing_content_fails() {
        let norm = PredicateNormalizer::new(HashMap::new());
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let c = ctx(
            req_episodic(None),
            MemoryType::Episodic,
            &norm,
            &dedup,
            &conflict,
        );
        assert!(matches!(run(&c), Err(CoreError::Validation(_))));
    }

    #[test]
    fn test_episodic_oversize_content_fails() {
        let norm = PredicateNormalizer::new(HashMap::new());
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let big = "x".repeat(MAX_CONTENT_LENGTH + 1);
        let c = ctx(
            req_episodic(Some(&big)),
            MemoryType::Episodic,
            &norm,
            &dedup,
            &conflict,
        );
        assert!(matches!(run(&c), Err(CoreError::Validation(_))));
    }

    #[test]
    fn test_semantic_happy() {
        let norm = PredicateNormalizer::new(HashMap::new());
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let c = ctx(
            req_semantic("a", "b", "c"),
            MemoryType::Semantic,
            &norm,
            &dedup,
            &conflict,
        );
        run(&c).unwrap();
    }

    #[test]
    fn test_semantic_missing_field_fails() {
        let norm = PredicateNormalizer::new(HashMap::new());
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let mut req = req_semantic("a", "b", "c");
        req.object = Some(String::new());
        let c = ctx(req, MemoryType::Semantic, &norm, &dedup, &conflict);
        assert!(matches!(run(&c), Err(CoreError::Validation(_))));
    }

    #[test]
    fn test_invalid_supersedes_uuid_fails() {
        let norm = PredicateNormalizer::new(HashMap::new());
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let mut req = req_episodic(Some("hi"));
        req.supersedes = Some("not-a-uuid".into());
        let c = ctx(req, MemoryType::Episodic, &norm, &dedup, &conflict);
        assert!(matches!(run(&c), Err(CoreError::Validation(_))));
    }
}
