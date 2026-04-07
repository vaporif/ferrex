use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};

use lru::LruCache;
use rustc_hash::FxHasher;
use tokio::sync::RwLock;

use crate::types::{CacheConfig, RecallResult};

pub struct RecallCache {
    embeddings: RwLock<LruCache<u64, Vec<f32>>>,
    results: RwLock<LruCache<u64, CachedResults>>,
    generations: RwLock<HashMap<String, u64>>,
    embedding_hits: AtomicU64,
    embedding_misses: AtomicU64,
    result_hits: AtomicU64,
    result_misses: AtomicU64,
}

struct CachedResults {
    generation: u64,
    results: Vec<RecallResult>,
}

pub struct ResultCacheKey {
    hash: u64,
}

pub struct ResultCacheKeyInput<'a> {
    pub embedding: &'a [f32],
    pub types: Option<&'a [&'a str]>,
    pub entities: Option<&'a [&'a str]>,
    pub namespace: &'a str,
    pub limit: usize,
    pub include_stale: Option<bool>,
    pub include_invalidated: Option<bool>,
    pub time_range_hash: Option<u64>,
    pub explain: bool,
}

impl ResultCacheKey {
    pub fn new(input: &ResultCacheKeyInput<'_>) -> Self {
        let mut hasher = FxHasher::default();
        for &v in input.embedding {
            v.to_bits().hash(&mut hasher);
        }
        input.types.hash(&mut hasher);
        input.entities.hash(&mut hasher);
        input.namespace.hash(&mut hasher);
        input.limit.hash(&mut hasher);
        input.include_stale.hash(&mut hasher);
        input.include_invalidated.hash(&mut hasher);
        input.time_range_hash.hash(&mut hasher);
        input.explain.hash(&mut hasher);
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
            embedding_hits: AtomicU64::new(0),
            embedding_misses: AtomicU64::new(0),
            result_hits: AtomicU64::new(0),
            result_misses: AtomicU64::new(0),
        }
    }

    #[tracing::instrument(name = "cache_lookup", skip_all, fields(hit))]
    pub async fn get_embedding(&self, query: &str) -> Option<Vec<f32>> {
        let result = self
            .embeddings
            .read()
            .await
            .peek(&hash_query(query))
            .cloned();
        if result.is_some() {
            self.embedding_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.embedding_misses.fetch_add(1, Ordering::Relaxed);
        }
        tracing::Span::current().record("hit", result.is_some());
        result
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
        let entry = cache.peek(&key.hash);
        match entry {
            Some(e) if e.generation == current_gen => {
                self.result_hits.fetch_add(1, Ordering::Relaxed);
                Some(e.results.clone())
            }
            _ => {
                self.result_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
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

    pub async fn cache_stats(&self) -> crate::types::CacheStats {
        let emb = self.embeddings.read().await;
        let res = self.results.read().await;
        crate::types::CacheStats {
            embedding_hits: self.embedding_hits.load(Ordering::Relaxed),
            embedding_misses: self.embedding_misses.load(Ordering::Relaxed),
            result_hits: self.result_hits.load(Ordering::Relaxed),
            result_misses: self.result_misses.load(Ordering::Relaxed),
            embedding_capacity: emb.cap().get(),
            embedding_len: emb.len(),
            result_capacity: res.cap().get(),
            result_len: res.len(),
        }
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
        let filter = ResultCacheKey::new(&ResultCacheKeyInput {
            embedding: &embedding,
            types: None,
            entities: None,
            namespace: ns,
            limit: 10,
            include_stale: None,
            include_invalidated: None,
            time_range_hash: None,
            explain: false,
        });
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
        let filter = ResultCacheKey::new(&ResultCacheKeyInput {
            embedding: &embedding,
            types: None,
            entities: None,
            namespace: ns,
            limit: 10,
            include_stale: None,
            include_invalidated: None,
            time_range_hash: None,
            explain: false,
        });
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
