use std::collections::HashMap;

use chrono::Utc;
use ferrex_store::{Memory, MetadataStore, QdrantSidecar};

use super::{MAX_SCAN_MEMORIES, MemoryService, STATS_RECENT_COUNT};
use crate::error::CoreError;
use crate::staleness::{FreshnessLabel, freshness_label, staleness_score, threshold_for_type};
use crate::types::{
    DiagnosticsReport, NeedsAttention, StalenessDistribution, StatsDetails, StatsRequest,
    StatsResponse, TypeStats,
};

impl MemoryService {
    #[allow(clippy::cast_precision_loss)]
    pub async fn stats(&self, req: StatsRequest) -> Result<StatsResponse, CoreError> {
        let namespace = &req.namespace;
        let mut all_ids = self.metadata_store.list_all_memory_ids(namespace).await?;
        if all_ids.len() > MAX_SCAN_MEMORIES {
            tracing::warn!(
                total = all_ids.len(),
                cap = MAX_SCAN_MEMORIES,
                "stats: truncating memory ID list to cap"
            );
            all_ids.truncate(MAX_SCAN_MEMORIES);
        }
        let total = all_ids.len() as u64;
        let recent = self
            .metadata_store
            .recent_memories(namespace, STATS_RECENT_COUNT)
            .await?;

        let staleness_config = &self.config.staleness;
        let now = Utc::now();

        let unvalidated = self
            .metadata_store
            .get_unvalidated_memories(namespace)
            .await?;
        let unvalidated_count = unvalidated.len() as u64;

        #[allow(clippy::cast_possible_truncation)]
        let cutoff =
            now - chrono::Duration::days((staleness_config.min_half_life_days() / 2.0) as i64);
        let stale_candidates = self
            .metadata_store
            .get_stale_candidates(namespace, cutoff)
            .await?;
        let stale_count = stale_candidates
            .iter()
            .filter(|m| {
                let score = staleness_score(m, now, staleness_config);
                let threshold = threshold_for_type(staleness_config, m.memory_type);
                score >= threshold
            })
            .count() as u64;

        let needs_attention = NeedsAttention {
            stale_count,
            conflict_count: 0,
            unvalidated_count,
        };

        let details = if req.detailed.unwrap_or(false) {
            Some(
                self.build_stats_details(namespace, &all_ids, staleness_config, now)
                    .await?,
            )
        } else {
            None
        };

        let diagnostics = if req.diagnostics.unwrap_or(false) {
            Some(self.diagnostics().await?)
        } else {
            None
        };

        Ok(StatsResponse {
            total_memories: total,
            recent_memories: recent,
            needs_attention,
            details,
            diagnostics,
        })
    }

    #[allow(clippy::cast_precision_loss)]
    async fn build_stats_details(
        &self,
        namespace: &str,
        all_ids: &[String],
        staleness_config: &crate::staleness::StalenessConfig,
        now: chrono::DateTime<Utc>,
    ) -> Result<StatsDetails, CoreError> {
        struct Acc {
            count: u64,
            stale_count: u64,
            sum_staleness: f64,
            sum_access: f64,
            oldest: Option<chrono::DateTime<Utc>>,
            newest: Option<chrono::DateTime<Utc>>,
        }

        let storage_size_bytes = self.metadata_store.storage_size_bytes().await?;
        let entity_count = self.metadata_store.entity_count(namespace).await?;

        let all_memories = self.metadata_store.get_memories_by_ids(all_ids).await?;
        let valid_memories: Vec<&Memory> = all_memories
            .iter()
            .filter(|m| m.t_invalid.is_none())
            .collect();

        let mut dist = StalenessDistribution {
            fresh: 0,
            aging: 0,
            stale: 0,
        };
        let mut type_acc: HashMap<String, Acc> = HashMap::new();

        for mem in &valid_memories {
            let score = staleness_score(mem, now, staleness_config);
            let threshold = threshold_for_type(staleness_config, mem.memory_type);
            let label = freshness_label(score, threshold);

            match label {
                FreshnessLabel::Fresh => dist.fresh += 1,
                FreshnessLabel::Aging => dist.aging += 1,
                FreshnessLabel::Stale => dist.stale += 1,
            }

            let key = mem.memory_type.as_str().to_string();
            let acc = type_acc.entry(key).or_insert(Acc {
                count: 0,
                stale_count: 0,
                sum_staleness: 0.0,
                sum_access: 0.0,
                oldest: None,
                newest: None,
            });
            acc.count += 1;
            if label == FreshnessLabel::Stale {
                acc.stale_count += 1;
            }
            acc.sum_staleness += score;
            acc.sum_access += mem.access_count as f64;
            acc.oldest = Some(acc.oldest.map_or(mem.created_at, |o| o.min(mem.created_at)));
            acc.newest = Some(acc.newest.map_or(mem.created_at, |n| n.max(mem.created_at)));
        }

        let by_type: HashMap<String, TypeStats> = type_acc
            .into_iter()
            .map(|(key, a)| {
                let count_f = a.count as f64;
                (
                    key,
                    TypeStats {
                        count: a.count,
                        stale_count: a.stale_count,
                        avg_staleness: if a.count > 0 {
                            a.sum_staleness / count_f
                        } else {
                            0.0
                        },
                        avg_access_count: if a.count > 0 {
                            a.sum_access / count_f
                        } else {
                            0.0
                        },
                        oldest: a.oldest,
                        newest: a.newest,
                    },
                )
            })
            .collect();

        Ok(StatsDetails {
            by_type,
            storage_size_bytes,
            entity_count,
            staleness_distribution: dist,
        })
    }

    pub async fn diagnostics(&self) -> Result<DiagnosticsReport, CoreError> {
        let namespace = &self.config.namespace;
        let memory_count = self.metadata_store.memory_count().await?;
        let entity_count = self.metadata_store.entity_count(namespace).await?;
        let sqlite_size = self.metadata_store.storage_size_bytes().await?;
        let pending = self
            .metadata_store
            .count_pending_ops_older_than(std::time::Duration::ZERO)
            .await?;
        let cache = self.cache.cache_stats().await;
        let recent_ops = self.ops_buffer.recent(10);

        let qdrant_url = self
            .config
            .qdrant_url
            .clone()
            .unwrap_or_else(|| format!("localhost:{} (sidecar)", self.config.qdrant_port));

        let qdrant_pid = self.sidecar.as_ref().and_then(QdrantSidecar::pid);

        Ok(DiagnosticsReport {
            version: env!("CARGO_PKG_VERSION").to_string(),
            embedding_model: self.config.model_tier.model_name().to_string(),
            qdrant_url,
            qdrant_pid,
            collection: namespace.clone(),
            sqlite_path: self.config.db_path.display().to_string(),
            sqlite_size_bytes: sqlite_size,
            memory_count,
            entity_count,
            pending_ops: pending,
            cache,
            recent_ops,
        })
    }
}
