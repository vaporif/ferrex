use ferrex_store::{Memory, MemoryType, MetadataStore, SqliteStore};

use crate::error::CoreError;
use crate::pipeline::StoreContext;
use crate::types::ConflictConfig;

pub enum Classification {
    NoConflict,
    Duplicate(String, f32),
    Update(String),
    Ambiguous { existing_id: String, ratio: f32 },
    MultiMatch(Vec<String>),
}

fn normalize_object(s: &str) -> String {
    s.trim().to_lowercase()
}

pub fn classify(
    incoming_object: &str,
    existing: &[Memory],
    config: &ConflictConfig,
) -> Classification {
    if existing.is_empty() {
        return Classification::NoConflict;
    }
    if existing.len() > 1 {
        return Classification::MultiMatch(existing.iter().map(|m| m.id.clone()).collect());
    }
    let candidate = &existing[0];
    let existing_obj = candidate.object.as_deref().unwrap_or("");
    let norm_in = normalize_object(incoming_object);
    let norm_ex = normalize_object(existing_obj);
    if norm_in == norm_ex {
        return Classification::Duplicate(candidate.id.clone(), 1.0);
    }
    #[allow(clippy::cast_possible_truncation)]
    let ratio = strsim::jaro_winkler(&norm_in, &norm_ex) as f32;
    if ratio >= config.object_fuzzy_duplicate {
        Classification::Duplicate(candidate.id.clone(), ratio)
    } else if ratio < config.object_fuzzy_update {
        Classification::Update(candidate.id.clone())
    } else {
        Classification::Ambiguous {
            existing_id: candidate.id.clone(),
            ratio,
        }
    }
}

pub async fn run(ctx: &mut StoreContext<'_>, metadata: &SqliteStore) -> Result<(), CoreError> {
    if ctx.memory_type != MemoryType::Semantic {
        return Ok(());
    }
    let subject = ctx.req.subject.as_deref().unwrap_or("");
    let normalized_predicate = ctx.normalized_predicate.as_deref().ok_or_else(|| {
        CoreError::Validation("conflict stage requires normalized predicate".into())
    })?;
    let existing = metadata
        .get_memories_by_subject_predicate(subject, normalized_predicate)
        .await?;
    let incoming_object = ctx.req.object.as_deref().unwrap_or("");
    match classify(incoming_object, &existing, ctx.conflict_config) {
        Classification::NoConflict => Ok(()),
        Classification::Duplicate(existing_id, similarity) => Err(CoreError::Duplicate {
            existing_id,
            similarity,
        }),
        Classification::Update(existing_id) => {
            ctx.superseded_ids.push(existing_id);
            Ok(())
        }
        Classification::Ambiguous { existing_id, ratio } => {
            Err(CoreError::ConflictAmbiguous { existing_id, ratio })
        }
        Classification::MultiMatch(existing_ids) => {
            Err(CoreError::MultiMatchConflict { existing_ids })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mem(id: &str, object: &str) -> Memory {
        Memory {
            id: id.into(),
            namespace: "default".into(),
            memory_type: MemoryType::Semantic,
            content: None,
            subject: Some("api".into()),
            predicate: Some("uses".into()),
            object: Some(object.into()),
            confidence: 1.0,
            source: None,
            context: None,
            entities: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            t_valid: None,
            t_invalid: None,
            last_accessed: Utc::now(),
            last_validated: None,
            access_count: 0,
            normalized_predicate: Some("depends_on".into()),
        }
    }

    #[test]
    fn test_classify_no_conflict() {
        let c = ConflictConfig::default();
        assert!(matches!(classify("x", &[], &c), Classification::NoConflict));
    }

    #[test]
    fn test_classify_duplicate_exact() {
        let c = ConflictConfig::default();
        let outcome = classify("tokio 1.38", &[mem("e-1", "tokio 1.38")], &c);
        #[allow(clippy::float_cmp)]
        {
            assert!(
                matches!(outcome, Classification::Duplicate(ref id, s) if id == "e-1" && s == 1.0)
            );
        }
    }

    #[test]
    fn test_classify_duplicate_case_insensitive() {
        let c = ConflictConfig::default();
        let outcome = classify("  Tokio 1.38 ", &[mem("e-1", "tokio 1.38")], &c);
        assert!(matches!(outcome, Classification::Duplicate(_, _)));
    }

    #[test]
    fn test_classify_update_far() {
        let c = ConflictConfig::default();
        // jaro_winkler on these pairs needs to be < object_fuzzy_update (0.50).
        // "async-std 1.0" vs "tokio 1.38" is ~0.52 — too close because of
        // shared digits/space. Use fully unrelated tokens.
        let outcome = classify("postgres", &[mem("e-1", "tokio 1.38")], &c);
        assert!(matches!(outcome, Classification::Update(_)));
    }

    #[test]
    fn test_classify_ambiguous() {
        let c = ConflictConfig::default();
        // Fuzzy ratio on partial overlap lands in the ambiguous band.
        let outcome = classify("tokio 1.38 with patches", &[mem("e-1", "tokio 1.38")], &c);
        assert!(matches!(outcome, Classification::Ambiguous { .. }));
    }

    #[test]
    fn test_classify_multi_match() {
        let c = ConflictConfig::default();
        let existing = vec![mem("e-1", "foo"), mem("e-2", "bar")];
        let outcome = classify("baz", &existing, &c);
        assert!(matches!(outcome, Classification::MultiMatch(ref ids) if ids.len() == 2));
    }
}
