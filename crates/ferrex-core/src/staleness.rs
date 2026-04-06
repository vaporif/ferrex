use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ferrex_store::{Memory, MemoryType};
use serde::Serialize;

pub const WEIGHT_AGE: f64 = 0.40;
pub const WEIGHT_ACCESS: f64 = 0.25;
pub const WEIGHT_VALIDATED: f64 = 0.25;
pub const WEIGHT_COUNT: f64 = 0.10;

pub const EPISODIC_HALF_LIFE: f64 = 30.0;
pub const SEMANTIC_HALF_LIFE: f64 = 180.0;
pub const PROCEDURAL_HALF_LIFE: f64 = 365.0;
const SECONDS_PER_DAY: f64 = 86400.0;
const FRESH_THRESHOLD_RATIO: f64 = 0.5;

pub const EPISODIC_STALE_THRESHOLD: f64 = 0.70;
pub const SEMANTIC_STALE_THRESHOLD: f64 = 0.80;
pub const PROCEDURAL_STALE_THRESHOLD: f64 = 0.90;

pub const COUNT_SCALE: f64 = 10.0;

#[derive(Debug, Clone)]
pub struct StalenessConfig {
    pub weights: StalenessWeights,
    pub count_scale: f64,
    pub type_config: HashMap<MemoryType, TypeStalenessConfig>,
}

#[derive(Debug, Clone)]
pub struct StalenessWeights {
    pub age: f64,
    pub access: f64,
    pub validated: f64,
    pub count: f64,
}

#[derive(Debug, Clone)]
pub struct TypeStalenessConfig {
    pub half_life_days: f64,
    pub stale_threshold: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FreshnessLabel {
    Fresh,
    Aging,
    Stale,
}

impl Default for StalenessConfig {
    fn default() -> Self {
        Self {
            weights: StalenessWeights {
                age: WEIGHT_AGE,
                access: WEIGHT_ACCESS,
                validated: WEIGHT_VALIDATED,
                count: WEIGHT_COUNT,
            },
            count_scale: COUNT_SCALE,
            type_config: HashMap::from([
                (
                    MemoryType::Episodic,
                    TypeStalenessConfig {
                        half_life_days: EPISODIC_HALF_LIFE,
                        stale_threshold: EPISODIC_STALE_THRESHOLD,
                    },
                ),
                (
                    MemoryType::Semantic,
                    TypeStalenessConfig {
                        half_life_days: SEMANTIC_HALF_LIFE,
                        stale_threshold: SEMANTIC_STALE_THRESHOLD,
                    },
                ),
                (
                    MemoryType::Procedural,
                    TypeStalenessConfig {
                        half_life_days: PROCEDURAL_HALF_LIFE,
                        stale_threshold: PROCEDURAL_STALE_THRESHOLD,
                    },
                ),
            ]),
        }
    }
}

impl StalenessConfig {
    #[must_use]
    pub fn min_half_life_days(&self) -> f64 {
        self.type_config
            .values()
            .map(|tc| tc.half_life_days)
            .fold(f64::INFINITY, f64::min)
    }
}

#[must_use]
#[allow(
    clippy::cast_precision_loss,  // seconds-to-f64 is fine for day-scale durations
    clippy::suboptimal_flops,     // readability over mul_add here
)]
pub fn staleness_score(memory: &Memory, now: DateTime<Utc>, config: &StalenessConfig) -> f64 {
    let type_cfg = config
        .type_config
        .get(&memory.memory_type)
        .expect("all MemoryType variants must be in type_config");

    let half_life = type_cfg.half_life_days;

    let to_days = |secs: i64| secs.max(0) as f64 / SECONDS_PER_DAY;
    let days_since_created = to_days((now - memory.created_at).num_seconds());
    let days_since_accessed = to_days((now - memory.last_accessed).num_seconds());
    let days_since_validated = memory
        .last_validated
        .map_or(days_since_created, |v| to_days((now - v).num_seconds()));

    let age_decay = 1.0 - (-f64::ln(2.0) * days_since_created / half_life).exp();
    let access_decay = 1.0 - (-f64::ln(2.0) * days_since_accessed / half_life).exp();
    let validation_decay = 1.0 - (-f64::ln(2.0) * days_since_validated / half_life).exp();
    let count_freshness = 1.0 / (1.0 + memory.access_count as f64 / config.count_scale);

    let w = &config.weights;
    (w.age * age_decay
        + w.access * access_decay
        + w.validated * validation_decay
        + w.count * count_freshness)
        .clamp(0.0, 1.0)
}

#[must_use]
pub fn freshness_label(score: f64, threshold: f64) -> FreshnessLabel {
    if score < threshold * FRESH_THRESHOLD_RATIO {
        FreshnessLabel::Fresh
    } else if score < threshold {
        FreshnessLabel::Aging
    } else {
        FreshnessLabel::Stale
    }
}

#[must_use]
pub fn threshold_for_type(config: &StalenessConfig, memory_type: MemoryType) -> f64 {
    config
        .type_config
        .get(&memory_type)
        .expect("all MemoryType variants must be in type_config")
        .stale_threshold
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn make_memory(
        memory_type: MemoryType,
        created_days_ago: i64,
        accessed_days_ago: i64,
        validated_days_ago: Option<i64>,
        access_count: u64,
    ) -> (Memory, DateTime<Utc>) {
        let now = Utc::now();
        let created = now - Duration::days(created_days_ago);
        let accessed = now - Duration::days(accessed_days_ago);
        let validated = validated_days_ago.map(|d| now - Duration::days(d));
        let mem = Memory {
            id: "test-id".into(),
            namespace: "test".into(),
            memory_type,
            content: Some("test content".into()),
            subject: None,
            predicate: None,
            object: None,
            confidence: 1.0,
            source: None,
            context: None,
            entities: vec![],
            created_at: created,
            updated_at: created,
            t_valid: None,
            t_invalid: None,
            last_accessed: accessed,
            last_validated: validated,
            access_count,
            normalized_predicate: None,
        };
        (mem, now)
    }

    #[test]
    fn test_brand_new_episodic_is_fresh() {
        let (mem, now) = make_memory(MemoryType::Episodic, 0, 0, Some(0), 5);
        let config = StalenessConfig::default();
        let score = staleness_score(&mem, now, &config);
        assert!(
            score < 0.1,
            "brand new memory should have low staleness, got {score}"
        );
    }

    #[test]
    fn test_old_unaccessed_episodic_is_stale() {
        let (mem, now) = make_memory(MemoryType::Episodic, 90, 90, None, 0);
        let config = StalenessConfig::default();
        let score = staleness_score(&mem, now, &config);
        assert!(
            score >= EPISODIC_STALE_THRESHOLD,
            "90-day old unaccessed episodic should be stale, got {score}"
        );
    }

    #[test]
    fn test_old_but_frequently_accessed_scores_lower() {
        let (mem_unused, now) = make_memory(MemoryType::Episodic, 60, 60, None, 0);
        let (mem_used, _) = make_memory(MemoryType::Episodic, 60, 1, Some(1), 20);
        let config = StalenessConfig::default();
        let score_unused = staleness_score(&mem_unused, now, &config);
        let score_used = staleness_score(&mem_used, now, &config);
        assert!(
            score_used < score_unused,
            "frequently accessed memory should be less stale: used={score_used} unused={score_unused}"
        );
    }

    #[test]
    fn test_semantic_decays_slower_than_episodic() {
        let (ep, now) = make_memory(MemoryType::Episodic, 60, 60, None, 0);
        let (sem, _) = make_memory(MemoryType::Semantic, 60, 60, None, 0);
        let config = StalenessConfig::default();
        let ep_score = staleness_score(&ep, now, &config);
        let sem_score = staleness_score(&sem, now, &config);
        assert!(
            sem_score < ep_score,
            "semantic should decay slower: sem={sem_score} ep={ep_score}"
        );
    }

    #[test]
    fn test_procedural_decays_slowest() {
        let (proc_mem, now) = make_memory(MemoryType::Procedural, 180, 180, None, 0);
        let config = StalenessConfig::default();
        let score = staleness_score(&proc_mem, now, &config);
        assert!(
            score < PROCEDURAL_STALE_THRESHOLD,
            "180-day procedural should not yet be stale (threshold {PROCEDURAL_STALE_THRESHOLD}), got {score}"
        );
    }

    #[test]
    fn test_never_validated_accumulates_from_creation() {
        let (validated, now) = make_memory(MemoryType::Episodic, 30, 0, Some(0), 0);
        let (unvalidated, _) = make_memory(MemoryType::Episodic, 30, 0, None, 0);
        let config = StalenessConfig::default();
        let v_score = staleness_score(&validated, now, &config);
        let u_score = staleness_score(&unvalidated, now, &config);
        assert!(
            u_score > v_score,
            "never-validated should be staler: unvalidated={u_score} validated={v_score}"
        );
    }

    #[test]
    fn test_freshness_label_fresh() {
        assert_eq!(freshness_label(0.2, 0.7), FreshnessLabel::Fresh);
    }

    #[test]
    fn test_freshness_label_aging() {
        assert_eq!(freshness_label(0.5, 0.7), FreshnessLabel::Aging);
    }

    #[test]
    fn test_freshness_label_stale() {
        assert_eq!(freshness_label(0.8, 0.7), FreshnessLabel::Stale);
    }

    #[test]
    fn test_count_freshness_high_access_reduces_staleness() {
        let (mem_low, now) = make_memory(MemoryType::Episodic, 30, 30, None, 0);
        let (mem_high, _) = make_memory(MemoryType::Episodic, 30, 30, None, 50);
        let config = StalenessConfig::default();
        let low_score = staleness_score(&mem_low, now, &config);
        let high_score = staleness_score(&mem_high, now, &config);
        assert!(
            high_score < low_score,
            "high access count should reduce staleness: high={high_score} low={low_score}"
        );
    }
}
