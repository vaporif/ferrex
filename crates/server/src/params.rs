use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
pub struct StoreParams {
    /// Required for episodic and procedural memories.
    pub content: Option<String>,
    /// "episodic", "semantic", or "procedural". Auto-detected if omitted.
    pub memory_type: Option<String>,
    /// Semantic triple subject, e.g. "api-server".
    pub subject: Option<String>,
    /// Semantic triple predicate, e.g. "uses".
    pub predicate: Option<String>,
    /// Semantic triple object, e.g. "tokio 1.38".
    pub object: Option<String>,
    /// 0.0-1.0, defaults to 1.0.
    pub confidence: Option<f64>,
    pub source: Option<String>,
    /// Arbitrary JSON metadata.
    pub context: Option<serde_json::Value>,
    /// Mentioned entity names.
    #[serde(default)]
    pub entities: Vec<String>,
    pub namespace: Option<String>,
    /// ID of a memory this one replaces.
    pub supersedes: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TimeRangeParam {
    /// ISO-8601 inclusive start, e.g. "2025-01-01T00:00:00Z".
    pub start: Option<String>,
    /// ISO-8601 inclusive end.
    pub end: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RecallParams {
    pub query: String,
    /// e.g. `["episodic"]`, `["semantic"]`.
    pub types: Option<Vec<String>>,
    pub entities: Option<Vec<String>>,
    pub namespace: Option<String>,
    /// Default 10.
    pub limit: Option<usize>,
    /// IDs to mark as still accurate.
    pub validate_ids: Option<Vec<String>>,
    pub time_range: Option<TimeRangeParam>,
    /// ISO-8601 instant (e.g. "2026-01-15T00:00:00Z"). Returns memories whose
    /// validity window covered this time.
    pub as_of: Option<String>,
    /// Include superseded memories (default false).
    pub include_invalidated: Option<bool>,
    /// Include stale memories (default true).
    pub include_stale: Option<bool>,
    /// Return scoring breakdown per result.
    #[serde(default)]
    pub explain: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct TimelineParams {
    /// Entity name or alias (resolved via the entity registry).
    pub entity: String,
    pub namespace: Option<String>,
    /// Default 10, max 200.
    pub limit: Option<usize>,
    /// Filter by memory type, e.g. `["semantic"]`.
    pub types: Option<Vec<String>>,
    /// Include superseded memories (default false).
    pub include_invalidated: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaxonomyParams {
    /// Defaults to the server namespace. Omit to also list all namespaces
    /// with live memories.
    pub namespace: Option<String>,
    /// Top-N size for entities and predicates. Default 10, max 100.
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ForgetParams {
    pub ids: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReflectParams {
    pub namespace: String,
    /// Default 20.
    pub limit: Option<u32>,
    /// Default true.
    pub include_contradictions: Option<bool>,
    /// Default true.
    pub include_stale: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StatsParams {
    pub namespace: String,
    /// Per-type breakdown and staleness distribution.
    pub detailed: Option<bool>,
    /// Qdrant status, SQLite size, cache stats, recent ops.
    pub diagnostics: Option<bool>,
}
