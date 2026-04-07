use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;

use lru::LruCache;
use rustc_hash::FxHasher;
use tokio::sync::RwLock;

use crate::types::{CacheConfig, RecallResult};

pub struct RecallCache {
    embeddings: RwLock<LruCache<u64, Vec<f32>>>,
    results: RwLock<LruCache<u64, CachedResults>>,
    generations: RwLock<HashMap<String, u64>>,
}

struct CachedResults {
    generation: u64,
    results: Vec<RecallResult>,
}

pub struct ResultCacheKey {
    hash: u64,
}

impl ResultCacheKey {
    pub fn new(
        embedding: &[f32],
        types: Option<&[&str]>,
        entities: Option<&[&str]>,
        namespace: &str,
        limit: usize,
        include_stale: Option<bool>,
        include_invalidated: Option<bool>,
        time_range_hash: Option<u64>,
    ) -> Self {
        let mut hasher = FxHasher::default();
        for &v in embedding {
            v.to_bits().hash(&mut hasher);
        }
        types.hash(&mut hasher);
        entities.hash(&mut hasher);
        namespace.hash(&mut hasher);
        limit.hash(&mut hasher);
        include_stale.hash(&mut hasher);
        include_invalidated.hash(&mut hasher);
        time_range_hash.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
        }
    }
}

fn hash_query(query: &str) -> u64 {
    let mut hasher = FxHasher::default();
    query.hash(&mut hasher);
    hasher.finish()
}

impl RecallCache {
    pub fn new(config: &CacheConfig) -> Self {
        Self {
            embeddings: RwLock::new(LruCache::new(
                NonZeroUsize::new(config.embedding_capacity).expect("capacity > 0"),
            )),
            results: RwLock::new(LruCache::new(
                NonZeroUsize::new(config.result_capacity).expect("capacity > 0"),
            )),
            generations: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_embedding(&self, query: &str) -> Option<Vec<f32>> {
        self.embeddings
            .read()
            .await
            .peek(&hash_query(query))
            .cloned()
    }

    pub async fn put_embedding(&self, query: &str, embedding: Vec<f32>) {
        self.embeddings
            .write()
            .await
            .put(hash_query(query), embedding);
    }

    pub async fn get_results(
        &self,
        key: &ResultCacheKey,
        namespace: &str,
    ) -> Option<Vec<RecallResult>> {
        let current_gen = self.generation(namespace).await;
        let cache = self.results.read().await;
        let entry = cache.peek(&key.hash)?;
        if entry.generation == current_gen {
            Some(entry.results.clone())
        } else {
            None
        }
    }

    pub async fn put_results(
        &self,
        key: &ResultCacheKey,
        results: Vec<RecallResult>,
        namespace: &str,
    ) {
        let current_gen = self.generation(namespace).await;
        self.results.write().await.put(
            key.hash,
            CachedResults {
                generation: current_gen,
                results,
            },
        );
    }

    pub async fn generation(&self, namespace: &str) -> u64 {
        self.generations
            .read()
            .await
            .get(namespace)
            .copied()
            .unwrap_or(0)
    }

    pub async fn bump_generation(&self, namespace: &str) {
        let mut gens = self.generations.write().await;
        let entry = gens.entry(namespace.to_string()).or_insert(0);
        *entry += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CacheConfig;

    fn make_cache() -> RecallCache {
        RecallCache::new(&CacheConfig {
            embedding_capacity: 4,
            result_capacity: 4,
        })
    }

    #[tokio::test]
    async fn test_embedding_cache_hit() {
        let cache = make_cache();
        let embedding = vec![0.1, 0.2, 0.3];
        cache.put_embedding("hello world", embedding.clone()).await;
        let cached = cache.get_embedding("hello world").await;
        assert_eq!(cached, Some(embedding));
    }

    #[tokio::test]
    async fn test_embedding_cache_miss() {
        let cache = make_cache();
        let cached = cache.get_embedding("not stored").await;
        assert_eq!(cached, None);
    }

    #[tokio::test]
    async fn test_result_cache_hit() {
        let cache = make_cache();
        let ns = "test-ns";
        let results = vec![];
        let embedding = vec![0.1, 0.2];
        let filter = ResultCacheKey::new(&embedding, None, None, ns, 10, None, None, None);
        cache.put_results(&filter, results, ns).await;

        let current_gen = cache.generation(ns).await;
        assert_eq!(current_gen, 0);

        let cached = cache.get_results(&filter, ns).await;
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn test_result_cache_invalidated_on_write() {
        let cache = make_cache();
        let ns = "test-ns";
        let embedding = vec![0.1, 0.2];
        let filter = ResultCacheKey::new(&embedding, None, None, ns, 10, None, None, None);
        cache.put_results(&filter, vec![], ns).await;

        cache.bump_generation(ns).await;

        let cached = cache.get_results(&filter, ns).await;
        assert!(cached.is_none(), "should miss after generation bump");
    }

    #[tokio::test]
    async fn test_embedding_cache_eviction() {
        let cache = make_cache(); // capacity 4
        for i in 0..5 {
            cache
                .put_embedding(&format!("query {i}"), vec![i as f32])
                .await;
        }
        assert!(cache.get_embedding("query 0").await.is_none());
        assert!(cache.get_embedding("query 4").await.is_some());
    }
}
