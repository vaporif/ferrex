use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ferrex_embed::ModelTier;
use ferrex_store::{Memory, MemoryType};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct FerrexConfig {
    pub qdrant_url: Option<String>,
    pub qdrant_bin: String,
    pub qdrant_port: u16,
    pub model_tier: ModelTier,
    pub namespace: String,
    pub db_path: PathBuf,
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
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TimeRange {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct ForgetRequest {
    pub ids: Vec<String>,
    pub cascade: Option<bool>,
}

#[derive(Debug)]
pub struct ReflectRequest {
    pub scope: Option<String>,
    pub window: Option<String>,
}

#[derive(Debug)]
pub struct StatsRequest {
    pub detail: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ForgetResponse {
    pub message: String,
    pub deleted: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ReflectResponse {
    pub message: String,
    pub stale: Vec<Memory>,
    pub contradictions: Vec<Contradiction>,
    pub low_access: Vec<Memory>,
}

#[derive(Debug, Serialize)]
pub struct Contradiction {
    pub memory_a: String,
    pub memory_b: String,
    pub subject: String,
    pub predicate: String,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_memories: u64,
    pub recent_memories: Vec<Memory>,
    pub needs_attention: NeedsAttention,
}

#[derive(Debug, Serialize)]
pub struct NeedsAttention {
    pub stale_count: u64,
    pub conflict_count: u64,
    pub unvalidated_count: u64,
}
