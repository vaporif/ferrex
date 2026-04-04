use ferrex_store::MemoryType;

const EPISODIC_BOOST_WEIGHT: f64 = 0.3;
const EPISODIC_HALF_LIFE_DAYS: f64 = 30.0;
const SEMANTIC_BOOST_WEIGHT: f64 = 0.15;
const SEMANTIC_HALF_LIFE_DAYS: f64 = 180.0;
pub const SECONDS_PER_DAY: f64 = 86_400.0;

pub fn compute_recency_boost(memory_type: MemoryType, age_days: f64) -> f64 {
    let age_days = age_days.max(0.0);
    match memory_type {
        MemoryType::Episodic => {
            EPISODIC_BOOST_WEIGHT.mul_add((-age_days / EPISODIC_HALF_LIFE_DAYS).exp2(), 1.0)
        }
        MemoryType::Semantic => {
            SEMANTIC_BOOST_WEIGHT.mul_add((-age_days / SEMANTIC_HALF_LIFE_DAYS).exp2(), 1.0)
        }
        MemoryType::Procedural => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_episodic_boost_at_zero() {
        let boost = compute_recency_boost(MemoryType::Episodic, 0.0);
        assert!((boost - 1.3).abs() < 0.001, "expected 1.3, got {boost}");
    }

    #[test]
    fn test_episodic_boost_at_half_life() {
        let boost = compute_recency_boost(MemoryType::Episodic, 30.0);
        assert!((boost - 1.15).abs() < 0.001, "expected ~1.15, got {boost}");
    }

    #[test]
    fn test_episodic_boost_decays_toward_one() {
        let boost = compute_recency_boost(MemoryType::Episodic, 300.0);
        assert!(boost > 1.0 && boost < 1.01, "expected ~1.0, got {boost}");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_semantic_boost_at_zero() {
        let boost = compute_recency_boost(MemoryType::Semantic, 0.0);
        assert!((boost - 1.15).abs() < 0.001, "expected 1.15, got {boost}");
    }

    #[test]
    fn test_semantic_boost_at_half_life() {
        let boost = compute_recency_boost(MemoryType::Semantic, 180.0);
        assert!(
            (boost - 1.075).abs() < 0.001,
            "expected ~1.075, got {boost}"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_procedural_no_boost() {
        let boost = compute_recency_boost(MemoryType::Procedural, 0.0);
        assert_eq!(boost, 1.0);
        let boost = compute_recency_boost(MemoryType::Procedural, 365.0);
        assert_eq!(boost, 1.0);
    }
}
