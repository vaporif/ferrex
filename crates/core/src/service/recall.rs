use std::collections::HashMap;

use chrono::Utc;
use ferrex_store::{MemoryType, MetadataStore};
use qdrant_client::qdrant::{Condition, DatetimeRange, Filter, Timestamp};
use uuid::Uuid;

use super::{DEFAULT_RECALL_LIMIT, MAX_RECALL_LIMIT, MIN_RERANK_POOL_SIZE, MemoryService};
use crate::cache;
use crate::error::CoreError;
use crate::retrieval::{SECONDS_PER_DAY, compute_recency_boost};
use crate::staleness::{FreshnessLabel, freshness_label, staleness_score, threshold_for_type};
use crate::types::{RecallRequest, RecallResult, ScoringBreakdown};

impl MemoryService {
    #[tracing::instrument(name = "recall", skip_all, fields(query = %req.query, namespace))]
    pub async fn recall(&self, req: RecallRequest) -> Result<Vec<RecallResult>, CoreError> {
        let op_start = std::time::Instant::now();
        let namespace = req.namespace.as_deref().unwrap_or(&self.config.namespace);
        tracing::Span::current().record("namespace", namespace);
        let limit = req
            .limit
            .unwrap_or(DEFAULT_RECALL_LIMIT)
            .min(MAX_RECALL_LIMIT);
        let candidate_pool_size = if req.include_stale == Some(false) {
            limit.max(MIN_RERANK_POOL_SIZE) * 2
        } else {
            limit.max(MIN_RERANK_POOL_SIZE)
        };

        let embedding = if let Some(cached) = self.cache.get_embedding(&req.query).await {
            cached
        } else {
            let emb = self.embedder.embed(&req.query).await?;
            self.cache.put_embedding(&req.query, emb.clone()).await;
            emb
        };

        let type_strs: Option<Vec<&str>> = req
            .types
            .as_ref()
            .map(|ts| ts.iter().map(MemoryType::as_str).collect());
        let entity_strs: Option<Vec<&str>> = req
            .entities
            .as_ref()
            .map(|es| es.iter().map(String::as_str).collect());
        let time_hash = req.time_range.as_ref().map(|tr| {
            use std::hash::{Hash, Hasher};
            let mut h = rustc_hash::FxHasher::default();
            tr.start.map(|s| s.timestamp()).hash(&mut h);
            tr.end.map(|e| e.timestamp()).hash(&mut h);
            h.finish()
        });
        let cache_key = cache::ResultCacheKey::new(&cache::ResultCacheKeyInput {
            embedding: &embedding,
            types: type_strs.as_deref(),
            entities: entity_strs.as_deref(),
            namespace,
            limit,
            include_stale: req.include_stale,
            include_invalidated: req.include_invalidated,
            time_range_hash: time_hash,
            explain: req.explain,
        });

        if let Some(cached) = self.cache.get_results(&cache_key, namespace).await {
            self.process_validate_ids(req.validate_ids.as_ref(), &cached)
                .await?;
            return Ok(cached);
        }

        let filter = build_qdrant_filter(&req);

        let results = self
            .vector_store
            .search(
                namespace,
                embedding,
                &req.query,
                candidate_pool_size,
                Some(filter),
            )
            .await?;

        tracing::debug!(recall.candidates = results.len(), "candidate pool size");

        if results.is_empty() {
            return Ok(vec![]);
        }

        let ids: Vec<String> = results.iter().map(|(id, _)| id.clone()).collect();
        let memories = self.metadata_store.get_memories_by_ids(&ids).await?;

        let memory_map: HashMap<&str, &ferrex_store::Memory> = memories
            .iter()
            .filter(|m| req.include_invalidated.unwrap_or(false) || m.t_invalid.is_none())
            .map(|m| (m.id.as_str(), m))
            .collect();

        let ordered: Vec<&ferrex_store::Memory> = results
            .iter()
            .filter_map(|(id, _)| memory_map.get(id.as_str()).copied())
            .collect();

        if ordered.is_empty() {
            return Ok(vec![]);
        }

        let doc_texts: Vec<String> = ordered.iter().map(|m| m.searchable_text()).collect();
        let doc_refs: Vec<&str> = doc_texts.iter().map(String::as_str).collect();

        let reranked = self.reranker.rerank(&req.query, &doc_refs, limit).await?;

        let now = Utc::now();
        let staleness_config = &self.config.staleness;
        let mut results: Vec<RecallResult> = reranked
            .iter()
            .filter_map(|r| {
                let memory = ordered.get(r.index)?;
                #[allow(clippy::cast_precision_loss)]
                let age_days = (now - memory.created_at).num_seconds() as f64 / SECONDS_PER_DAY;
                let recency = compute_recency_boost(memory.memory_type, age_days);
                #[allow(clippy::cast_possible_truncation)]
                let relevance_score = (f64::from(r.score) * recency) as f32;
                let s_score = staleness_score(memory, now, staleness_config);
                let threshold = threshold_for_type(staleness_config, memory.memory_type);
                let label = freshness_label(s_score, threshold);
                let scoring = if req.explain {
                    Some(ScoringBreakdown {
                        rrf_rank: 0,
                        rrf_score: 0.0,
                        rerank_score: r.score,
                        recency_boost: recency,
                        boosted_score: relevance_score,
                        staleness: s_score,
                        staleness_label: label,
                        final_rank: 0,
                    })
                } else {
                    None
                };

                Some(RecallResult {
                    memory: (*memory).clone(),
                    relevance_score,
                    staleness_score: s_score,
                    freshness_label: label,
                    scoring,
                })
            })
            .collect();

        if !results.is_empty() {
            let scores: Vec<f32> = results.iter().map(|r| r.relevance_score).collect();
            let max_score = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let min_score = scores.iter().copied().fold(f32::INFINITY, f32::min);
            tracing::debug!(
                recall.rerank_delta = max_score - min_score,
                recall.top_scores = ?&scores[..scores.len().min(3)],
                "reranked scores"
            );
        }

        results.sort_by(|a, b| b.relevance_score.total_cmp(&a.relevance_score));
        results.truncate(limit);

        if req.include_stale == Some(false) {
            results.retain(|r| r.freshness_label != FreshnessLabel::Stale);
        }

        if req.explain {
            for (i, r) in results.iter_mut().enumerate() {
                if let Some(ref mut s) = r.scoring {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        s.final_rank = (i + 1) as u32;
                    }
                }
            }
        }

        let accessed_ids: Vec<String> = results.iter().map(|r| r.memory.id.clone()).collect();
        self.access_tracker.record(&accessed_ids);
        if let Some(ids) = self.access_tracker.drain_if_full() {
            self.metadata_store.update_last_accessed(&ids).await?;
        }

        self.cache
            .put_results(&cache_key, results.clone(), namespace)
            .await;

        self.process_validate_ids(req.validate_ids.as_ref(), &results)
            .await?;

        self.ops_buffer.record(
            "recall",
            &req.query,
            op_start.elapsed(),
            &format!("{} results", results.len()),
        );

        Ok(results)
    }

    async fn process_validate_ids(
        &self,
        validate_ids: Option<&Vec<String>>,
        results: &[RecallResult],
    ) -> Result<(), CoreError> {
        if let Some(validate_ids) = validate_ids
            && !validate_ids.is_empty()
        {
            let result_ids: std::collections::HashSet<&str> =
                results.iter().map(|r| r.memory.id.as_str()).collect();
            let valid_ids: Vec<String> = validate_ids
                .iter()
                .filter(|id| Uuid::parse_str(id).is_ok() && result_ids.contains(id.as_str()))
                .cloned()
                .collect();
            if !valid_ids.is_empty() {
                self.metadata_store
                    .update_last_validated(&valid_ids, Utc::now())
                    .await?;
            }
        }
        Ok(())
    }
}

fn build_qdrant_filter(req: &RecallRequest) -> Filter {
    let mut must_conditions = vec![Condition::matches(
        ferrex_store::POINT_TYPE_FIELD,
        ferrex_store::POINT_TYPE_MEMORY.to_string(),
    )];

    if let Some(ref types) = req.types {
        let type_strings: Vec<String> = types.iter().map(|t| t.as_str().to_string()).collect();
        must_conditions.push(Condition::matches("memory_type", type_strings));
    }

    if let Some(ref range) = req.time_range {
        if let Some(start) = range.start {
            #[allow(clippy::cast_possible_wrap)]
            let ts = Timestamp {
                seconds: start.timestamp(),
                nanos: start.timestamp_subsec_nanos() as i32,
            };
            must_conditions.push(Condition::datetime_range(
                "created_at",
                DatetimeRange {
                    gte: Some(ts),
                    ..Default::default()
                },
            ));
        }
        if let Some(end) = range.end {
            #[allow(clippy::cast_possible_wrap)]
            let ts = Timestamp {
                seconds: end.timestamp(),
                nanos: end.timestamp_subsec_nanos() as i32,
            };
            must_conditions.push(Condition::datetime_range(
                "created_at",
                DatetimeRange {
                    lte: Some(ts),
                    ..Default::default()
                },
            ));
        }
    }

    let mut filter = Filter::must(must_conditions);

    if let Some(ref entities) = req.entities
        && !entities.is_empty()
    {
        let should = entities
            .iter()
            .map(|e| Condition::matches("entities", e.clone()))
            .collect();
        filter = Filter { should, ..filter };
    }

    filter
}
