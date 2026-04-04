use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::schema::migrate;
use crate::{Entity, Memory, MemoryType, StoreError};

pub trait MetadataStore: Send + Sync {
    fn insert_memory(&self, memory: &Memory)
    -> impl Future<Output = Result<(), StoreError>> + Send;
    fn get_memory(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<Memory>, StoreError>> + Send;
    fn get_memories_by_ids(
        &self,
        ids: &[String],
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send;
    fn update_last_accessed(
        &self,
        ids: &[String],
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn insert_entity(&self, entity: &Entity)
    -> impl Future<Output = Result<(), StoreError>> + Send;
    fn get_entity_by_name(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<Option<Entity>, StoreError>> + Send;
    fn get_all_entities(&self) -> impl Future<Output = Result<Vec<Entity>, StoreError>> + Send;
    fn add_entity_alias(
        &self,
        entity_id: &str,
        alias: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn link_memory_entity(
        &self,
        memory_id: &str,
        entity_id: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn get_metadata(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<String>, StoreError>> + Send;
    fn set_metadata(
        &self,
        key: &str,
        value: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn memory_count(&self) -> impl Future<Output = Result<u64, StoreError>> + Send;
    fn recent_memories(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send;

    // --- Phase 3 stubs ---
    fn delete_memory(&self, _id: &str) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async { Ok(false) }
    }
    fn delete_memories(
        &self,
        _ids: &[String],
    ) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }
    fn get_memories_by_subject_predicate(
        &self,
        _subject: &str,
        _predicate: &str,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn invalidate_memory(
        &self,
        _id: &str,
        _t_invalid: DateTime<Utc>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }

    // --- Phase 4 stubs ---
    fn get_stale_memories(
        &self,
        _threshold_days: u64,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn get_unvalidated_memories(
        &self,
        _since: DateTime<Utc>,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn get_low_access_memories(
        &self,
        _limit: usize,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn update_last_validated(
        &self,
        _ids: &[String],
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }
    fn memory_count_by_type(
        &self,
    ) -> impl Future<Output = Result<HashMap<String, u64>, StoreError>> + Send {
        async { Ok(HashMap::new()) }
    }
    fn storage_size_bytes(&self) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }
    fn entity_count(&self) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }
}

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = if path.as_ref().to_str() == Some(":memory:") {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("lock poisoned");
            f(&conn)
        })
        .await
        .map_err(|e| StoreError::TaskJoin(e.to_string()))?
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_default()
}

fn parse_optional_dt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| s.parse::<DateTime<Utc>>().ok())
}

fn row_to_memory(
    row: &rusqlite::Row<'_>,
    entities: Vec<String>,
) -> Result<Memory, rusqlite::Error> {
    Ok(Memory {
        id: row.get("id")?,
        namespace: row.get("namespace")?,
        memory_type: row
            .get::<_, String>("memory_type")?
            .parse::<MemoryType>()
            .unwrap_or(MemoryType::Episodic),
        content: row.get("content")?,
        subject: row.get("subject")?,
        predicate: row.get("predicate")?,
        object: row.get("object")?,
        confidence: row.get("confidence")?,
        source: row.get("source")?,
        context: row
            .get::<_, Option<String>>("context")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        entities,
        created_at: parse_dt(&row.get::<_, String>("created_at")?),
        updated_at: parse_dt(&row.get::<_, String>("updated_at")?),
        t_valid: parse_optional_dt(row.get("t_valid")?),
        t_invalid: parse_optional_dt(row.get("t_invalid")?),
        last_accessed: parse_dt(&row.get::<_, String>("last_accessed")?),
        last_validated: parse_optional_dt(row.get("last_validated")?),
        access_count: row.get::<_, i64>("access_count")?.cast_unsigned(),
    })
}

fn row_to_entity(row: &rusqlite::Row<'_>) -> Result<Entity, rusqlite::Error> {
    let aliases_str: String = row.get("aliases")?;
    let aliases: Vec<String> = serde_json::from_str(&aliases_str).unwrap_or_default();
    Ok(Entity {
        id: row.get("id")?,
        name: row.get("name")?,
        aliases,
        entity_type: row.get("entity_type")?,
        created_at: parse_dt(&row.get::<_, String>("created_at")?),
        updated_at: parse_dt(&row.get::<_, String>("updated_at")?),
    })
}

fn get_entity_names_for_memory(
    conn: &Connection,
    memory_id: &str,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT e.name FROM entities e
         INNER JOIN memory_entities me ON e.id = me.entity_id
         WHERE me.memory_id = ?1",
    )?;
    let names: Vec<String> = stmt
        .query_map(rusqlite::params![memory_id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(names)
}

#[allow(clippy::needless_pass_by_value)]
impl MetadataStore for SqliteStore {
    async fn insert_memory(&self, memory: &Memory) -> Result<(), StoreError> {
        let memory = memory.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO memories (id, namespace, memory_type, content, subject, predicate, object, confidence, source, context, created_at, updated_at, t_valid, t_invalid, last_accessed, last_validated, access_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                rusqlite::params![
                    memory.id,
                    memory.namespace,
                    memory.memory_type.as_str(),
                    memory.content,
                    memory.subject,
                    memory.predicate,
                    memory.object,
                    memory.confidence,
                    memory.source,
                    memory.context.as_ref().map(std::string::ToString::to_string),
                    memory.created_at.to_rfc3339(),
                    memory.updated_at.to_rfc3339(),
                    memory.t_valid.map(|d| d.to_rfc3339()),
                    memory.t_invalid.map(|d| d.to_rfc3339()),
                    memory.last_accessed.to_rfc3339(),
                    memory.last_validated.map(|d| d.to_rfc3339()),
                    memory.access_count.cast_signed(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_memory(&self, id: &str) -> Result<Option<Memory>, StoreError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let entities = get_entity_names_for_memory(conn, &id)?;
            let mut stmt = conn.prepare("SELECT * FROM memories WHERE id = ?1")?;
            let mut rows = stmt.query(rusqlite::params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(row_to_memory(row, entities)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn get_memories_by_ids(&self, ids: &[String]) -> Result<Vec<Memory>, StoreError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let ids = ids.to_vec();
        self.with_conn(move |conn| {
            let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT * FROM memories WHERE id IN ({})",
                placeholders.join(", ")
            );
            let params: Vec<&dyn rusqlite::types::ToSql> = ids
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(params.as_slice())?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                let mem_id: String = row.get("id")?;
                let entities = get_entity_names_for_memory(conn, &mem_id)?;
                memories.push(row_to_memory(row, entities)?);
            }
            Ok(memories)
        })
        .await
    }

    async fn update_last_accessed(&self, ids: &[String]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        self.with_conn(move |conn| {
            let now = Utc::now().to_rfc3339();
            let placeholders: Vec<String> =
                (1..=ids.len()).map(|i| format!("?{}", i + 1)).collect();
            let sql = format!(
                "UPDATE memories SET last_accessed = ?1, access_count = access_count + 1 WHERE id IN ({})",
                placeholders.join(", ")
            );
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::with_capacity(ids.len() + 1);
            params.push(Box::new(now));
            params.extend(ids.iter().map(|id| Box::new(id.clone()) as Box<dyn rusqlite::types::ToSql>));
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(std::convert::AsRef::as_ref).collect();
            conn.execute(&sql, param_refs.as_slice())?;
            Ok(())
        })
        .await
    }

    async fn insert_entity(&self, entity: &Entity) -> Result<(), StoreError> {
        let entity = entity.clone();
        self.with_conn(move |conn| {
            let aliases_json = serde_json::to_string(&entity.aliases)?;
            conn.execute(
                "INSERT INTO entities (id, name, aliases, entity_type, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    entity.id,
                    entity.name,
                    aliases_json,
                    entity.entity_type,
                    entity.created_at.to_rfc3339(),
                    entity.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_entity_by_name(&self, name: &str) -> Result<Option<Entity>, StoreError> {
        let name = name.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare("SELECT * FROM entities WHERE name = ?1")?;
            let mut rows = stmt.query(rusqlite::params![name])?;
            if let Some(row) = rows.next()? {
                return Ok(Some(row_to_entity(row)?));
            }

            let mut stmt = conn.prepare("SELECT * FROM entities")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let aliases_str: String = row.get("aliases")?;
                let aliases: Vec<String> = serde_json::from_str(&aliases_str).unwrap_or_default();
                if aliases.iter().any(|a| a == &name) {
                    return Ok(Some(row_to_entity(row)?));
                }
            }

            Ok(None)
        })
        .await
    }

    async fn get_all_entities(&self) -> Result<Vec<Entity>, StoreError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM entities")?;
            let mut rows = stmt.query([])?;
            let mut entities = Vec::new();
            while let Some(row) = rows.next()? {
                entities.push(row_to_entity(row)?);
            }
            Ok(entities)
        })
        .await
    }

    async fn add_entity_alias(&self, entity_id: &str, alias: &str) -> Result<(), StoreError> {
        let entity_id = entity_id.to_string();
        let alias = alias.to_string();
        self.with_conn(move |conn| {
            let now = Utc::now().to_rfc3339();
            let mut stmt = conn.prepare("SELECT aliases FROM entities WHERE id = ?1")?;
            let aliases_str: String =
                stmt.query_row(rusqlite::params![entity_id], |row| row.get(0))?;
            let mut aliases: Vec<String> = serde_json::from_str(&aliases_str).unwrap_or_default();
            if !aliases.contains(&alias) {
                aliases.push(alias);
            }
            let new_aliases = serde_json::to_string(&aliases)?;
            conn.execute(
                "UPDATE entities SET aliases = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![new_aliases, now, entity_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn link_memory_entity(&self, memory_id: &str, entity_id: &str) -> Result<(), StoreError> {
        let memory_id = memory_id.to_string();
        let entity_id = entity_id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id) VALUES (?1, ?2)",
                rusqlite::params![memory_id, entity_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StoreError> {
        let key = key.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key = ?1")?;
            let mut rows = stmt.query(rusqlite::params![key])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let key = key.to_string();
        let value = value.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )?;
            Ok(())
        })
        .await
    }

    async fn memory_count(&self) -> Result<u64, StoreError> {
        self.with_conn(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
            Ok(count.cast_unsigned())
        })
        .await
    }

    async fn recent_memories(&self, limit: usize) -> Result<Vec<Memory>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT * FROM memories ORDER BY created_at DESC LIMIT ?1")?;
            let mut rows = stmt.query(rusqlite::params![limit])?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                let mem_id: String = row.get("id")?;
                let entities = get_entity_names_for_memory(conn, &mem_id)?;
                memories.push(row_to_memory(row, entities)?);
            }
            Ok(memories)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryType;

    fn test_memory(id: &str) -> Memory {
        let now = Utc::now();
        Memory {
            id: id.to_string(),
            namespace: "default".to_string(),
            memory_type: MemoryType::Episodic,
            content: Some("test content".to_string()),
            subject: None,
            predicate: None,
            object: None,
            confidence: 1.0,
            source: None,
            context: None,
            entities: vec![],
            created_at: now,
            updated_at: now,
            t_valid: None,
            t_invalid: None,
            last_accessed: now,
            last_validated: None,
            access_count: 0,
        }
    }

    #[tokio::test]
    #[allow(clippy::float_cmp)]
    async fn test_insert_and_get_memory() {
        let store = SqliteStore::open(":memory:").unwrap();
        let mem = test_memory("test-id-001");
        store.insert_memory(&mem).await.unwrap();

        let fetched = store.get_memory("test-id-001").await.unwrap().unwrap();
        assert_eq!(fetched.id, "test-id-001");
        assert_eq!(fetched.content, Some("test content".to_string()));
        assert_eq!(fetched.memory_type, MemoryType::Episodic);
        assert_eq!(fetched.confidence, 1.0);
    }

    #[tokio::test]
    async fn test_get_memory_not_found() {
        let store = SqliteStore::open(":memory:").unwrap();
        let result = store.get_memory("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_memories_by_ids() {
        let store = SqliteStore::open(":memory:").unwrap();
        store.insert_memory(&test_memory("id-1")).await.unwrap();
        store.insert_memory(&test_memory("id-2")).await.unwrap();
        store.insert_memory(&test_memory("id-3")).await.unwrap();

        let ids = vec!["id-1".to_string(), "id-3".to_string()];
        let results = store.get_memories_by_ids(&ids).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_update_last_accessed() {
        let store = SqliteStore::open(":memory:").unwrap();
        let mem = test_memory("acc-id");
        store.insert_memory(&mem).await.unwrap();

        let before = store.get_memory("acc-id").await.unwrap().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        store
            .update_last_accessed(&["acc-id".to_string()])
            .await
            .unwrap();

        let after = store.get_memory("acc-id").await.unwrap().unwrap();
        assert!(after.last_accessed >= before.last_accessed);
        assert_eq!(after.access_count, before.access_count + 1);
    }

    #[tokio::test]
    async fn test_memory_count_and_recent() {
        let store = SqliteStore::open(":memory:").unwrap();
        store.insert_memory(&test_memory("r-1")).await.unwrap();
        store.insert_memory(&test_memory("r-2")).await.unwrap();

        assert_eq!(store.memory_count().await.unwrap(), 2);

        let recent = store.recent_memories(5).await.unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[tokio::test]
    async fn test_metadata_kv() {
        let store = SqliteStore::open(":memory:").unwrap();
        assert!(store.get_metadata("key1").await.unwrap().is_none());

        store.set_metadata("key1", "val1").await.unwrap();
        assert_eq!(store.get_metadata("key1").await.unwrap().unwrap(), "val1");

        store.set_metadata("key1", "val2").await.unwrap();
        assert_eq!(store.get_metadata("key1").await.unwrap().unwrap(), "val2");
    }

    #[tokio::test]
    async fn test_entity_insert_and_get() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = Utc::now();
        let entity = Entity {
            id: "ent-1".to_string(),
            name: "rust-lang".to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();

        let fetched = store
            .get_entity_by_name("rust-lang")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id, "ent-1");
        assert_eq!(fetched.name, "rust-lang");
    }

    #[tokio::test]
    async fn test_entity_alias_lookup() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = Utc::now();
        let entity = Entity {
            id: "ent-2".to_string(),
            name: "tokio".to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();
        store.add_entity_alias("ent-2", "tokio-rs").await.unwrap();

        let fetched = store.get_entity_by_name("tokio-rs").await.unwrap().unwrap();
        assert_eq!(fetched.id, "ent-2");
        assert!(fetched.aliases.contains(&"tokio-rs".to_string()));
    }

    #[tokio::test]
    async fn test_memory_entity_linking() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = Utc::now();

        let mem = test_memory("mem-link");
        store.insert_memory(&mem).await.unwrap();

        let entity = Entity {
            id: "ent-link".to_string(),
            name: "linked-entity".to_string(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();
        store
            .link_memory_entity("mem-link", "ent-link")
            .await
            .unwrap();

        let fetched = store.get_memory("mem-link").await.unwrap().unwrap();
        assert_eq!(fetched.entities, vec!["linked-entity".to_string()]);
    }
}
