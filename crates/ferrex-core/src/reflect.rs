use std::collections::{HashMap, HashSet};

use ferrex_store::Memory;

use crate::types::{ContradictionMatchType, ContradictionPair};

pub const REFLECT_FUZZY_PREDICATE_THRESHOLD: f64 = 0.75;
pub const DEFAULT_REFLECT_LIMIT: u32 = 20;

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

    // Exact predicate matches with different objects
    for mems in grouped.values() {
        if mems.len() < 2 {
            continue;
        }
        for i in 0..mems.len() {
            for j in (i + 1)..mems.len() {
                let obj_a = mems[i].object.as_deref().unwrap_or("");
                let obj_b = mems[j].object.as_deref().unwrap_or("");
                if obj_a != obj_b {
                    contradictions.push(ContradictionPair {
                        a: mems[i].clone(),
                        b: mems[j].clone(),
                        match_type: ContradictionMatchType::ExactPredicate,
                        similarity: 1.0,
                    });
                    matched_ids.insert(&mems[i].id);
                    matched_ids.insert(&mems[j].id);
                }
            }
        }
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

    // Fuzzy predicate matches
    for mems in by_subject.values() {
        if mems.len() < 2 {
            continue;
        }
        for i in 0..mems.len() {
            for j in (i + 1)..mems.len() {
                if matched_ids.contains(mems[i].id.as_str())
                    && matched_ids.contains(mems[j].id.as_str())
                {
                    continue;
                }
                let pred_a = mems[i]
                    .normalized_predicate
                    .as_deref()
                    .or(mems[i].predicate.as_deref())
                    .unwrap_or("");
                let pred_b = mems[j]
                    .normalized_predicate
                    .as_deref()
                    .or(mems[j].predicate.as_deref())
                    .unwrap_or("");
                if pred_a == pred_b {
                    continue;
                }
                let sim = strsim::jaro_winkler(pred_a, pred_b);
                if sim > REFLECT_FUZZY_PREDICATE_THRESHOLD {
                    let obj_a = mems[i].object.as_deref().unwrap_or("");
                    let obj_b = mems[j].object.as_deref().unwrap_or("");
                    if obj_a != obj_b {
                        contradictions.push(ContradictionPair {
                            a: mems[i].clone(),
                            b: mems[j].clone(),
                            match_type: ContradictionMatchType::FuzzyPredicate,
                            similarity: sim,
                        });
                        matched_ids.insert(&mems[i].id);
                        matched_ids.insert(&mems[j].id);
                    }
                }
            }
        }
    }

    // Entity alias-based matches
    for mems in by_subject.values() {
        for mem in mems {
            if matched_ids.contains(mem.id.as_str()) {
                continue;
            }
            let subj = match mem.subject.as_deref() {
                Some(s) => s.to_lowercase(),
                None => continue,
            };
            if let Some(aliases) = entity_aliases.get(&subj) {
                for alias in aliases {
                    let alias_lower = alias.to_lowercase();
                    if alias_lower == subj {
                        continue;
                    }
                    if let Some(alias_mems) = by_subject.get(&alias_lower) {
                        for other in alias_mems {
                            if other.id == mem.id || matched_ids.contains(other.id.as_str()) {
                                continue;
                            }
                            let pred_a = mem
                                .normalized_predicate
                                .as_deref()
                                .or(mem.predicate.as_deref())
                                .unwrap_or("");
                            let pred_b = other
                                .normalized_predicate
                                .as_deref()
                                .or(other.predicate.as_deref())
                                .unwrap_or("");
                            if pred_a == pred_b {
                                let obj_a = mem.object.as_deref().unwrap_or("");
                                let obj_b = other.object.as_deref().unwrap_or("");
                                if obj_a != obj_b {
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
                }
            }
        }
    }

    contradictions
}

pub fn build_entity_alias_map(entities: &[ferrex_store::Entity]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for entity in entities {
        let name_lower = entity.name.to_lowercase();
        let mut all_names: Vec<String> = entity.aliases.iter().map(|a| a.to_lowercase()).collect();
        all_names.push(name_lower.clone());
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
