use ferrex_embed::EmbedError;
use ferrex_store::StoreError;

use crate::config::ConfigError;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("duplicate memory: existing={existing_id}, similarity={similarity:.4}")]
    Duplicate {
        existing_id: String,
        similarity: f32,
    },
    #[error("conflict ambiguous: existing={existing_id}, ratio={ratio:.4}")]
    ConflictAmbiguous { existing_id: String, ratio: f32 },
    #[error("multi-match conflict: existing={existing_ids:?}")]
    MultiMatchConflict { existing_ids: Vec<String> },
}
