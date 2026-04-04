use qdrant_client::qdrant::{
    CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder, Distance,
    Document, FieldType, Filter, Fusion, Modifier, NamedVectors, PointStruct, PrefetchQueryBuilder,
    Query, QueryPointsBuilder, SparseVectorParamsBuilder, SparseVectorsConfigBuilder,
    UpsertPointsBuilder, VectorInput, VectorParamsBuilder, VectorsConfigBuilder,
    point_id::PointIdOptions,
};
use qdrant_client::{Payload, Qdrant};
use uuid::Uuid;

use crate::StoreError;

const QDRANT_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const DENSE_VECTOR: &str = "dense";
const SPARSE_VECTOR: &str = "sparse";
const BM25_TOKENIZER: &str = "Qdrant/bm25";
const MIN_PREFETCH_LIMIT: u64 = 30;

pub struct VectorStore {
    client: Qdrant,
    dimension: usize,
}

impl VectorStore {
    pub fn new(url: &str, dimension: usize) -> Result<Self, StoreError> {
        let client = Qdrant::from_url(url)
            .timeout(QDRANT_CLIENT_TIMEOUT)
            .build()
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(Self { client, dimension })
    }

    fn collection_name(namespace: &str) -> Result<String, StoreError> {
        if namespace.is_empty()
            || !namespace
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(StoreError::Qdrant(format!(
                "invalid namespace: {namespace:?} (only alphanumeric, underscore, hyphen allowed)"
            )));
        }
        Ok(format!("ferrex_{namespace}"))
    }

    pub async fn ensure_collection(&self, namespace: &str) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace)?;

        if self
            .client
            .collection_exists(&name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?
        {
            self.validate_collection_schema(&name).await?;
            return Ok(());
        }

        let mut vectors_config = VectorsConfigBuilder::default();
        vectors_config.add_named_vector_params(
            DENSE_VECTOR,
            VectorParamsBuilder::new(self.dimension as u64, Distance::Cosine),
        );

        let mut sparse_config = SparseVectorsConfigBuilder::default();
        sparse_config.add_named_vector_params(
            SPARSE_VECTOR,
            SparseVectorParamsBuilder::default().modifier(Modifier::Idf),
        );

        self.client
            .create_collection(
                CreateCollectionBuilder::new(&name)
                    .vectors_config(vectors_config)
                    .sparse_vectors_config(sparse_config),
            )
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;

        for field in [
            "memory_type",
            "namespace",
            "entities",
            crate::POINT_TYPE_FIELD,
        ] {
            self.client
                .create_field_index(CreateFieldIndexCollectionBuilder::new(
                    &name,
                    field,
                    FieldType::Keyword,
                ))
                .await
                .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        }

        Ok(())
    }

    async fn validate_collection_schema(&self, name: &str) -> Result<(), StoreError> {
        let info = self
            .client
            .collection_info(name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;

        let has_named_vectors = info
            .result
            .and_then(|r| r.config)
            .and_then(|c| c.params)
            .and_then(|p| p.vectors_config)
            .and_then(|vc| vc.config)
            .is_some_and(|c| {
                matches!(
                    c,
                    qdrant_client::qdrant::vectors_config::Config::ParamsMap(_)
                )
            });

        if !has_named_vectors {
            return Err(StoreError::Qdrant(format!(
                "collection {name} uses an incompatible schema (single vector). \
                 Hybrid search requires named vectors (dense + sparse). \
                 Delete the collection or start with a fresh database."
            )));
        }

        Ok(())
    }

    pub async fn upsert(
        &self,
        namespace: &str,
        id: Uuid,
        vector: Vec<f32>,
        content_text: &str,
        payload: Payload,
    ) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace)?;
        let vectors = NamedVectors::default()
            .add_vector(DENSE_VECTOR, vector)
            .add_vector(SPARSE_VECTOR, Document::new(content_text, BM25_TOKENIZER));
        let point = PointStruct::new(id.to_string(), vectors, payload);
        self.client
            .upsert_points(UpsertPointsBuilder::new(&name, vec![point]).wait(true))
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }

    pub async fn search(
        &self,
        namespace: &str,
        vector: Vec<f32>,
        query_text: &str,
        limit: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        let name = Self::collection_name(namespace)?;
        let prefetch_limit = (limit as u64).max(MIN_PREFETCH_LIMIT);

        let make_prefetch = |query, using, f: Option<Filter>| -> PrefetchQueryBuilder {
            let mut b = PrefetchQueryBuilder::default()
                .query(query)
                .using(using)
                .limit(prefetch_limit);
            if let Some(f) = f {
                b = b.filter(f);
            }
            b
        };

        let dense_prefetch =
            make_prefetch(VectorInput::new_dense(vector), DENSE_VECTOR, filter.clone());
        let sparse_prefetch = make_prefetch(
            VectorInput::from(Document::new(query_text, BM25_TOKENIZER)),
            SPARSE_VECTOR,
            filter,
        );

        let builder = QueryPointsBuilder::new(&name)
            .add_prefetch(dense_prefetch)
            .add_prefetch(sparse_prefetch)
            .query(Query::new_fusion(Fusion::Rrf))
            .limit(limit as u64)
            .with_payload(true);

        let results = self
            .client
            .query(builder)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;

        Ok(results
            .result
            .into_iter()
            .filter_map(|point| {
                let id = match point.id?.point_id_options? {
                    PointIdOptions::Uuid(s) => s,
                    PointIdOptions::Num(n) => n.to_string(),
                };
                let score = point.score;
                Some((id, score))
            })
            .collect())
    }

    pub async fn delete(&self, namespace: &str, id: Uuid) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace)?;
        self.client
            .delete_points(
                DeletePointsBuilder::new(&name)
                    .points(vec![id.to_string()])
                    .wait(true),
            )
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }

    pub async fn health_check(&self) -> Result<(), StoreError> {
        self.client
            .health_check()
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrant_client::qdrant::Condition;

    const TEST_DIM: usize = 384;

    fn test_vector() -> Vec<f32> {
        vec![0.1; TEST_DIM]
    }

    #[tokio::test]
    #[ignore = "requires running Qdrant"]
    async fn test_upsert_and_search() {
        let store = VectorStore::new("http://localhost:6334", TEST_DIM).unwrap();
        let ns = "test_hybrid_upsert_search";
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let content = "test memory about Rust programming";
        let payload = Payload::try_from(serde_json::json!({
            "memory_id": id.to_string(),
            "memory_type": "episodic",
            "namespace": ns,
            "content": content,
            "entities": Vec::<String>::new(),
            "created_at": "2026-01-01T00:00:00Z",
            crate::POINT_TYPE_FIELD: crate::POINT_TYPE_MEMORY,
        }))
        .unwrap();

        store
            .upsert(ns, id, test_vector(), content, payload)
            .await
            .unwrap();

        let filter = Filter::must([Condition::matches(
            crate::POINT_TYPE_FIELD,
            crate::POINT_TYPE_MEMORY.to_string(),
        )]);
        let results = store
            .search(ns, test_vector(), "Rust programming", 10, Some(filter))
            .await
            .unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires running Qdrant"]
    async fn test_delete() {
        let store = VectorStore::new("http://localhost:6334", TEST_DIM).unwrap();
        let ns = "test_hybrid_delete";
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let payload = Payload::try_from(serde_json::json!({
            crate::POINT_TYPE_FIELD: crate::POINT_TYPE_MEMORY,
        }))
        .unwrap();
        store
            .upsert(ns, id, test_vector(), "test content", payload)
            .await
            .unwrap();
        store.delete(ns, id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires running Qdrant"]
    async fn test_bm25_keyword_contribution() {
        let store = VectorStore::new("http://localhost:6334", TEST_DIM).unwrap();
        let ns = "test_bm25_keyword";
        let _ = store
            .client
            .delete_collection(&format!("ferrex_{ns}"))
            .await;
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let content = "the MemoryService struct handles all store and recall operations";
        let payload = Payload::try_from(serde_json::json!({
            "memory_id": id.to_string(),
            "memory_type": "episodic",
            "namespace": ns,
            "content": content,
            "entities": Vec::<String>::new(),
            "created_at": "2026-01-01T00:00:00Z",
            crate::POINT_TYPE_FIELD: crate::POINT_TYPE_MEMORY,
        }))
        .unwrap();

        store
            .upsert(ns, id, test_vector(), content, payload)
            .await
            .unwrap();

        let results = store
            .search(ns, test_vector(), "MemoryService", 10, None)
            .await
            .unwrap();
        assert!(
            !results.is_empty(),
            "BM25 should find MemoryService keyword"
        );
    }
}
