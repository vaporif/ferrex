use ferrex_embed::EmbedError;
use ferrex_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("validation error: {0}")]
    Validation(String),
}
