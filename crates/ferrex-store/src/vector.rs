use qdrant_client::qdrant::{
    CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder, Distance,
    FieldType, Filter, PointStruct, QueryPointsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
    point_id::PointIdOptions,
};
use qdrant_client::{Payload, Qdrant};
use uuid::Uuid;

use crate::StoreError;

pub struct VectorStore {
    client: Qdrant,
    dimension: usize,
}

impl VectorStore {
    pub fn new(url: &str, dimension: usize) -> Result<Self, StoreError> {
        let client = Qdrant::from_url(url)
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(Self { client, dimension })
    }

    fn collection_name(namespace: &str) -> String {
        format!("ferrex_{namespace}")
    }

    pub async fn ensure_collection(&self, namespace: &str) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace);

        if self
            .client
            .collection_exists(&name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?
        {
            return Ok(());
        }

        self.client
            .create_collection(CreateCollectionBuilder::new(&name).vectors_config(
                VectorParamsBuilder::new(self.dimension as u64, Distance::Cosine),
            ))
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;

        for field in ["memory_type", "namespace", "entities", "point_type"] {
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

    pub async fn upsert(
        &self,
        namespace: &str,
        id: Uuid,
        vector: Vec<f32>,
        payload: Payload,
    ) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace);
        let point = PointStruct::new(id.to_string(), vector, payload);
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
        limit: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        let name = Self::collection_name(namespace);
        let mut builder = QueryPointsBuilder::new(&name)
            .query(vector)
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
                let score = point.score;
                Some((id, score))
            })
            .collect())
    }

    pub async fn delete(&self, namespace: &str, id: Uuid) -> Result<(), StoreError> {
        let name = Self::collection_name(namespace);
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
        let ns = "test_upsert_search";
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let payload = Payload::try_from(serde_json::json!({
            "memory_id": id.to_string(),
            "memory_type": "episodic",
            "namespace": ns,
            "content": "test memory",
            "entities": Vec::<String>::new(),
            "created_at": "2026-01-01T00:00:00Z",
            "point_type": "memory",
        }))
        .unwrap();

        store.upsert(ns, id, test_vector(), payload).await.unwrap();

        let filter = Filter::must([Condition::matches("point_type", "memory".to_string())]);
        let results = store
            .search(ns, test_vector(), 10, Some(filter))
            .await
            .unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires running Qdrant"]
    async fn test_delete() {
        let store = VectorStore::new("http://localhost:6334", TEST_DIM).unwrap();
        let ns = "test_delete";
        store.ensure_collection(ns).await.unwrap();

        let id = Uuid::now_v7();
        let payload = Payload::try_from(serde_json::json!({
            "point_type": "memory",
        }))
        .unwrap();
        store.upsert(ns, id, test_vector(), payload).await.unwrap();
        store.delete(ns, id).await.unwrap();
    }
}
