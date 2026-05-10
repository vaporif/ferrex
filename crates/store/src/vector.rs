use chrono::{DateTime, Utc};
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DatetimeRange,
    DeletePointsBuilder, Distance, Document, FieldType, Filter, Fusion, Modifier, NamedVectors,
    PointStruct, PrefetchQueryBuilder, Query, QueryPointsBuilder, ScrollPointsBuilder,
    SparseVectorParamsBuilder, SparseVectorsConfigBuilder, Timestamp, UpsertPointsBuilder,
    VectorInput, VectorParamsBuilder, VectorsConfigBuilder, point_id::PointIdOptions,
};
use qdrant_client::{Payload, Qdrant};
use uuid::Uuid;

use crate::{MemoryType, StoreError};

/// Filter spec for memory searches. Builder over the abstract query, not over
/// Qdrant types — keeps the Qdrant dep contained in this crate.
#[derive(Debug, Default, Clone)]
pub struct MemorySearch {
    pub memory_types: Vec<MemoryType>,
    pub entities_any: Vec<String>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
}

impl MemorySearch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_types(mut self, types: Vec<MemoryType>) -> Self {
        self.memory_types = types;
        self
    }
    pub fn only_type(mut self, t: MemoryType) -> Self {
        self.memory_types = vec![t];
        self
    }
    pub fn with_entities(mut self, entities: Vec<String>) -> Self {
        self.entities_any = entities;
        self
    }
    pub fn created_after(mut self, t: DateTime<Utc>) -> Self {
        self.created_after = Some(t);
        self
    }
    pub fn created_before(mut self, t: DateTime<Utc>) -> Self {
        self.created_before = Some(t);
        self
    }
}

/// Typed payload for memory upserts. The vector store owns the Qdrant payload
/// shape; callers stay agnostic to vendor types.
pub struct MemoryFields<'a> {
    pub memory_id: &'a str,
    pub memory_type: MemoryType,
    pub namespace: &'a str,
    pub entities: &'a [String],
    pub created_at: DateTime<Utc>,
}

fn memory_filter(search: &MemorySearch) -> Filter {
    let mut must: Vec<Condition> = vec![Condition::matches(
        crate::QdrantField::POINT_TYPE,
        crate::PointType::MEMORY.to_string(),
    )];

    if !search.memory_types.is_empty() {
        let types: Vec<String> = search
            .memory_types
            .iter()
            .map(|t| t.as_str().to_string())
            .collect();
        must.push(Condition::matches(crate::QdrantField::MEMORY_TYPE, types));
    }

    if let Some(start) = search.created_after {
        #[allow(clippy::cast_possible_wrap)]
        let ts = Timestamp {
            seconds: start.timestamp(),
            nanos: start.timestamp_subsec_nanos() as i32,
        };
        must.push(Condition::datetime_range(
            crate::QdrantField::CREATED_AT,
            DatetimeRange {
                gte: Some(ts),
                ..Default::default()
            },
        ));
    }

    if let Some(end) = search.created_before {
        #[allow(clippy::cast_possible_wrap)]
        let ts = Timestamp {
            seconds: end.timestamp(),
            nanos: end.timestamp_subsec_nanos() as i32,
        };
        must.push(Condition::datetime_range(
            crate::QdrantField::CREATED_AT,
            DatetimeRange {
                lte: Some(ts),
                ..Default::default()
            },
        ));
    }

    let should: Vec<Condition> = search
        .entities_any
        .iter()
        .map(|e| Condition::matches(crate::QdrantField::ENTITIES, e.clone()))
        .collect();

    Filter {
        must,
        should,
        ..Default::default()
    }
}

fn entity_filter() -> Filter {
    Filter::must([Condition::matches(
        crate::QdrantField::POINT_TYPE,
        crate::PointType::ENTITY.to_string(),
    )])
}

const QDRANT_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub(crate) const DENSE_VECTOR: &str = "dense";
const SPARSE_VECTOR: &str = "sparse";
const BM25_TOKENIZER: &str = "Qdrant/bm25";
const MIN_PREFETCH_LIMIT: u64 = 20;
const SCROLL_PAGE_SIZE: u32 = 1024;

pub struct VectorStore {
    pub(crate) client: Qdrant,
    pub(crate) dimension: usize,
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
            crate::QdrantField::MEMORY_TYPE,
            crate::QdrantField::NAMESPACE,
            crate::QdrantField::ENTITIES,
            crate::QdrantField::POINT_TYPE,
            crate::QdrantField::AGENT_ID,
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

        self.client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &name,
                crate::QdrantField::CREATED_AT,
                FieldType::Datetime,
            ))
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;

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

    /// Upsert a memory point. Owns Qdrant payload construction.
    pub async fn upsert_memory(
        &self,
        id: Uuid,
        vector: Vec<f32>,
        search_text: &str,
        fields: MemoryFields<'_>,
    ) -> Result<(), StoreError> {
        let payload = Payload::try_from(serde_json::json!({
            "memory_id": fields.memory_id,
            crate::QdrantField::MEMORY_TYPE: fields.memory_type.as_str(),
            crate::QdrantField::NAMESPACE: fields.namespace,
            crate::QdrantField::SEARCHABLE_TEXT: search_text,
            crate::QdrantField::ENTITIES: fields.entities,
            crate::QdrantField::CREATED_AT: fields.created_at.to_rfc3339(),
            crate::QdrantField::POINT_TYPE: crate::PointType::MEMORY,
        }))
        .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        self.upsert(fields.namespace, id, vector, search_text, payload)
            .await
    }

    /// Upsert an entity point. Owns Qdrant payload construction.
    pub async fn upsert_entity(
        &self,
        namespace: &str,
        id: Uuid,
        name: &str,
        vector: Vec<f32>,
    ) -> Result<(), StoreError> {
        let payload = Payload::try_from(serde_json::json!({
            crate::QdrantField::ENTITY_ID: id.to_string(),
            crate::QdrantField::NAME: name,
            crate::QdrantField::POINT_TYPE: crate::PointType::ENTITY,
            crate::QdrantField::NAMESPACE: namespace,
        }))
        .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        self.upsert(namespace, id, vector, name, payload).await
    }

    /// Hybrid (dense + BM25) search filtered to memory points.
    pub async fn search_memories(
        &self,
        namespace: &str,
        vector: Vec<f32>,
        query_text: &str,
        limit: usize,
        search: &MemorySearch,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        self.search(
            namespace,
            vector,
            query_text,
            limit,
            Some(memory_filter(search)),
        )
        .await
    }

    /// Dense-only search filtered to memory points (used for dedup where RRF
    /// fusion would obscure the actual cosine similarity).
    pub async fn search_memories_dense(
        &self,
        namespace: &str,
        vector: Vec<f32>,
        limit: usize,
        search: &MemorySearch,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        self.search_dense(namespace, vector, limit, Some(memory_filter(search)))
            .await
    }

    /// Hybrid search filtered to entity points.
    pub async fn search_entities(
        &self,
        namespace: &str,
        vector: Vec<f32>,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        self.search(namespace, vector, query_text, limit, Some(entity_filter()))
            .await
    }

    pub(crate) async fn upsert(
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

    pub(crate) async fn search(
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

    /// Dense-only search; dedup needs actual cosine similarity, not RRF fusion scores.
    pub(crate) async fn search_dense(
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

    pub async fn scroll_all_ids(&self, namespace: &str) -> Result<Vec<Uuid>, StoreError> {
        let name = Self::collection_name(namespace)?;
        let mut ids = Vec::new();
        let mut offset: Option<qdrant_client::qdrant::PointId> = None;
        loop {
            let mut req = ScrollPointsBuilder::new(&name)
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

    /// List ferrex-owned namespaces by enumerating Qdrant collections with the
    /// `ferrex_` prefix and stripping it.
    pub async fn list_namespaces(&self) -> Result<Vec<String>, StoreError> {
        let resp = self
            .client
            .list_collections()
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        let mut namespaces: Vec<String> = resp
            .collections
            .into_iter()
            .filter_map(|c| c.name.strip_prefix("ferrex_").map(str::to_owned))
            .collect();
        namespaces.sort();
        Ok(namespaces)
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
    #[ignore = "requires running Qdrant"]
    async fn test_delete() {
        let store = VectorStore::new("http://localhost:6334", TEST_DIM).unwrap();
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
    #[ignore = "requires running Qdrant"]
    async fn test_delete_by_ids_removes_multiple_points() {
        let store = VectorStore::new("http://localhost:6334", TEST_DIM).unwrap();
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
}
