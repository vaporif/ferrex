use std::collections::{HashMap, HashSet};

use ferrex_store::Memory;

use crate::types::{ContradictionMatchType, ContradictionPair};

pub const REFLECT_FUZZY_PREDICATE_THRESHOLD: f64 = 0.75;
pub const DEFAULT_REFLECT_LIMIT: u32 = 20;

fn effective_predicate(mem: &Memory) -> &str {
    mem.normalized_predicate
        .as_deref()
        .or(mem.predicate.as_deref())
        .unwrap_or("")
}

fn effective_object(mem: &Memory) -> &str {
    mem.object.as_deref().unwrap_or("")
}

pub fn detect_contradictions(
    valid_semantics: &[Memory],
    entity_aliases: &HashMap<String, Vec<String>>,
) -> Vec<ContradictionPair> {
    let mut contradictions = Vec::new();
    let mut grouped: HashMap<(String, String), Vec<&Memory>> = HashMap::new();

    for mem in valid_semantics {
        let subj = match mem.subject.as_deref() {
            Some(s) => s.to_lowercase(),
            None => continue,
        };
        let pred = match mem.normalized_predicate.as_deref() {
            Some(p) => p.to_string(),
            None => match mem.predicate.as_deref() {
                Some(p) => p.to_lowercase(),
                None => continue,
            },
        };
        grouped.entry((subj, pred)).or_default().push(mem);
    }

    let mut matched_ids: HashSet<&str> = HashSet::new();

    for mems in grouped.values() {
        find_pairs_with_different_objects(mems, &mut contradictions, &mut matched_ids, |_a, _b| {
            Some((ContradictionMatchType::ExactPredicate, 1.0))
        });
    }

    let by_subject: HashMap<String, Vec<&Memory>> = {
        let mut m: HashMap<String, Vec<&Memory>> = HashMap::new();
        for mem in valid_semantics {
            if let Some(subj) = mem.subject.as_deref() {
                m.entry(subj.to_lowercase()).or_default().push(mem);
            }
        }
        m
    };

    for mems in by_subject.values() {
        find_pairs_with_different_objects(mems, &mut contradictions, &mut matched_ids, |a, b| {
            let pred_a = effective_predicate(a);
            let pred_b = effective_predicate(b);
            if pred_a == pred_b {
                return None;
            }
            let sim = strsim::jaro_winkler(pred_a, pred_b);
            (sim > REFLECT_FUZZY_PREDICATE_THRESHOLD)
                .then_some((ContradictionMatchType::FuzzyPredicate, sim))
        });
    }

    find_alias_contradictions(
        &by_subject,
        entity_aliases,
        &matched_ids,
        &mut contradictions,
    );

    contradictions
}

fn find_pairs_with_different_objects<'a>(
    mems: &[&'a Memory],
    contradictions: &mut Vec<ContradictionPair>,
    matched_ids: &mut HashSet<&'a str>,
    classify: impl Fn(&Memory, &Memory) -> Option<(ContradictionMatchType, f64)>,
) {
    if mems.len() < 2 {
        return;
    }
    for (i, a) in mems.iter().enumerate() {
        for b in &mems[i + 1..] {
            if matched_ids.contains(a.id.as_str()) && matched_ids.contains(b.id.as_str()) {
                continue;
            }
            if effective_object(a) == effective_object(b) {
                continue;
            }
            if let Some((match_type, similarity)) = classify(a, b) {
                contradictions.push(ContradictionPair {
                    a: (*a).clone(),
                    b: (*b).clone(),
                    match_type,
                    similarity,
                });
                matched_ids.insert(&a.id);
                matched_ids.insert(&b.id);
            }
        }
    }
}

fn find_alias_contradictions(
    by_subject: &HashMap<String, Vec<&Memory>>,
    entity_aliases: &HashMap<String, Vec<String>>,
    matched_ids: &HashSet<&str>,
    contradictions: &mut Vec<ContradictionPair>,
) {
    for (subj, mems) in by_subject {
        let Some(aliases) = entity_aliases.get(subj) else {
            continue;
        };
        let alias_mems: Vec<&Memory> = aliases
            .iter()
            .filter(|a| a.to_lowercase() != *subj)
            .filter_map(|a| by_subject.get(&a.to_lowercase()))
            .flatten()
            .copied()
            .collect();

        for mem in mems {
            if matched_ids.contains(mem.id.as_str()) {
                continue;
            }
            let pred_a = effective_predicate(mem);
            for other in &alias_mems {
                if other.id == mem.id || matched_ids.contains(other.id.as_str()) {
                    continue;
                }
                if pred_a != effective_predicate(other) {
                    continue;
                }
                if effective_object(mem) == effective_object(other) {
                    continue;
                }
                contradictions.push(ContradictionPair {
                    a: (*mem).clone(),
                    b: (*other).clone(),
                    match_type: ContradictionMatchType::SimilarSubject,
                    similarity: 1.0,
                });
            }
        }
    }
}

pub fn build_entity_alias_map(entities: &[ferrex_store::Entity]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for entity in entities {
        let name_lower = entity.name.to_lowercase();
        let mut all_names: Vec<String> = entity.aliases.iter().map(|a| a.to_lowercase()).collect();
        all_names.push(name_lower);
        for name in &all_names {
            map.entry(name.clone())
                .or_default()
                .extend(all_names.iter().cloned());
        }
    }
    for (key, aliases) in &mut map {
        aliases.sort();
        aliases.dedup();
        aliases.retain(|a| a != key);
    }
    map
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use assert_matches::assert_matches;
    use chrono::Utc;
    use ferrex_store::{Entity, MemoryType};
    use pretty_assertions::assert_eq;

    use super::*;

    fn triple(id: &str, subject: &str, predicate: &str, object: &str) -> Memory {
        let now = Utc::now();
        Memory {
            id: id.into(),
            namespace: "ns".into(),
            memory_type: MemoryType::Semantic,
            content: None,
            subject: Some(subject.into()),
            predicate: Some(predicate.into()),
            object: Some(object.into()),
            confidence: 1.0,
            source: None,
            context: None,
            entities: vec![],
            created_at: now,
            updated_at: now,
            t_valid: None,
            t_invalid: None,
            last_accessed: now,
            last_validated: None,
            access_count: 0,
            normalized_predicate: None,
        }
    }

    fn entity(name: &str, aliases: &[&str]) -> Entity {
        let now = Utc::now();
        Entity {
            id: name.into(),
            name: name.into(),
            aliases: aliases.iter().map(|a| (*a).to_string()).collect(),
            entity_type: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn ids(pair: &ContradictionPair) -> (&str, &str) {
        (pair.a.id.as_str(), pair.b.id.as_str())
    }

    #[test]
    fn exact_predicate_with_different_objects() {
        let mems = [
            triple("1", "API", "uses", "tokio"),
            triple("2", "api", "USES", "async-std"),
        ];
        let found = detect_contradictions(&mems, &HashMap::new());
        assert_eq!(found.len(), 1);
        assert_matches!(found[0].match_type, ContradictionMatchType::ExactPredicate);
        assert!((found[0].similarity - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn same_object_is_not_a_contradiction() {
        let mems = [
            triple("1", "api", "uses", "tokio"),
            triple("2", "api", "uses", "tokio"),
        ];
        assert!(detect_contradictions(&mems, &HashMap::new()).is_empty());
    }

    #[test]
    fn normalized_predicate_groups_memories() {
        let mut a = triple("1", "api", "depends on", "tokio");
        a.normalized_predicate = Some("uses".into());
        let mut b = triple("2", "api", "relies on", "smol");
        b.normalized_predicate = Some("uses".into());
        let found = detect_contradictions(&[a, b], &HashMap::new());
        assert_eq!(found.len(), 1);
        assert_matches!(found[0].match_type, ContradictionMatchType::ExactPredicate);
    }

    #[test]
    fn fuzzy_predicate_match() {
        let mems = [
            triple("1", "api", "deployed_on", "aws"),
            triple("2", "api", "deployed_to", "gcp"),
        ];
        let found = detect_contradictions(&mems, &HashMap::new());
        assert_eq!(found.len(), 1);
        assert_matches!(found[0].match_type, ContradictionMatchType::FuzzyPredicate);
        assert!(found[0].similarity > REFLECT_FUZZY_PREDICATE_THRESHOLD);
    }

    #[test]
    fn dissimilar_predicates_are_ignored() {
        let mems = [
            triple("1", "api", "uses", "tokio"),
            triple("2", "api", "owned_by", "team-x"),
        ];
        assert!(detect_contradictions(&mems, &HashMap::new()).is_empty());
    }

    #[test]
    fn memories_without_subject_or_predicate_are_skipped() {
        let mut no_subject = triple("1", "api", "uses", "tokio");
        no_subject.subject = None;
        let mut no_predicate = triple("2", "api", "uses", "smol");
        no_predicate.predicate = None;
        let other = triple("3", "api", "uses", "async-std");
        let found = detect_contradictions(&[no_subject, no_predicate, other], &HashMap::new());
        assert!(
            found
                .iter()
                .all(|p| ids(p) != ("1", "3") && ids(p) != ("3", "1"))
        );
    }

    #[test]
    fn already_matched_pairs_are_not_reported_twice() {
        let mems = [
            triple("1", "api", "uses", "tokio"),
            triple("2", "api", "uses", "smol"),
        ];
        // The exact pass matches them; the fuzzy pass must not re-report.
        assert_eq!(detect_contradictions(&mems, &HashMap::new()).len(), 1);
    }

    #[test]
    fn alias_subjects_contradict() {
        let mems = [
            triple("1", "postgres", "version", "15"),
            triple("2", "pg", "version", "16"),
        ];
        let aliases = build_entity_alias_map(&[entity("Postgres", &["PG"])]);
        let found = detect_contradictions(&mems, &aliases);
        assert_eq!(found.len(), 2, "reported from both alias directions");
        assert!(
            found
                .iter()
                .all(|p| matches!(p.match_type, ContradictionMatchType::SimilarSubject))
        );
    }

    #[test]
    fn alias_subjects_with_different_predicate_or_same_object_are_ignored() {
        let mems = [
            triple("1", "postgres", "version", "15"),
            triple("2", "pg", "port", "5432"),
            triple("3", "pg", "version", "15"),
        ];
        let aliases = build_entity_alias_map(&[entity("postgres", &["pg"])]);
        assert!(detect_contradictions(&mems, &aliases).is_empty());
    }

    #[test]
    fn alias_pass_skips_already_matched_memories() {
        let mems = [
            triple("1", "postgres", "version", "15"),
            triple("2", "postgres", "version", "16"),
            triple("3", "pg", "version", "17"),
        ];
        let aliases = build_entity_alias_map(&[entity("postgres", &["pg"])]);
        let found = detect_contradictions(&mems, &aliases);
        assert_eq!(found.len(), 1);
        assert_matches!(found[0].match_type, ContradictionMatchType::ExactPredicate);
    }

    #[test]
    fn alias_map_links_every_name_to_the_others() {
        let map = build_entity_alias_map(&[entity("Postgres", &["PG", "psql", "pg"])]);
        assert_eq!(map["postgres"], vec!["pg", "psql"]);
        assert_eq!(map["pg"], vec!["postgres", "psql"]);
        assert_eq!(map["psql"], vec!["pg", "postgres"]);
    }

    #[test]
    fn alias_map_empty_for_no_entities() {
        assert!(build_entity_alias_map(&[]).is_empty());
    }
}
