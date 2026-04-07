use ferrex_store::MemoryType;

use crate::error::CoreError;
use crate::pipeline::StoreContext;

pub fn run(ctx: &mut StoreContext<'_>) -> Result<(), CoreError> {
    if ctx.memory_type != MemoryType::Semantic {
        return Ok(());
    }
    let predicate = ctx
        .req
        .predicate
        .as_deref()
        .ok_or_else(|| CoreError::Validation("semantic predicate missing".into()))?;
    ctx.normalized_predicate = Some(ctx.normalizer.normalize(predicate));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::predicate::PredicateNormalizer;
    use crate::types::{ConflictConfig, DedupConfig, StoreRequest};

    #[test]
    fn test_normalizes_semantic_predicate() {
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        groups.insert("depends_on".into(), vec!["uses".into()]);
        let norm = PredicateNormalizer::new(groups);
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let req = StoreRequest {
            content: None,
            memory_type: Some(MemoryType::Semantic),
            subject: Some("a".into()),
            predicate: Some("uses".into()),
            object: Some("b".into()),
            confidence: None,
            source: None,
            context: None,
            entities: vec![],
            namespace: None,
            supersedes: None,
        };
        let mut ctx = StoreContext::new(
            req,
            "default".into(),
            MemoryType::Semantic,
            &norm,
            &dedup,
            &conflict,
        );
        run(&mut ctx).unwrap();
        assert_eq!(ctx.normalized_predicate.as_deref(), Some("depends_on"));
    }

    #[test]
    fn test_skips_non_semantic() {
        let norm = PredicateNormalizer::new(HashMap::new());
        let dedup = DedupConfig::default();
        let conflict = ConflictConfig::default();
        let req = StoreRequest {
            content: Some("ev".into()),
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
        };
        let mut ctx = StoreContext::new(
            req,
            "default".into(),
            MemoryType::Episodic,
            &norm,
            &dedup,
            &conflict,
        );
        run(&mut ctx).unwrap();
        assert!(ctx.normalized_predicate.is_none());
    }
}
