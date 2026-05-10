use ferrex_embed::{Embedder, ModelTier, Reranker, RerankerTier, init_embed_env};

fn main() {
    init_embed_env();

    println!("Downloading embedding model (small)...");
    Embedder::new(ModelTier::Small).expect("failed to init small embedding model");

    println!("Downloading reranker model (default)...");
    Reranker::new(RerankerTier::Default).expect("failed to init default reranker");

    println!("All models cached.");
}
