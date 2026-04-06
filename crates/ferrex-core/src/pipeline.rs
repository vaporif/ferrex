pub mod conflict;
pub mod dedup;
pub mod embed;
pub mod normalize_predicate;
pub mod resolve_entities;
pub mod validate;
pub mod write;

use chrono::{DateTime, Utc};
use ferrex_store::MemoryType;
use uuid::Uuid;

use crate::predicate::PredicateNormalizer;
use crate::types::{ConflictConfig, DedupConfig, StoreRequest};

pub struct StoreContext<'a> {
    pub req: StoreRequest,
    pub namespace: String,
    pub memory_type: MemoryType,
    pub now: DateTime<Utc>,
    pub id: Uuid,
    pub normalized_predicate: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub search_text: Option<String>,
    pub resolved_entities: Vec<ferrex_store::Entity>,
    pub superseded_ids: Vec<String>,
    pub normalizer: &'a PredicateNormalizer,
    pub dedup_config: &'a DedupConfig,
    pub conflict_config: &'a ConflictConfig,
}

impl<'a> StoreContext<'a> {
    pub fn new(
        req: StoreRequest,
        namespace: String,
        memory_type: MemoryType,
        normalizer: &'a PredicateNormalizer,
        dedup_config: &'a DedupConfig,
        conflict_config: &'a ConflictConfig,
    ) -> Self {
        Self {
            req,
            namespace,
            memory_type,
            now: Utc::now(),
            id: Uuid::now_v7(),
            normalized_predicate: None,
            embedding: None,
            search_text: None,
            resolved_entities: vec![],
            superseded_ids: vec![],
            normalizer,
            dedup_config,
            conflict_config,
        }
    }
}
