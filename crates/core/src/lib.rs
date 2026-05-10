mod access_tracker;
mod cache;
mod config;
mod entity;
mod error;
mod ops_buffer;
mod pipeline;
mod predicate;
mod reflect;
mod retrieval;
mod service;
pub mod staleness;
mod types;

pub use config::{ConfigError, LoadedConfig, load_or_init, resolve_namespace_groups};
pub use entity::EntityResolver;
pub use error::CoreError;
pub use ops_buffer::{AuditReport, BackfillReport, RecoveryReport};
pub use predicate::PredicateNormalizer;
pub use retrieval::compute_recency_boost;
pub use service::MemoryService;
pub use staleness::{FreshnessLabel, StalenessConfig, StalenessWeights, TypeStalenessConfig};
pub use types::*;

pub use ferrex_embed::{EmbedError, Embedder, ModelTier, Reranker, RerankerTier};
pub use ferrex_store::{Entity, Memory, MemoryType, VectorStore};
