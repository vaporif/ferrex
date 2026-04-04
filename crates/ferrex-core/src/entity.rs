use chrono::Utc;
use ferrex_embed::Embedder;
use ferrex_store::{Entity, MetadataStore, VectorStore};
use qdrant_client::Payload;
use qdrant_client::qdrant::{Condition, Filter};
use uuid::Uuid;

use crate::CoreError;

pub struct EntityResolver<'a, M: MetadataStore> {
    pub metadata_store: &'a M,
    pub vector_store: &'a VectorStore,
    pub embedder: &'a Embedder,
}

impl<M: MetadataStore> EntityResolver<'_, M> {
    pub async fn resolve(
        &self,
        entity_names: &[String],
        namespace: &str,
    ) -> Result<Vec<Entity>, CoreError> {
        let mut resolved = Vec::new();
        for name in entity_names {
            let entity = self.resolve_single(name, namespace).await?;
            resolved.push(entity);
        }
        Ok(resolved)
    }

    async fn resolve_single(&self, name: &str, namespace: &str) -> Result<Entity, CoreError> {
        let normalized = normalize(name);
        if normalized.is_empty() {
            return Err(CoreError::Validation("empty entity name".to_string()));
        }

        // Stage 1: exact match (canonical name + aliases)
        if let Some(entity) = self.metadata_store.get_entity_by_name(&normalized).await? {
            return Ok(entity);
        }

        // Stage 2: fuzzy match (Jaro-Winkler > 0.85)
        let all_entities = self.metadata_store.get_all_entities().await?;
        if let Some((entity, _score)) = best_fuzzy_match(&normalized, &all_entities, 0.85) {
            self.metadata_store
                .add_entity_alias(&entity.id, &normalized)
                .await?;
            return Ok(entity);
        }

        // Stage 3: embedding similarity > 0.92
        let embedding = self.embedder.embed(&normalized).await?;
        let filter = Filter::must([Condition::matches("point_type", "entity".to_string())]);
        let results = self
            .vector_store
            .search(namespace, embedding.clone(), 1, Some(filter))
            .await?;

        if let Some((point_id, score)) = results.first() {
            if *score > 0.92
                && let Some(entity) = find_entity_by_point_id(&all_entities, point_id)
            {
                self.metadata_store
                    .add_entity_alias(&entity.id, &normalized)
                    .await?;
                return Ok(entity);
            }

            // Stage 4: ambiguous (0.80-0.92)
            if *score > 0.80 {
                let new_entity = self.create_entity(&normalized).await?;
                self.upsert_entity_point(&new_entity, namespace, embedding)
                    .await?;
                if let Some(near_match) = find_entity_by_point_id(&all_entities, point_id) {
                    self.metadata_store
                        .add_entity_alias(&new_entity.id, &near_match.name)
                        .await?;
                }
                return Ok(new_entity);
            }
        }

        // Stage 5: no match — create new
        let new_entity = self.create_entity(&normalized).await?;
        self.upsert_entity_point(&new_entity, namespace, embedding)
            .await?;
        Ok(new_entity)
    }

    async fn create_entity(&self, name: &str) -> Result<Entity, CoreError> {
        let now = Utc::now();
        let entity = Entity {
            id: Uuid::now_v7().to_string(),
            name: name.to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        self.metadata_store.insert_entity(&entity).await?;
        Ok(entity)
    }

    async fn upsert_entity_point(
        &self,
        entity: &Entity,
        namespace: &str,
        embedding: Vec<f32>,
    ) -> Result<(), CoreError> {
        let id: Uuid = entity
            .id
            .parse()
            .map_err(|e| CoreError::Validation(format!("invalid entity UUID: {e}")))?;
        let payload = Payload::try_from(serde_json::json!({
            "entity_id": entity.id,
            "name": entity.name,
            "point_type": "entity",
            "namespace": namespace,
        }))
        .map_err(|e| CoreError::Validation(e.to_string()))?;

        self.vector_store
            .upsert(namespace, id, embedding, payload)
            .await?;
        Ok(())
    }
}

fn normalize(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .replace(['-', '_', '/'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn best_fuzzy_match(name: &str, entities: &[Entity], threshold: f64) -> Option<(Entity, f64)> {
    let mut best: Option<(Entity, f64)> = None;
    for entity in entities {
        let score = strsim::jaro_winkler(&entity.name, name);
        if score > threshold && best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((entity.clone(), score));
        }
        for alias in &entity.aliases {
            let score = strsim::jaro_winkler(alias, name);
            if score > threshold && best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((entity.clone(), score));
            }
        }
    }
    best
}

fn find_entity_by_point_id(entities: &[Entity], point_id: &str) -> Option<Entity> {
    entities.iter().find(|e| e.id == point_id).cloned()
}

/// Stages 1-2 only, for unit testing without Qdrant.
#[cfg(test)]
async fn resolve_entities_stages_1_2<M: MetadataStore>(
    metadata_store: &M,
    entity_names: &[String],
) -> Result<Vec<Entity>, CoreError> {
    let mut resolved = Vec::new();
    for name in entity_names {
        let normalized = normalize(name);
        if normalized.is_empty() {
            return Err(CoreError::Validation("empty entity name".to_string()));
        }

        // Stage 1
        if let Some(entity) = metadata_store.get_entity_by_name(&normalized).await? {
            resolved.push(entity);
            continue;
        }

        // Stage 2
        let all_entities = metadata_store.get_all_entities().await?;
        if let Some((entity, _)) = best_fuzzy_match(&normalized, &all_entities, 0.85) {
            metadata_store
                .add_entity_alias(&entity.id, &normalized)
                .await?;
            resolved.push(entity);
            continue;
        }

        // Create new (stages 3-5 need Qdrant)
        let now = Utc::now();
        let entity = Entity {
            id: Uuid::now_v7().to_string(),
            name: normalized,
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        metadata_store.insert_entity(&entity).await?;
        resolved.push(entity);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrex_store::SqliteStore;

    #[tokio::test]
    async fn test_resolve_exact_match() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = chrono::Utc::now();
        let entity = ferrex_store::Entity {
            id: uuid::Uuid::now_v7().to_string(),
            name: "rust-lang".to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();

        let resolved = resolve_entities_stages_1_2(&store, &["  Rust-Lang  ".to_string()])
            .await
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "rust-lang");
    }

    #[tokio::test]
    async fn test_resolve_separator_normalization() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = chrono::Utc::now();
        let entity = ferrex_store::Entity {
            id: uuid::Uuid::now_v7().to_string(),
            name: "tokio runtime".to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();

        let resolved = resolve_entities_stages_1_2(&store, &["tokio-runtime".to_string()])
            .await
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "tokio runtime");
    }

    #[tokio::test]
    async fn test_resolve_fuzzy_match() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = chrono::Utc::now();
        let entity = ferrex_store::Entity {
            id: uuid::Uuid::now_v7().to_string(),
            name: "serde json".to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();

        let resolved = resolve_entities_stages_1_2(&store, &["serdejson".to_string()])
            .await
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "serde json");
    }

    #[tokio::test]
    async fn test_resolve_creates_new() {
        let store = SqliteStore::open(":memory:").unwrap();

        let resolved = resolve_entities_stages_1_2(&store, &["brand-new-entity".to_string()])
            .await
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "brand new entity");

        let fetched = store.get_entity_by_name("brand new entity").await.unwrap();
        assert!(fetched.is_some());
    }
}
