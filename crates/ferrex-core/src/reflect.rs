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

fn find_alias_contradictions<'a>(
    by_subject: &HashMap<String, Vec<&'a Memory>>,
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
            let others: Vec<String> = all_names.iter().filter(|n| *n != name).cloned().collect();
            map.entry(name.clone()).or_default().extend(others);
        }
    }
    for aliases in map.values_mut() {
        aliases.sort();
        aliases.dedup();
    }
    map
}
