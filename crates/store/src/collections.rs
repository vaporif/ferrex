use qdrant_client::Payload;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, Distance, Filter, NamedVectors, PointStruct, QueryPointsBuilder,
    UpsertPointsBuilder, VectorInput, VectorParamsBuilder, VectorsConfigBuilder,
    point_id::PointIdOptions,
};
use uuid::Uuid;

use crate::StoreError;
use crate::vector::{DENSE_VECTOR, VectorStore};

impl VectorStore {
    pub async fn create_collection(&self, name: &str, dimension: usize) -> Result<(), StoreError> {
        if self
            .client
            .collection_exists(name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?
        {
            return Ok(());
        }

        let mut vectors_config = VectorsConfigBuilder::default();
        vectors_config.add_named_vector_params(
            DENSE_VECTOR,
            VectorParamsBuilder::new(dimension as u64, Distance::Cosine),
        );

        self.client
            .create_collection(CreateCollectionBuilder::new(name).vectors_config(vectors_config))
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }

    pub async fn upsert_batch(
        &self,
        collection: &str,
        points: Vec<(Uuid, Vec<f32>, Payload)>,
    ) -> Result<(), StoreError> {
        if points.is_empty() {
            return Ok(());
        }
        let point_structs: Vec<PointStruct> = points
            .into_iter()
            .map(|(id, vector, payload)| {
                let vectors = NamedVectors::default().add_vector(DENSE_VECTOR, vector);
                PointStruct::new(id.to_string(), vectors, payload)
            })
            .collect();
        self.client
            .upsert_points(UpsertPointsBuilder::new(collection, point_structs).wait(true))
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }

    pub async fn search_collection(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<(String, f32)>, StoreError> {
        let mut builder = QueryPointsBuilder::new(collection)
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

    pub async fn delete_collection(&self, name: &str) -> Result<(), StoreError> {
        self.client
            .delete_collection(name)
            .await
            .map_err(|e| StoreError::Qdrant(e.to_string()))?;
        Ok(())
    }
}
