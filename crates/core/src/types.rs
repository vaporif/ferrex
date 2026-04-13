use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ferrex_embed::{ModelTier, RerankerTier};
use ferrex_store::{Memory, MemoryType};
use serde::Serialize;

use crate::staleness::StalenessConfig;

#[derive(Debug, Clone)]
pub struct FerrexConfig {
    pub qdrant_url: Option<String>,
    pub qdrant_bin: String,
    pub qdrant_port: u16,
    pub model_tier: ModelTier,
    pub reranker_tier: RerankerTier,
    pub namespace: String,
    pub db_path: PathBuf,
    pub config_path: Option<PathBuf>,
    pub deduplication: DedupConfig,
    pub conflict: ConflictConfig,
    pub predicates: PredicatesConfig,
    pub reconciliation: ReconciliationConfig,
    pub staleness: StalenessConfig,
    pub reader_pool_size: usize,
    pub cache: CacheConfig,
}

#[derive(Debug, Clone)]
pub struct DedupConfig {
    pub threshold: f32,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self { threshold: 0.95 }
    }
}

#[derive(Debug, Clone)]
pub struct ConflictConfig {
    pub object_fuzzy_duplicate: f32,
    pub object_fuzzy_update: f32,
}

impl Default for ConflictConfig {
    fn default() -> Self {
        Self {
            object_fuzzy_duplicate: 0.95,
            object_fuzzy_update: 0.50,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PredicatesConfig {
    pub groups: HashMap<String, Vec<String>>,
    pub namespaces: HashMap<String, NamespacePredicatesConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct NamespacePredicatesConfig {
    pub groups: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct ReconciliationConfig {
    pub audit_interval_hours: Option<u64>,
    pub audit_fix_limit: u64,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            audit_interval_hours: None,
            audit_fix_limit: 1000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub embedding_capacity: usize,
    pub result_capacity: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            embedding_capacity: 256,
            result_capacity: 128,
        }
    }
}

#[derive(Debug)]
pub struct StoreRequest {
    pub content: Option<String>,
    pub memory_type: Option<MemoryType>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub confidence: Option<f64>,
    pub source: Option<String>,
    pub context: Option<serde_json::Value>,
    pub entities: Vec<String>,
    pub namespace: Option<String>,
    pub supersedes: Option<String>,
}

#[derive(Debug)]
pub struct RecallRequest {
    pub query: String,
    pub types: Option<Vec<MemoryType>>,
    pub entities: Option<Vec<String>>,
    pub namespace: Option<String>,
    pub limit: Option<usize>,
    pub include_stale: Option<bool>,
    pub include_invalidated: Option<bool>,
    pub time_range: Option<TimeRange>,
    /// Point-in-time filter. Keeps rows whose validity window (`t_valid` or
    /// `created_at` through `t_invalid`) covered this instant.
    /// `include_invalidated = true` drops the end check.
    ///
    /// Scoring and `include_stale` still use the current clock, so a row that
    /// was fresh at `as_of` but is stale today still gets dropped. `as_of`
    /// asks "what did we know at T", it does not replay the whole pipeline at T.
    pub as_of: Option<DateTime<Utc>>,
    pub validate_ids: Option<Vec<String>>,
    pub explain: bool,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TimeRange {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct ForgetRequest {
    pub ids: Vec<String>,
    /// Deprecated, ignored.
    pub cascade: Option<bool>,
}

#[derive(Debug)]
pub struct ReflectRequest {
    pub namespace: String,
    pub limit: Option<u32>,
    pub include_contradictions: bool,
    pub include_stale: bool,
}

#[derive(Debug)]
pub struct StatsRequest {
    pub namespace: String,
    pub detailed: Option<bool>,
    pub diagnostics: Option<bool>,
}

#[derive(Debug)]
pub struct TimelineRequest {
    pub entity: String,
    pub namespace: Option<String>,
    pub limit: Option<usize>,
    pub types: Option<Vec<MemoryType>>,
    pub include_invalidated: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct TimelineEntry {
    pub memory: Memory,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct TimelineResponse {
    pub entity: String,
    pub resolved_entity_id: Option<String>,
    pub entries: Vec<TimelineEntry>,
}

#[derive(Debug)]
pub struct TaxonomyRequest {
    pub namespace: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct TaxonomyEntityEntry {
    pub name: String,
    pub memory_count: u64,
}

#[derive(Debug, Serialize)]
pub struct TaxonomyPredicateEntry {
    pub predicate: String,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct TaxonomyResponse {
    pub namespace: String,
    pub total_memories: u64,
    pub by_type: HashMap<MemoryType, u64>,
    pub top_entities: Vec<TaxonomyEntityEntry>,
    pub top_predicates: Vec<TaxonomyPredicateEntry>,
    /// Distinct namespaces with live memories. `None` when the request was scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_namespaces: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct StoreResponse {
    pub id: String,
    pub memory_type: String,
    pub superseded: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ForgetResponse {
    pub message: String,
    pub deleted: Vec<String>,
    pub not_found: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ReflectResponse {
    pub stale: Vec<StaleCandidate>,
    pub contradictions: Vec<ContradictionPair>,
    pub summary: ReflectSummary,
}

#[derive(Debug, Serialize)]
pub struct StaleCandidate {
    pub memory: Memory,
    pub staleness_score: f64,
    pub freshness_label: crate::staleness::FreshnessLabel,
    pub eviction_rank: u32,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ContradictionPair {
    pub a: Memory,
    pub b: Memory,
    pub match_type: ContradictionMatchType,
    pub similarity: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionMatchType {
    ExactPredicate,
    FuzzyPredicate,
    SimilarSubject,
}

#[derive(Debug, Serialize)]
pub struct ReflectSummary {
    pub total_scanned: u64,
    pub stale_count: u64,
    pub contradiction_count: u64,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_memories: u64,
    pub recent_memories: Vec<Memory>,
    pub needs_attention: NeedsAttention,
    pub details: Option<StatsDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<DiagnosticsReport>,
}

#[derive(Debug, Serialize)]
pub struct StatsDetails {
    pub by_type: HashMap<String, TypeStats>,
    pub storage_size_bytes: u64,
    pub entity_count: u64,
    pub staleness_distribution: StalenessDistribution,
}

#[derive(Debug, Serialize)]
pub struct TypeStats {
    pub count: u64,
    pub stale_count: u64,
    pub avg_staleness: f64,
    pub avg_access_count: f64,
    pub oldest: Option<DateTime<Utc>>,
    pub newest: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct StalenessDistribution {
    pub fresh: u64,
    pub aging: u64,
    pub stale: u64,
}

#[derive(Debug, Serialize)]
pub struct NeedsAttention {
    pub stale_count: u64,
    pub conflict_count: u64,
    pub unvalidated_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallResult {
    pub memory: Memory,
    pub relevance_score: f32,
    pub staleness_score: f64,
    pub freshness_label: crate::staleness::FreshnessLabel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoring: Option<ScoringBreakdown>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoringBreakdown {
    pub rrf_rank: u32,
    pub rrf_score: f32,
    pub rerank_score: f32,
    pub recency_boost: f64,
    pub boosted_score: f32,
    pub staleness: f64,
    pub staleness_label: crate::staleness::FreshnessLabel,
    pub final_rank: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub embedding_hits: u64,
    pub embedding_misses: u64,
    pub result_hits: u64,
    pub result_misses: u64,
    pub embedding_capacity: usize,
    pub embedding_len: usize,
    pub result_capacity: usize,
    pub result_len: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpRecord {
    pub kind: String,
    pub detail: String,
    pub duration_ms: u64,
    pub outcome: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsReport {
    pub version: String,
    pub embedding_model: String,
    pub qdrant_url: String,
    pub qdrant_pid: Option<u32>,
    pub collection: String,
    pub sqlite_path: String,
    pub sqlite_size_bytes: u64,
    pub memory_count: u64,
    pub entity_count: u64,
    pub pending_ops: u64,
    pub cache: CacheStats,
    pub recent_ops: Vec<OpRecord>,
}
