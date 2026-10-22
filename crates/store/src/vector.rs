use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder,
    Distance, Document, FieldType, Filter, Fusion, Modifier, NamedVectors, PointStruct,
    PrefetchQueryBuilder, Query, QueryPointsBuilder, ScrollPointsBuilder,
    SparseVectorParamsBuilder, SparseVectorsConfigBuilder, UpsertPointsBuilder, VectorInput,
    VectorParamsBuilder, VectorsConfigBuilder, point_id::PointIdOptions,
};
use qdrant_client::{Payload, Qdrant};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::StoreError;

pub struct BatchPoint<'a> {
    pub id: Uuid,
    pub vector: Vec<f32>,
    pub content_text: &'a str,
    pub payload: Payload,
}

pub struct ScoredPayload {
    pub id: String,
    pub score: f32,
    pub payload: Payload,
}

const QDRANT_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const DENSE_VECTOR: &str = "dense";
const SPARSE_VECTOR: &str = "sparse";
const BM25_TOKENIZER: &str = "Qdrant/bm25";
const MIN_PREFETCH_LIMIT: u64 = 20;
const SCROLL_PAGE_SIZE: u32 = 1024;
const INDEXED_FIELDS: [(&str, FieldType); 6] = [
    (crate::QdrantField::MEMORY_TYPE, FieldType::Keyword),
    (crate::QdrantField::NAMESPACE, FieldType::Keyword),
    (crate::QdrantField::ENTITIES, FieldType::Keyword),
    (crate::QdrantField::POINT_TYPE, FieldType::Keyword),
    (crate::QdrantField::AGENT_ID, FieldType::Keyword),
    (crate::QdrantField::CREATED_AT, FieldType::Datetime),
];

pub fn validate_namespace(namespace: &str) -> Result<(), String> {
    if namespace.is_empty()
        || !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "invalid namespace: {namespace:?} (only alphanumeric, underscore, hyphen allowed)"
        ));
    }
    Ok(())
}

pub struct VectorStore {
    pub(crate) client: Qdrant,
    pub(crate) dimension: usize,
    /// Collections already created or validated, so each is checked once per process.
    ensured: Mutex<HashMap<String, Arc<OnceCell<()>>>>,
}

impl VectorStore {
    pub fn new(url: &str, dimension: usize) -> Result<Self, StoreError> {
        let client = Qdrant::from_url(url)
            .timeout(QDRANT_CLIENT_TIMEOUT)
            .skip_compatibility_check()
            .build()
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(Self {
            client,
            dimension,
            ensured: Mutex::default(),
        })
    }

    fn collection_name(namespace: &str) -> Result<String, StoreError> {
        validate_namespace(namespace).map_err(StoreError::Qdrant)?;
        Ok(format!("ferrex_{namespace}"))
    }

    pub async fn ensure_collection(&self, namespace: &str) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace)?;
        let cell = {
            let mut ensured = self.ensured.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(ensured.entry(name.clone()).or_default())
        };
        cell.get_or_try_init(|| self.create_or_validate(&name))
            .await
            .map(|&()| ())
    }

    pub(crate) fn forget_ensured(&self, name: &str) {
        self.ensured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }

    async fn create_or_validate(&self, name: &str) -> Result<(), StoreError> {
        let exists = self
            .client
            .collection_exists(name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        if !exists {
            self.create_hybrid_collection(name).await?;
        }
        let indexed = self.validate_collection_schema(name).await?;
        // Re-checked every time: a crash after creating the collection can leave indexes missing.
        for (field, field_type) in INDEXED_FIELDS {
            if indexed.iter().any(|f| f == field) {
                continue;
            }
            self.client
                .create_field_index(
                    CreateFieldIndexCollectionBuilder::new(name, field, field_type).wait(true),
                )
                .await
                .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        }
        Ok(())
    }

    async fn create_hybrid_collection(&self, name: &str) -> Result<(), StoreError> {
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

        match self
            .client
            .create_collection(
                CreateCollectionBuilder::new(name)
                    .vectors_config(vectors_config)
                    .sparse_vectors_config(sparse_config),
            )
            .await
        {
            Ok(_) => Ok(()),
            // Another process created it between our existence check and now.
            Err(e) if e.to_string().contains("already exists") => Ok(()),
            Err(e) => Err(StoreError::Qdrant(e.to_string())),
        }
    }

    /// Checks the vector layout and returns the payload fields already indexed.
    async fn validate_collection_schema(&self, name: &str) -> Result<Vec<String>, StoreError> {
        let info = self
            .client
            .collection_info(name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?
            .result
            .ok_or_else(|| StoreError::Qdrant(format!("collection {name} returned no info")))?;

        let params = info
            .config
            .and_then(|c| c.params)
            .and_then(|p| p.vectors_config)
            .and_then(|vc| vc.config);
        let Some(qdrant_client::qdrant::vectors_config::Config::ParamsMap(map)) = params else {
            return Err(StoreError::Qdrant(format!(
                "collection {name} uses an incompatible schema (single vector). \
                 Hybrid search requires named vectors (dense + sparse). \
                 Delete the collection or start with a fresh database."
            )));
        };
        let dense_size = map.map.get(DENSE_VECTOR).map(|p| p.size);
        if dense_size != Some(self.dimension as u64) {
            return Err(StoreError::Qdrant(format!(
                "collection {name} has dense dimension {dense_size:?}, but the embedding \
                 model produces {}. Use the original model or start with a fresh database.",
                self.dimension
            )));
        }

        Ok(info.payload_schema.into_keys().collect())
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

    pub async fn upsert_batch_hybrid(
        &self,
        namespace: &str,
        points: Vec<BatchPoint<'_>>,
    ) -> Result<(), StoreError> {
        if points.is_empty() {
            return Ok(());
        }
        let name = Self::collection_name(namespace)?;
        let point_structs: Vec<PointStruct> = points
            .into_iter()
            .map(|p| {
                let vectors = NamedVectors::default()
                    .add_vector(DENSE_VECTOR, p.vector)
                    .add_vector(SPARSE_VECTOR, Document::new(p.content_text, BM25_TOKENIZER));
                PointStruct::new(p.id.to_string(), vectors, p.payload)
            })
            .collect();
        self.client
            .upsert_points(UpsertPointsBuilder::new(&name, point_structs).wait(true))
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

    pub async fn search_with_payload(
        &self,
        namespace: &str,
        vector: Vec<f32>,
        query_text: &str,
        limit: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<ScoredPayload>, StoreError> {
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
                Some(ScoredPayload {
                    id,
                    score: point.score,
                    payload: point.payload.into(),
                })
            })
            .collect())
    }

    /// Dense-only search; dedup needs actual cosine similarity, not RRF fusion scores.
    pub async fn search_dense(
        &self,
        namespace: &str,
        vector: Vec<f32>,
        limit: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        let name = Self::collection_name(namespace)?;

        let mut builder = QueryPointsBuilder::new(&name)
            .query(VectorInput::new_dense(vector))
            .using(DENSE_VECTOR)
            .limit(limit as u64)
            .with_payload(true);
        if let Some(f) = filter {
            builder = builder.filter(f);
        }

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
                Some((id, point.score))
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

    pub async fn delete_by_ids(&self, namespace: &str, ids: &[Uuid]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let name = Self::collection_name(namespace)?;
        let id_strings: Vec<String> = ids.iter().map(ToString::to_string).collect();
        self.client
            .delete_points(
                DeletePointsBuilder::new(&name)
                    .points(id_strings)
                    .wait(true),
            )
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }

    /// Ids of memory points only; entity points share the collection.
    pub async fn scroll_memory_ids(&self, namespace: &str) -> Result<Vec<Uuid>, StoreError> {
        let name = Self::collection_name(namespace)?;
        let memories_only = Filter::must([Condition::matches(
            crate::QdrantField::POINT_TYPE,
            crate::PointType::MEMORY.to_string(),
        )]);
        let mut ids = Vec::new();
        let mut offset: Option<qdrant_client::qdrant::PointId> = None;
        loop {
            let mut req = ScrollPointsBuilder::new(&name)
                .filter(memories_only.clone())
                .limit(SCROLL_PAGE_SIZE)
                .with_payload(false);
            if let Some(o) = offset.clone() {
                req = req.offset(o);
            }
            let resp = self
                .client
                .scroll(req)
                .await
                .map_err(|e| StoreError::Qdrant(e.to_string()))?;
            for point in &resp.result {
                if let Some(id) = point.id.clone()
                    && let Some(PointIdOptions::Uuid(s)) = id.point_id_options
                    && let Ok(uuid) = s.parse::<Uuid>()
                {
                    ids.push(uuid);
                }
            }
            match resp.next_page_offset {
                Some(next) => offset = Some(next),
                None => break,
            }
        }
        Ok(ids)
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
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const TEST_DIM: usize = 384;

    fn test_vector() -> Vec<f32> {
        vec![0.1; TEST_DIM]
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn test_upsert_and_search() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
        let ns = "test_hybrid_upsert_search";
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let content = "test memory about Rust programming";
        let payload = Payload::try_from(serde_json::json!({
            "memory_id": id.to_string(),
            "memory_type": "episodic",
            "namespace": ns,
            "searchable_text": content,
            "entities": Vec::<String>::new(),
            "created_at": "2026-01-01T00:00:00Z",
            crate::QdrantField::POINT_TYPE: crate::PointType::MEMORY,
        }))
        .unwrap();

        store
            .upsert(ns, id, test_vector(), content, payload)
            .await
            .unwrap();

        let filter = Filter::must([Condition::matches(
            crate::QdrantField::POINT_TYPE,
            crate::PointType::MEMORY.to_string(),
        )]);
        let results = store
            .search(ns, test_vector(), "Rust programming", 10, Some(filter))
            .await
            .unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn test_delete() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
        let ns = "test_hybrid_delete";
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let payload = Payload::try_from(serde_json::json!({
            crate::QdrantField::POINT_TYPE: crate::PointType::MEMORY,
        }))
        .unwrap();
        store
            .upsert(ns, id, test_vector(), "test content", payload)
            .await
            .unwrap();
        store.delete(ns, id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn test_delete_by_ids_removes_multiple_points() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
        let ns = "test_delete_by_ids";
        store.ensure_collection(ns).await.unwrap();
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        for id in &ids {
            let payload = Payload::try_from(serde_json::json!({
                crate::QdrantField::POINT_TYPE: crate::PointType::MEMORY,
            }))
            .unwrap();
            store
                .upsert(ns, *id, test_vector(), "x", payload)
                .await
                .unwrap();
        }
        store.delete_by_ids(ns, &ids).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn test_bm25_keyword_contribution() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
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
            "searchable_text": content,
            "entities": Vec::<String>::new(),
            "created_at": "2026-01-01T00:00:00Z",
            crate::QdrantField::POINT_TYPE: crate::PointType::MEMORY,
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

    async fn sidecar() -> (crate::QdrantSidecar, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let port = crate::testing::free_port();
        let sidecar = crate::QdrantSidecar::start(
            "qdrant",
            port,
            Some(dir.path().to_path_buf()),
            crate::SidecarPolicy::KillOnDrop,
        )
        .await
        .unwrap();
        (sidecar, dir)
    }

    const INDEXED_FIELD_COUNT: usize = 6;

    async fn indexed_fields(store: &VectorStore, name: &str) -> Vec<String> {
        let info = store.client.collection_info(name).await.unwrap();
        let mut fields: Vec<String> = info.result.unwrap().payload_schema.into_keys().collect();
        fields.sort();
        fields
    }

    /// Index creation finishes asynchronously, so poll for the expected count.
    async fn wait_for_indexes(store: &VectorStore, name: &str, count: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let fields = indexed_fields(store, name).await;
            if fields.len() == count || std::time::Instant::now() > deadline {
                return fields;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn ensure_collection_adds_indexes_missing_after_partial_create() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
        let ns = "partial";
        store.ensure_collection(ns).await.unwrap();
        let full = wait_for_indexes(&store, "ferrex_partial", INDEXED_FIELD_COUNT).await;
        assert_eq!(full.len(), INDEXED_FIELD_COUNT, "{full:?}");

        // Simulate a crash between creating the collection and its indexes.
        for field in &full {
            store
                .client
                .delete_field_index(
                    qdrant_client::qdrant::DeleteFieldIndexCollectionBuilder::new(
                        "ferrex_partial",
                        field,
                    ),
                )
                .await
                .unwrap();
        }
        assert!(
            wait_for_indexes(&store, "ferrex_partial", 0)
                .await
                .is_empty()
        );
        let fresh = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
        fresh.ensure_collection(ns).await.unwrap();

        assert_eq!(
            wait_for_indexes(&fresh, "ferrex_partial", INDEXED_FIELD_COUNT).await,
            full
        );
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn ensure_collection_rejects_dimension_mismatch() {
        let (sidecar, _dir) = sidecar().await;
        VectorStore::new(&sidecar.url(), TEST_DIM)
            .unwrap()
            .ensure_collection("dims")
            .await
            .unwrap();

        let err = VectorStore::new(&sidecar.url(), TEST_DIM * 2)
            .unwrap()
            .ensure_collection("dims")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("dimension"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires qdrant binary on PATH"]
    async fn concurrent_ensure_collection_from_two_processes_succeeds() {
        let (sidecar, _dir) = sidecar().await;
        for i in 0..10 {
            let a = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
            let b = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
            let ns = format!("race{i}");
            let (ra, rb) = tokio::join!(a.ensure_collection(&ns), b.ensure_collection(&ns));
            ra.unwrap();
            rb.unwrap();
        }
    }

    fn memory_payload(id: Uuid, ns: &str, text: &str) -> Payload {
        Payload::try_from(serde_json::json!({
            "memory_id": id.to_string(),
            "memory_type": "episodic",
            "namespace": ns,
            "searchable_text": text,
            crate::QdrantField::POINT_TYPE: crate::PointType::MEMORY,
        }))
        .unwrap()
    }

    #[test]
    fn validate_namespace_rules() {
        assert!(validate_namespace("ok_name-1").is_ok());
        assert!(validate_namespace("").is_err());
        let err = validate_namespace("bad name").unwrap_err();
        assert!(err.contains("bad name"), "{err}");
        assert!(VectorStore::collection_name("a/b").is_err());
        assert_eq!(VectorStore::collection_name("ns").unwrap(), "ferrex_ns");
    }

    #[tokio::test]
    async fn invalid_namespace_fails_before_any_request() {
        let store = VectorStore::new("http://127.0.0.1:1", TEST_DIM).unwrap();
        let id = Uuid::now_v7();
        let bad = "no spaces";
        assert!(store.ensure_collection(bad).await.is_err());
        assert!(
            store
                .upsert(bad, id, test_vector(), "x", Payload::new())
                .await
                .is_err()
        );
        assert!(
            store
                .search(bad, test_vector(), "x", 1, None)
                .await
                .is_err()
        );
        assert!(
            store
                .search_dense(bad, test_vector(), 1, None)
                .await
                .is_err()
        );
        assert!(store.delete(bad, id).await.is_err());
        assert!(store.delete_by_ids(bad, &[id]).await.is_err());
        assert!(store.scroll_memory_ids(bad).await.is_err());
    }

    #[tokio::test]
    async fn empty_batches_are_noops() {
        let store = VectorStore::new("http://127.0.0.1:1", TEST_DIM).unwrap();
        store.upsert_batch_hybrid("ns", Vec::new()).await.unwrap();
        store.delete_by_ids("ns", &[]).await.unwrap();
        store.upsert_batch("ferrex_ns", Vec::new()).await.unwrap();
    }

    #[tokio::test]
    async fn unreachable_qdrant_reports_errors() {
        let store = VectorStore::new("http://127.0.0.1:1", TEST_DIM).unwrap();
        assert!(store.health_check().await.is_err());
        assert!(store.ensure_collection("ns").await.is_err());
        assert!(store.create_collection("c", TEST_DIM).await.is_err());
        assert!(store.delete_collection("c").await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn hybrid_batch_payload_dense_and_scroll() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), TEST_DIM).unwrap();
        store.health_check().await.unwrap();
        let ns = "batch";
        store.ensure_collection(ns).await.unwrap();

        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        let texts = ["alpha rust", "beta tokio", "gamma qdrant"];
        let points = ids
            .iter()
            .zip(texts)
            .map(|(id, text)| BatchPoint {
                id: *id,
                vector: test_vector(),
                content_text: text,
                payload: memory_payload(*id, ns, text),
            })
            .collect();
        store.upsert_batch_hybrid(ns, points).await.unwrap();

        let mut scrolled = store.scroll_memory_ids(ns).await.unwrap();
        scrolled.sort();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(scrolled, expected);

        let hits = store
            .search_with_payload(ns, test_vector(), "tokio", 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
        assert!(
            hits.iter()
                .all(|h| { serde_json::to_value(&h.payload).unwrap()["memory_id"].is_string() })
        );

        let filter = Filter::must([Condition::matches(
            crate::QdrantField::POINT_TYPE,
            crate::PointType::MEMORY.to_string(),
        )]);
        let dense = store
            .search_dense(ns, test_vector(), 2, Some(filter))
            .await
            .unwrap();
        assert_eq!(dense.len(), 2);

        store.delete_by_ids(ns, &ids[..2]).await.unwrap();
        assert_eq!(store.scroll_memory_ids(ns).await.unwrap(), vec![ids[2]]);
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn plain_collection_lifecycle() {
        let (sidecar, _dir) = sidecar().await;
        let store = VectorStore::new(&sidecar.url(), 4).unwrap();
        let name = "plain";
        store.create_collection(name, 4).await.unwrap();
        // A second create is a no-op on an existing collection.
        store.create_collection(name, 4).await.unwrap();

        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let payload = || Payload::try_from(serde_json::json!({"kind": "x"})).unwrap();
        store
            .upsert_batch(
                name,
                vec![
                    (a, vec![1.0, 0.0, 0.0, 0.0], payload()),
                    (b, vec![0.0, 1.0, 0.0, 0.0], payload()),
                ],
            )
            .await
            .unwrap();

        let hits = store
            .search_collection(name, vec![1.0, 0.0, 0.0, 0.0], 1, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, a.to_string());

        let filter = Filter::must([Condition::matches("kind", "y".to_string())]);
        let none = store
            .search_collection(name, vec![1.0, 0.0, 0.0, 0.0], 5, Some(filter))
            .await
            .unwrap();
        assert!(none.is_empty());

        store.delete_collection(name).await.unwrap();
        assert!(!store.client.collection_exists(name).await.unwrap());
        assert!(
            store
                .search_collection(name, vec![1.0, 0.0, 0.0, 0.0], 1, None)
                .await
                .is_err()
        );
    }
}
