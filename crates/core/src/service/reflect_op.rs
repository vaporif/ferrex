use chrono::Utc;
use ferrex_store::{Memory, MemoryType, MetadataStore};

use super::{MAX_SCAN_MEMORIES, MemoryService};
use crate::error::CoreError;
use crate::reflect;
use crate::staleness::{freshness_label, staleness_score, threshold_for_type};
use crate::types::{ReflectRequest, ReflectResponse, ReflectSummary, StaleCandidate};

impl MemoryService {
    pub async fn reflect(&self, req: ReflectRequest) -> Result<ReflectResponse, CoreError> {
        let namespace = &req.namespace;
        let limit = req.limit.unwrap_or(reflect::DEFAULT_REFLECT_LIMIT);
        let staleness_config = &self.config.staleness;
        let now = Utc::now();

        let mut stale_candidates = Vec::new();
        let mut total_scanned: u64 = 0;

        if req.include_stale {
            #[allow(clippy::cast_possible_truncation)]
            let cutoff_days = (staleness_config.min_half_life_days() / 2.0) as i64;
            let cutoff = now - chrono::Duration::days(cutoff_days);

            let candidates = self
                .metadata_store
                .get_stale_candidates(namespace, cutoff)
                .await?;
            total_scanned = candidates.len() as u64;

            let mut scored: Vec<(Memory, f64, String)> = candidates
                .into_iter()
                .filter_map(|mem| {
                    let score = staleness_score(&mem, now, staleness_config);
                    let threshold = threshold_for_type(staleness_config, mem.memory_type);
                    if score >= threshold {
                        let reason = build_stale_reason(&mem, now);
                        Some((mem, score, reason))
                    } else {
                        None
                    }
                })
                .collect();

            scored.sort_by(|a, b| {
                let priority_a = eviction_priority(&a.0);
                let priority_b = eviction_priority(&b.0);
                priority_a.cmp(&priority_b).then(b.1.total_cmp(&a.1))
            });

            for (rank, (mem, score, reason)) in scored.into_iter().take(limit as usize).enumerate()
            {
                let threshold = threshold_for_type(staleness_config, mem.memory_type);
                let label = freshness_label(score, threshold);
                stale_candidates.push(StaleCandidate {
                    memory: mem,
                    staleness_score: score,
                    freshness_label: label,
                    #[allow(clippy::cast_possible_truncation)]
                    eviction_rank: rank as u32 + 1,
                    reason,
                });
            }
        }

        let contradictions = if req.include_contradictions {
            let mut all_ids = self.metadata_store.list_all_memory_ids(namespace).await?;
            if all_ids.len() > MAX_SCAN_MEMORIES {
                tracing::warn!(
                    total = all_ids.len(),
                    cap = MAX_SCAN_MEMORIES,
                    "reflect: truncating memory ID list to cap"
                );
                all_ids.truncate(MAX_SCAN_MEMORIES);
            }
            let all_memories = self.metadata_store.get_memories_by_ids(&all_ids).await?;
            let valid_semantics: Vec<Memory> = all_memories
                .into_iter()
                .filter(|m| m.memory_type == MemoryType::Semantic && m.t_invalid.is_none())
                .collect();
            total_scanned = total_scanned.max(valid_semantics.len() as u64);

            let entities = self.metadata_store.get_all_entities().await?;
            let alias_map = reflect::build_entity_alias_map(&entities);
            reflect::detect_contradictions(&valid_semantics, &alias_map)
        } else {
            Vec::new()
        };

        let stale_count = stale_candidates.len() as u64;
        let contradiction_count = contradictions.len() as u64;

        Ok(ReflectResponse {
            stale: stale_candidates,
            contradictions,
            summary: ReflectSummary {
                total_scanned,
                stale_count,
                contradiction_count,
            },
        })
    }
}

const fn eviction_priority(mem: &Memory) -> u8 {
    match mem.memory_type {
        MemoryType::Episodic if mem.access_count == 0 => 0,
        MemoryType::Episodic => 1,
        _ => 2,
    }
}

fn build_stale_reason(mem: &Memory, now: chrono::DateTime<Utc>) -> String {
    let age_days = (now - mem.created_at).num_days();
    let access_days = (now - mem.last_accessed).num_days();
    let validated_str = mem.last_validated.map_or_else(
        || "never validated".to_string(),
        |v| format!("last validated {} days ago", (now - v).num_days()),
    );
    format!(
        "created {} days ago, last accessed {} days ago, {}, {} accesses",
        age_days, access_days, validated_str, mem.access_count
    )
}
