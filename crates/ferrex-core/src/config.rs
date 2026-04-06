use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::staleness::{StalenessConfig, StalenessWeights};
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
    pub staleness: StalenessFile,
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

#[derive(Debug, Deserialize, Default)]
pub struct StalenessFile {
    pub weight_age: Option<f64>,
    pub weight_access: Option<f64>,
    pub weight_validated: Option<f64>,
    pub weight_count: Option<f64>,
    pub count_scale: Option<f64>,
    pub episodic: Option<TypeStalenessFile>,
    pub semantic: Option<TypeStalenessFile>,
    pub procedural: Option<TypeStalenessFile>,
}

#[derive(Debug, Deserialize, Default)]
pub struct TypeStalenessFile {
    pub half_life_days: Option<f64>,
    pub stale_threshold: Option<f64>,
}

pub struct LoadedConfig {
    pub deduplication: DedupConfig,
    pub conflict: ConflictConfig,
    pub predicates: PredicatesConfig,
    pub reconciliation: ReconciliationConfig,
    pub staleness: StalenessConfig,
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
    let default_staleness = StalenessConfig::default();
    let staleness = resolve_staleness(&file.staleness, &default_staleness);

    LoadedConfig {
        deduplication,
        conflict,
        predicates,
        reconciliation: ReconciliationConfig {
            audit_interval_hours: file.reconciliation.audit_interval_hours,
            audit_fix_limit: file.reconciliation.audit_fix_limit.unwrap_or(1000),
        },
        staleness,
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

fn resolve_staleness(file: &StalenessFile, defaults: &StalenessConfig) -> StalenessConfig {
    let weights = StalenessWeights {
        age: file.weight_age.unwrap_or(defaults.weights.age),
        access: file.weight_access.unwrap_or(defaults.weights.access),
        validated: file.weight_validated.unwrap_or(defaults.weights.validated),
        count: file.weight_count.unwrap_or(defaults.weights.count),
    };
    let count_scale = file.count_scale.unwrap_or(defaults.count_scale);

    let mut type_config = defaults.type_config.clone();

    if let Some(ref ep) = file.episodic {
        let entry = type_config
            .get_mut(&ferrex_store::MemoryType::Episodic)
            .expect("default type_config must contain Episodic");
        if let Some(v) = ep.half_life_days {
            entry.half_life_days = v;
        }
        if let Some(v) = ep.stale_threshold {
            entry.stale_threshold = v;
        }
    }
    if let Some(ref sem) = file.semantic {
        let entry = type_config
            .get_mut(&ferrex_store::MemoryType::Semantic)
            .expect("default type_config must contain Semantic");
        if let Some(v) = sem.half_life_days {
            entry.half_life_days = v;
        }
        if let Some(v) = sem.stale_threshold {
            entry.stale_threshold = v;
        }
    }
    if let Some(ref proc) = file.procedural {
        let entry = type_config
            .get_mut(&ferrex_store::MemoryType::Procedural)
            .expect("default type_config must contain Procedural");
        if let Some(v) = proc.half_life_days {
            entry.half_life_days = v;
        }
        if let Some(v) = proc.stale_threshold {
            entry.stale_threshold = v;
        }
    }

    StalenessConfig {
        weights,
        count_scale,
        type_config,
    }
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
    fn test_staleness_defaults() {
        let cfg = resolve(FileConfig::default());
        let sc = &cfg.staleness;
        assert!((sc.weights.age - 0.40).abs() < f64::EPSILON);
        assert!((sc.weights.access - 0.25).abs() < f64::EPSILON);
        assert!((sc.weights.validated - 0.25).abs() < f64::EPSILON);
        assert!((sc.weights.count - 0.10).abs() < f64::EPSILON);
        assert!((sc.count_scale - 10.0).abs() < f64::EPSILON);

        let ep = sc
            .type_config
            .get(&ferrex_store::MemoryType::Episodic)
            .unwrap();
        assert!((ep.half_life_days - 30.0).abs() < f64::EPSILON);
        assert!((ep.stale_threshold - 0.70).abs() < f64::EPSILON);

        let sem = sc
            .type_config
            .get(&ferrex_store::MemoryType::Semantic)
            .unwrap();
        assert!((sem.half_life_days - 180.0).abs() < f64::EPSILON);

        let proc = sc
            .type_config
            .get(&ferrex_store::MemoryType::Procedural)
            .unwrap();
        assert!((proc.half_life_days - 365.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_staleness_override_from_toml() {
        let raw = r#"
[staleness]
weight_age = 0.50
weight_access = 0.20
weight_validated = 0.20
weight_count = 0.10
count_scale = 5.0

[staleness.episodic]
half_life_days = 14
stale_threshold = 0.60
"#;
        let file: FileConfig = toml::from_str(raw).unwrap();
        let cfg = resolve(file);
        assert!((cfg.staleness.weights.age - 0.50).abs() < f64::EPSILON);
        assert!((cfg.staleness.count_scale - 5.0).abs() < f64::EPSILON);
        let ep = cfg
            .staleness
            .type_config
            .get(&ferrex_store::MemoryType::Episodic)
            .unwrap();
        assert!((ep.half_life_days - 14.0).abs() < f64::EPSILON);
        assert!((ep.stale_threshold - 0.60).abs() < f64::EPSILON);
        // Semantic should still be default
        let sem = cfg
            .staleness
            .type_config
            .get(&ferrex_store::MemoryType::Semantic)
            .unwrap();
        assert!((sem.half_life_days - 180.0).abs() < f64::EPSILON);
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
