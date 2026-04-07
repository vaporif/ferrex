use std::collections::HashMap;

const FUZZY_THRESHOLD: f64 = 0.85;

struct SynonymEntry {
    member: String,
    canonical: String,
}

/// Collapses free-form predicates (typos, casing, separators) into canonical
/// group names so that e.g. `uses`, `depends-on`, and `requires` all resolve
/// to `depends_on`.
pub struct PredicateNormalizer {
    synonyms: Vec<SynonymEntry>,
    lookup: HashMap<String, String>,
}

impl PredicateNormalizer {
    pub fn new(groups: HashMap<String, Vec<String>>) -> Self {
        let mut synonyms = Vec::new();
        let mut lookup: HashMap<String, String> = HashMap::new();
        let mut sorted_groups: Vec<_> = groups.into_iter().collect();
        sorted_groups.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (canonical, members) in sorted_groups {
            let normalized_members: Vec<String> = std::iter::once(canonical.as_str())
                .chain(members.iter().map(String::as_str))
                .map(Self::text_normalize)
                .collect();
            for nm in &normalized_members {
                if let Some(existing) = lookup.get(nm) {
                    tracing::warn!(
                        synonym = %nm,
                        existing_group = %existing,
                        new_group = %canonical,
                        "ambiguous predicate config: synonym belongs to multiple groups, keeping first"
                    );
                    continue;
                }
                lookup.insert(nm.clone(), canonical.clone());
                synonyms.push(SynonymEntry {
                    member: nm.clone(),
                    canonical: canonical.clone(),
                });
            }
        }
        Self { synonyms, lookup }
    }

    pub fn text_normalize(s: &str) -> String {
        s.trim()
            .to_lowercase()
            .replace(['-', '_', '/'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn normalize(&self, input: &str) -> String {
        let text = Self::text_normalize(input);
        if text.is_empty() {
            return text;
        }
        if let Some(canonical) = self.lookup.get(&text) {
            return canonical.clone();
        }
        let mut best: Option<(&str, f64)> = None;
        for entry in &self.synonyms {
            let score = strsim::jaro_winkler(&text, &entry.member);
            if score > FUZZY_THRESHOLD && best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((entry.canonical.as_str(), score));
            }
        }
        match best {
            Some((canonical, _)) => canonical.to_string(),
            None => text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_groups() -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert(
            "depends_on".into(),
            vec!["uses".into(), "depends-on".into(), "requires".into()],
        );
        m.insert(
            "has_version".into(),
            vec!["version".into(), "pinned-to".into()],
        );
        m
    }

    #[test]
    fn test_text_normalize() {
        assert_eq!(PredicateNormalizer::text_normalize("  Uses  "), "uses");
        assert_eq!(
            PredicateNormalizer::text_normalize("depends-on"),
            "depends on"
        );
        assert_eq!(
            PredicateNormalizer::text_normalize("HAS_version"),
            "has version"
        );
    }

    #[test]
    fn test_exact_group_member_match() {
        let n = PredicateNormalizer::new(sample_groups());
        assert_eq!(n.normalize("uses"), "depends_on");
        assert_eq!(n.normalize("REQUIRES"), "depends_on");
        assert_eq!(n.normalize("depends-on"), "depends_on");
    }

    #[test]
    fn test_group_key_match() {
        let n = PredicateNormalizer::new(sample_groups());
        assert_eq!(n.normalize("depends_on"), "depends_on");
        assert_eq!(n.normalize("has version"), "has_version");
    }

    #[test]
    fn test_fuzzy_fallback() {
        let n = PredicateNormalizer::new(sample_groups());
        assert_eq!(n.normalize("requies"), "depends_on");
    }

    #[test]
    fn test_passthrough_unknown() {
        let n = PredicateNormalizer::new(sample_groups());
        assert_eq!(n.normalize("totally-unrelated"), "totally unrelated");
    }

    #[test]
    fn test_empty_input() {
        let n = PredicateNormalizer::new(sample_groups());
        assert_eq!(n.normalize(""), "");
        assert_eq!(n.normalize("   "), "");
    }
}
