use std::sync::{Arc, Mutex};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

#[derive(Debug, Clone, Copy, Default)]
pub enum ModelTier {
    Small,
    Mid,
    #[default]
    Best,
}

impl ModelTier {
    const fn to_fastembed(self) -> EmbeddingModel {
        match self {
            Self::Small => EmbeddingModel::AllMiniLML6V2,
            Self::Mid => EmbeddingModel::BGESmallENV15,
            Self::Best => EmbeddingModel::BGEBaseENV15,
        }
    }

    pub const fn dimension(self) -> usize {
        match self {
            Self::Small | Self::Mid => 384,
            Self::Best => 768,
        }
    }

    pub const fn model_name(self) -> &'static str {
        match self {
            Self::Small => "all-MiniLM-L6-v2",
            Self::Mid => "bge-small-en-v1.5",
            Self::Best => "bge-base-en-v1.5",
        }
    }
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.model_name())
    }
}

impl std::str::FromStr for ModelTier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "small" => Ok(Self::Small),
            "mid" => Ok(Self::Mid),
            "best" => Ok(Self::Best),
            _ => Err(format!(
                "unknown model tier: {s} (expected: small, mid, best)"
            )),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("failed to initialize embedding model: {0}")]
    Init(String),
    #[error("embedding failed: {0}")]
    Embed(String),
}

pub struct Embedder {
    model: Arc<Mutex<TextEmbedding>>,
    tier: ModelTier,
}

impl Embedder {
    pub fn new(tier: ModelTier) -> Result<Self, EmbedError> {
        tracing::info!(tier = %tier, "initializing embedding model");
        let options = TextInitOptions::new(tier.to_fastembed()).with_show_download_progress(true);
        let model = TextEmbedding::try_new(options).map_err(|e| EmbedError::Init(e.to_string()))?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            tier,
        })
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let model = Arc::clone(&self.model);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut model = model.lock().expect("embedding model lock poisoned");
            let results = model
                .embed(vec![&text], None)
                .map_err(|e| EmbedError::Embed(e.to_string()))?;
            results
                .into_iter()
                .next()
                .ok_or_else(|| EmbedError::Embed("no embedding returned".into()))
        })
        .await
        .map_err(|e| EmbedError::Embed(e.to_string()))?
    }

    pub async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            let mut model = model.lock().expect("embedding model lock poisoned");
            model
                .embed(texts, None)
                .map_err(|e| EmbedError::Embed(e.to_string()))
        })
        .await
        .map_err(|e| EmbedError::Embed(e.to_string()))?
    }

    pub const fn dimension(&self) -> usize {
        self.tier.dimension()
    }

    pub const fn tier(&self) -> ModelTier {
        self.tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_embedder_small() {
        let embedder = Embedder::new(ModelTier::Small).expect("failed to create embedder");
        assert_eq!(embedder.dimension(), 384);

        let embedding = embedder.embed("hello world").await.expect("embed failed");
        assert_eq!(embedding.len(), 384);
    }

    #[tokio::test]
    async fn test_embed_batch() {
        let embedder = Embedder::new(ModelTier::Small).expect("failed to create embedder");
        let texts = vec![
            "first document".to_owned(),
            "second document".to_owned(),
            "third document".to_owned(),
        ];
        let embeddings = embedder
            .embed_batch(texts)
            .await
            .expect("batch embed failed");
        assert_eq!(embeddings.len(), 3);
        for emb in &embeddings {
            assert_eq!(emb.len(), 384);
        }
    }

    #[tokio::test]
    async fn test_model_tier_parse() {
        assert!(matches!("small".parse::<ModelTier>(), Ok(ModelTier::Small)));
        assert!(matches!("mid".parse::<ModelTier>(), Ok(ModelTier::Mid)));
        assert!(matches!("best".parse::<ModelTier>(), Ok(ModelTier::Best)));
        assert!("unknown".parse::<ModelTier>().is_err());
    }
}
