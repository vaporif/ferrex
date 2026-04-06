use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::types::{
    ConflictConfig, DedupConfig, NamespacePredicatesConfig, PredicatesConfig, ReconciliationConfig,
};

#[derive(Debug, Deserialize, Default)]
pub struct FileConfig {
    #[serde(default)]
    pub deduplication: DedupFile,
    #[serde(default)]
    pub conflict: ConflictFile,
    #[serde(default)]
    pub predicates: PredicatesFile,
    #[serde(default)]
    pub namespaces: HashMap<String, NamespaceFile>,
    #[serde(default)]
    pub reconciliation: ReconciliationFile,
    #[serde(default)]
    pub reader_pool_size: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DedupFile {
    pub threshold: Option<f32>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConflictFile {
    pub object_fuzzy_duplicate: Option<f32>,
    pub object_fuzzy_update: Option<f32>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PredicatesFile {
    #[serde(default)]
    pub groups: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct NamespaceFile {
    #[serde(default)]
    pub predicates: PredicatesFile,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReconciliationFile {
    pub audit_interval_hours: Option<u64>,
    pub audit_fix_limit: Option<u64>,
}

pub struct LoadedConfig {
    pub deduplication: DedupConfig,
    pub conflict: ConflictConfig,
    pub predicates: PredicatesConfig,
    pub reconciliation: ReconciliationConfig,
    pub reader_pool_size: usize,
}

pub const DEFAULT_READER_POOL_SIZE: usize = 4;

/// Load config from `path`, writing the embedded baseline on first run.
pub fn load_or_init(path: &Path, baseline: &str) -> Result<LoadedConfig, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
            }
            std::fs::write(path, baseline).map_err(ConfigError::Io)?;
            baseline.to_string()
        }
        Err(e) => return Err(ConfigError::Io(e)),
    };
    let file: FileConfig = toml::from_str(&raw).map_err(|e| ConfigError::Parse(e.to_string()))?;
    Ok(resolve(file))
}

pub fn resolve(file: FileConfig) -> LoadedConfig {
    let deduplication = DedupConfig {
        threshold: file.deduplication.threshold.unwrap_or(0.95),
    };
    let conflict = ConflictConfig {
        object_fuzzy_duplicate: file.conflict.object_fuzzy_duplicate.unwrap_or(0.95),
        object_fuzzy_update: file.conflict.object_fuzzy_update.unwrap_or(0.50),
    };
    let namespaces = file
        .namespaces
        .into_iter()
        .map(|(name, nsf)| {
            (
                name,
                NamespacePredicatesConfig {
                    groups: nsf.predicates.groups,
                },
            )
        })
        .collect();
    let predicates = PredicatesConfig {
        groups: file.predicates.groups,
        namespaces,
    };
    LoadedConfig {
        deduplication,
        conflict,
        predicates,
        reconciliation: ReconciliationConfig {
            audit_interval_hours: file.reconciliation.audit_interval_hours,
            audit_fix_limit: file.reconciliation.audit_fix_limit.unwrap_or(1000),
        },
        reader_pool_size: file.reader_pool_size.unwrap_or(DEFAULT_READER_POOL_SIZE),
    }
}

/// Merge global predicate groups with namespace-specific overrides.
pub fn resolve_namespace_groups(
    predicates: &PredicatesConfig,
    namespace: &str,
) -> HashMap<String, Vec<String>> {
    let mut merged = predicates.groups.clone();
    if let Some(ns) = predicates.namespaces.get(namespace) {
        for (key, members) in &ns.groups {
            if members.is_empty() {
                merged.remove(key);
            } else {
                merged.insert(key.clone(), members.clone());
            }
        }
    }
    merged
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config io: {0}")]
    Io(std::io::Error),
    #[error("config parse: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_defaults() {
        let cfg = resolve(FileConfig::default());
        assert!((cfg.deduplication.threshold - 0.95).abs() < f32::EPSILON);
        assert!((cfg.conflict.object_fuzzy_duplicate - 0.95).abs() < f32::EPSILON);
        assert!((cfg.conflict.object_fuzzy_update - 0.50).abs() < f32::EPSILON);
        assert_eq!(cfg.reader_pool_size, DEFAULT_READER_POOL_SIZE);
    }

    #[test]
    fn test_namespace_merge_replaces_and_adds() {
        let raw = r#"
[predicates.groups]
depends_on = ["uses", "requires"]
has_version = ["version"]

[namespaces.legal.predicates.groups]
cites = ["references"]
depends_on = ["rests-on"]
"#;
        let file: FileConfig = toml::from_str(raw).unwrap();
        let cfg = resolve(file);
        let legal = resolve_namespace_groups(&cfg.predicates, "legal");
        assert_eq!(legal.get("cites").unwrap(), &vec!["references".to_string()]);
        assert_eq!(
            legal.get("depends_on").unwrap(),
            &vec!["rests-on".to_string()]
        );
        assert!(legal.contains_key("has_version"));
    }

    #[test]
    fn test_namespace_empty_group_suppresses_global() {
        let raw = r#"
[predicates.groups]
depends_on = ["uses"]
runs_on = ["deployed-to"]

[namespaces.art.predicates.groups]
runs_on = []
"#;
        let file: FileConfig = toml::from_str(raw).unwrap();
        let cfg = resolve(file);
        let art = resolve_namespace_groups(&cfg.predicates, "art");
        assert!(art.contains_key("depends_on"));
        assert!(!art.contains_key("runs_on"));
    }

    #[test]
    fn test_missing_file_writes_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("ferrex.toml");
        let baseline = "[deduplication]\nthreshold = 0.9\n";
        let cfg = load_or_init(&path, baseline).unwrap();
        assert!((cfg.deduplication.threshold - 0.9).abs() < f32::EPSILON);
        assert!(path.exists());
    }
}
