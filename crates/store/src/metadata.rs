use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use deadpool::managed::{Metrics, RecycleResult};
use deadpool_sync::SyncWrapper;
use rusqlite::Connection;

use crate::journal::{PendingOp, PendingOpKind};
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
        namespace: &str,
        limit: i64,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send;

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

    fn list_all_memory_ids(
        &self,
        _namespace: &str,
    ) -> impl Future<Output = Result<Vec<String>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn semantic_rows_missing_normalized_predicate(
        &self,
        _namespace: Option<&str>,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn set_normalized_predicate(
        &self,
        _id: &str,
        _normalized: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }

    fn get_stale_candidates(
        &self,
        _namespace: &str,
        _accessed_before: DateTime<Utc>,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn get_unvalidated_memories(
        &self,
        _namespace: &str,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn get_low_access_memories(
        &self,
        _namespace: &str,
        _max_count: i64,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn update_last_validated(
        &self,
        _ids: &[String],
        _now: DateTime<Utc>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }
    fn memory_count_by_type(
        &self,
        _namespace: &str,
    ) -> impl Future<Output = Result<HashMap<String, u64>, StoreError>> + Send {
        async { Ok(HashMap::new()) }
    }
    fn storage_size_bytes(&self) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }
    fn entity_count(
        &self,
        _namespace: &str,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }

    fn insert_pending_op(
        &self,
        _op: &PendingOp,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }
    fn mark_pending_op_qdrant_written(
        &self,
        _op_id: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }
    fn clear_pending_op(
        &self,
        _op_id: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }
    fn list_pending_ops(&self) -> impl Future<Output = Result<Vec<PendingOp>, StoreError>> + Send {
        async { Ok(vec![]) }
    }
    fn count_pending_ops_older_than(
        &self,
        _age: std::time::Duration,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }

    fn complete_op(
        &self,
        _op: &crate::journal::CompletedOp,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async { Ok(()) }
    }

    fn list_completed_ops(
        &self,
        _status: Option<&str>,
        _limit: usize,
        _since: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<Vec<crate::journal::CompletedOp>, StoreError>> + Send {
        async { Ok(vec![]) }
    }

    fn prune_completed_ops(
        &self,
        _max_count: usize,
        _max_age: chrono::Duration,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send {
        async { Ok(0) }
    }
}

const DEFAULT_READER_POOL_SIZE: usize = 4;

const IN_CLAUSE_BATCH_SIZE: usize = 500;

#[derive(Debug, Clone)]
enum ConnectionSource {
    File(PathBuf),
    SharedMemory { uri: String },
}

impl ConnectionSource {
    fn open_reader(&self) -> Result<Connection, rusqlite::Error> {
        use rusqlite::OpenFlags;
        match self {
            Self::File(path) => Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            ),
            Self::SharedMemory { uri } => Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
            ),
        }
    }

    const fn is_memory(&self) -> bool {
        matches!(self, Self::SharedMemory { .. })
    }
}

struct ReaderManager {
    source: ConnectionSource,
}

impl deadpool::managed::Manager for ReaderManager {
    type Type = SyncWrapper<Connection>;
    type Error = StoreError;

    async fn create(&self) -> Result<Self::Type, StoreError> {
        let source = self.source.clone();
        SyncWrapper::new(deadpool::Runtime::Tokio1, move || {
            let conn = source.open_reader()?;
            apply_reader_pragmas(&conn, source.is_memory())?;
            Ok::<_, StoreError>(conn)
        })
        .await
    }

    async fn recycle(&self, obj: &mut Self::Type, _: &Metrics) -> RecycleResult<StoreError> {
        if obj.is_mutex_poisoned() {
            return Err(deadpool::managed::RecycleError::message(
                "reader connection mutex poisoned",
            ));
        }
        Ok(())
    }
}

const SQLITE_CACHE_SIZE_KB: i32 = 20_000;
const SQLITE_MMAP_SIZE: u64 = 256 * 1024 * 1024;

fn apply_reader_pragmas(conn: &Connection, is_memory: bool) -> Result<(), StoreError> {
    conn.execute_batch(&format!(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         {mmap}
         PRAGMA cache_size = -{SQLITE_CACHE_SIZE_KB};
         PRAGMA query_only = 1;",
        mmap = if is_memory {
            String::new()
        } else {
            format!("PRAGMA mmap_size = {SQLITE_MMAP_SIZE};")
        },
    ))?;
    Ok(())
}

fn apply_writer_pragmas(conn: &Connection, is_memory: bool) -> Result<(), StoreError> {
    conn.execute_batch(&format!(
        "{wal}
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         {mmap}
         PRAGMA cache_size = -{SQLITE_CACHE_SIZE_KB};",
        wal = if is_memory {
            ""
        } else {
            "PRAGMA journal_mode = WAL;"
        },
        mmap = if is_memory {
            String::new()
        } else {
            format!("PRAGMA mmap_size = {SQLITE_MMAP_SIZE};")
        },
    ))?;
    Ok(())
}

type ReaderPool = deadpool::managed::Pool<ReaderManager>;

pub struct SqliteStore {
    writer: Arc<Mutex<Connection>>,
    readers: ReaderPool,
    // Keeps a connection open so SQLite doesn't drop shared in-memory DBs.
    _memory_keepalive: Option<Arc<Mutex<Connection>>>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_pool_size(path, DEFAULT_READER_POOL_SIZE)
    }

    pub fn open_with_pool_size(
        path: impl AsRef<Path>,
        pool_size: usize,
    ) -> Result<Self, StoreError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.to_str().unwrap_or("");
        let is_memory = path_str == ":memory:" || path_str.contains("mode=memory");

        let (writer_conn, source, keepalive) = if is_memory {
            let name = format!("memdb_{}", uuid::Uuid::now_v7().simple());
            let uri = format!("file:{name}?mode=memory&cache=shared");
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI;
            let conn = Connection::open_with_flags(&uri, flags)?;
            let keepalive = Connection::open_with_flags(&uri, flags)?;
            (
                conn,
                ConnectionSource::SharedMemory { uri },
                Some(Arc::new(Mutex::new(keepalive))),
            )
        } else {
            let conn = Connection::open(path_ref)?;
            (conn, ConnectionSource::File(path_ref.to_path_buf()), None)
        };

        apply_writer_pragmas(&writer_conn, is_memory)?;
        migrate(&writer_conn)?;
        let writer = Arc::new(Mutex::new(writer_conn));

        let manager = ReaderManager { source };
        let readers = deadpool::managed::Pool::builder(manager)
            .max_size(pool_size.max(1))
            .build()
            .map_err(|e| StoreError::Pool(format!("reader pool init: {e}")))?;

        Ok(Self {
            writer,
            readers,
            _memory_keepalive: keepalive,
        })
    }

    async fn with_reader<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        let wrapper = self
            .readers
            .get()
            .await
            .map_err(|e| StoreError::Pool(format!("reader pool: {e}")))?;
        wrapper
            .interact(move |conn: &mut Connection| f(conn))
            .await
            .map_err(|e| StoreError::TaskJoin(e.to_string()))?
    }

    async fn with_writer<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        let writer = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            let mut conn = writer.lock().expect("writer lock poisoned");
            f(&mut conn)
        })
        .await
        .map_err(|e| StoreError::TaskJoin(e.to_string()))?
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| {
        tracing::warn!(value = %s, "failed to parse datetime, falling back to epoch");
        DateTime::default()
    })
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
        normalized_predicate: row.get("normalized_predicate")?,
    })
}

fn row_to_entity_without_aliases(row: &rusqlite::Row<'_>) -> Result<Entity, rusqlite::Error> {
    Ok(Entity {
        id: row.get("id")?,
        name: row.get("name")?,
        aliases: vec![],
        entity_type: row.get("entity_type")?,
        created_at: parse_dt(&row.get::<_, String>("created_at")?),
        updated_at: parse_dt(&row.get::<_, String>("updated_at")?),
    })
}

fn get_entity_names_for_memory(
    conn: &Connection,
    memory_id: &str,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.name FROM entities e
         INNER JOIN memory_entities me ON e.id = me.entity_id
         WHERE me.memory_id = ?1",
    )?;
    let names: Vec<String> = stmt
        .query_map(rusqlite::params![memory_id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(names)
}

impl MetadataStore for SqliteStore {
    async fn insert_memory(&self, memory: &Memory) -> Result<(), StoreError> {
        let memory = memory.clone();
        self.with_writer(move |conn| {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO memories (id, namespace, memory_type, content, subject, predicate, object, confidence, source, context, created_at, updated_at, t_valid, t_invalid, last_accessed, last_validated, access_count, normalized_predicate)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            )?;
            stmt.execute(rusqlite::params![
                memory.id,
                memory.namespace,
                memory.memory_type.as_str(),
                memory.content,
                memory.subject,
                memory.predicate,
                memory.object,
                memory.confidence,
                memory.source,
                memory.context.as_ref().map(ToString::to_string),
                memory.created_at.to_rfc3339(),
                memory.updated_at.to_rfc3339(),
                memory.t_valid.map(|d| d.to_rfc3339()),
                memory.t_invalid.map(|d| d.to_rfc3339()),
                memory.last_accessed.to_rfc3339(),
                memory.last_validated.map(|d| d.to_rfc3339()),
                memory.access_count.cast_signed(),
                memory.normalized_predicate,
            ])?;
            Ok(())
        })
        .await
    }

    async fn get_memory(&self, id: &str) -> Result<Option<Memory>, StoreError> {
        let id = id.to_string();
        self.with_reader(move |conn| {
            let entities = get_entity_names_for_memory(conn, &id)?;
            let mut stmt = conn.prepare_cached("SELECT * FROM memories WHERE id = ?1")?;
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
        self.with_reader(move |conn| {
            let mut memories = Vec::new();
            for chunk in ids.chunks(IN_CLAUSE_BATCH_SIZE) {
                let placeholders: Vec<String> =
                    (1..=chunk.len()).map(|i| format!("?{i}")).collect();
                let sql = format!(
                    "SELECT m.*, GROUP_CONCAT(e.name, '\u{1f}') AS entity_names \
                     FROM memories m \
                     LEFT JOIN memory_entities me ON me.memory_id = m.id \
                     LEFT JOIN entities e ON e.id = me.entity_id \
                     WHERE m.id IN ({}) \
                     GROUP BY m.id",
                    placeholders.join(", ")
                );
                let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                    .iter()
                    .map(|s| s as &dyn rusqlite::types::ToSql)
                    .collect();
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(params.as_slice())?;
                while let Some(row) = rows.next()? {
                    let entity_names: Option<String> = row.get("entity_names")?;
                    let entities: Vec<String> = entity_names
                        .map(|s| s.split('\u{1f}').map(String::from).collect())
                        .unwrap_or_default();
                    memories.push(row_to_memory(row, entities)?);
                }
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
        self.with_writer(move |conn| {
            let now = Utc::now().to_rfc3339();
            for chunk in ids.chunks(IN_CLAUSE_BATCH_SIZE) {
                let placeholders: Vec<String> =
                    (1..=chunk.len()).map(|i| format!("?{}", i + 1)).collect();
                let sql = format!(
                    "UPDATE memories SET last_accessed = ?1, access_count = access_count + 1 WHERE id IN ({})",
                    placeholders.join(", ")
                );
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    Vec::with_capacity(chunk.len() + 1);
                params.push(Box::new(now.clone()));
                params.extend(
                    chunk
                        .iter()
                        .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::types::ToSql>),
                );
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(std::convert::AsRef::as_ref).collect();
                let mut stmt = conn.prepare(&sql)?;
                stmt.execute(param_refs.as_slice())?;
            }
            Ok(())
        })
        .await
    }

    async fn insert_entity(&self, entity: &Entity) -> Result<(), StoreError> {
        let entity = entity.clone();
        self.with_writer(move |conn| {
            let aliases_json = serde_json::to_string(&entity.aliases)?;
            let mut stmt = conn.prepare_cached(
                "INSERT INTO entities (id, name, aliases, entity_type, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            stmt.execute(rusqlite::params![
                entity.id,
                entity.name,
                aliases_json,
                entity.entity_type,
                entity.created_at.to_rfc3339(),
                entity.updated_at.to_rfc3339(),
            ])?;
            Ok(())
        })
        .await
    }

    async fn get_entity_by_name(&self, name: &str) -> Result<Option<Entity>, StoreError> {
        let name = name.to_string();
        self.with_reader(move |conn| {
            // Canonical name first, then alias fallback.
            let entity_opt = {
                let mut stmt = conn.prepare_cached(
                    "SELECT e.id, e.name, e.entity_type, e.created_at, e.updated_at \
                     FROM entities e WHERE e.name = ?1 LIMIT 1",
                )?;
                let mut rows = stmt.query(rusqlite::params![name])?;
                if let Some(row) = rows.next()? {
                    Some(row_to_entity_without_aliases(row)?)
                } else {
                    None
                }
            };
            let entity_opt = if let Some(entity) = entity_opt {
                Some(entity)
            } else {
                let mut alias_stmt = conn.prepare_cached(
                    "SELECT e.id, e.name, e.entity_type, e.created_at, e.updated_at \
                     FROM entities e \
                     INNER JOIN entity_aliases a ON a.entity_id = e.id \
                     WHERE a.alias = ?1 LIMIT 1",
                )?;
                let mut alias_rows = alias_stmt.query(rusqlite::params![name])?;
                if let Some(row) = alias_rows.next()? {
                    Some(row_to_entity_without_aliases(row)?)
                } else {
                    None
                }
            };
            let Some(mut entity) = entity_opt else {
                return Ok(None);
            };
            let mut alias_stmt =
                conn.prepare_cached("SELECT alias FROM entity_aliases WHERE entity_id = ?1")?;
            let aliases: Vec<String> = alias_stmt
                .query_map(rusqlite::params![entity.id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            entity.aliases = aliases;
            Ok(Some(entity))
        })
        .await
    }

    async fn get_all_entities(&self) -> Result<Vec<Entity>, StoreError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT e.id, e.name, e.entity_type, e.created_at, e.updated_at, \
                        GROUP_CONCAT(a.alias, '\u{1f}') AS alias_list \
                 FROM entities e \
                 LEFT JOIN entity_aliases a ON a.entity_id = e.id \
                 GROUP BY e.id",
            )?;
            let mut rows = stmt.query([])?;
            let mut entities = Vec::new();
            while let Some(row) = rows.next()? {
                let mut entity = row_to_entity_without_aliases(row)?;
                let alias_list: Option<String> = row.get("alias_list")?;
                entity.aliases = alias_list
                    .map(|s| s.split('\u{1f}').map(String::from).collect())
                    .unwrap_or_default();
                entities.push(entity);
            }
            Ok(entities)
        })
        .await
    }

    async fn add_entity_alias(&self, entity_id: &str, alias: &str) -> Result<(), StoreError> {
        let entity_id = entity_id.to_string();
        let alias = alias.to_string();
        self.with_writer(move |conn| {
            let tx = conn.transaction()?;
            tx.prepare_cached(
                "INSERT OR IGNORE INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
            )?
            .execute(rusqlite::params![entity_id, alias])?;
            tx.prepare_cached("UPDATE entities SET updated_at = ?1 WHERE id = ?2")?
                .execute(rusqlite::params![Utc::now().to_rfc3339(), entity_id])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn link_memory_entity(&self, memory_id: &str, entity_id: &str) -> Result<(), StoreError> {
        let memory_id = memory_id.to_string();
        let entity_id = entity_id.to_string();
        self.with_writer(move |conn| {
            let mut stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id) VALUES (?1, ?2)",
            )?;
            stmt.execute(rusqlite::params![memory_id, entity_id])?;
            Ok(())
        })
        .await
    }

    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StoreError> {
        let key = key.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare_cached("SELECT value FROM metadata WHERE key = ?1")?;
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
        self.with_writer(move |conn| {
            let mut stmt = conn
                .prepare_cached("INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)")?;
            stmt.execute(rusqlite::params![key, value])?;
            Ok(())
        })
        .await
    }

    async fn memory_count(&self) -> Result<u64, StoreError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached("SELECT COUNT(*) FROM memories")?;
            let count: i64 = stmt.query_row([], |row| row.get(0))?;
            Ok(count.cast_unsigned())
        })
        .await
    }

    async fn recent_memories(
        &self,
        namespace: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, StoreError> {
        let ns = namespace.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT m.*, GROUP_CONCAT(e.name, '\u{1f}') AS entity_names \
                 FROM memories m \
                 LEFT JOIN memory_entities me ON me.memory_id = m.id \
                 LEFT JOIN entities e ON e.id = me.entity_id \
                 WHERE m.t_invalid IS NULL AND m.namespace = ?1 \
                 GROUP BY m.id \
                 ORDER BY m.created_at DESC \
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![ns, limit])?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                let entity_names: Option<String> = row.get("entity_names")?;
                let entities: Vec<String> = entity_names
                    .map(|s| s.split('\u{1f}').map(String::from).collect())
                    .unwrap_or_default();
                memories.push(row_to_memory(row, entities)?);
            }
            Ok(memories)
        })
        .await
    }

    async fn delete_memories(&self, ids: &[String]) -> Result<u64, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let ids = ids.to_vec();
        self.with_writer(move |conn| {
            let mut total: u64 = 0;
            for chunk in ids.chunks(IN_CLAUSE_BATCH_SIZE) {
                let placeholders: Vec<String> =
                    (1..=chunk.len()).map(|i| format!("?{i}")).collect();
                let sql = format!(
                    "DELETE FROM memories WHERE id IN ({})",
                    placeholders.join(", ")
                );
                let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                    .iter()
                    .map(|s| s as &dyn rusqlite::types::ToSql)
                    .collect();
                let mut stmt = conn.prepare(&sql)?;
                let n = stmt.execute(params.as_slice())?;
                total += n as u64;
            }
            Ok(total)
        })
        .await
    }

    async fn get_memories_by_subject_predicate(
        &self,
        subject: &str,
        predicate: &str,
    ) -> Result<Vec<Memory>, StoreError> {
        let subject = subject.to_string();
        let predicate = predicate.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT m.*, GROUP_CONCAT(e.name, '\u{1f}') AS entity_names \
                 FROM memories m \
                 LEFT JOIN memory_entities me ON me.memory_id = m.id \
                 LEFT JOIN entities e ON e.id = me.entity_id \
                 WHERE m.subject = ?1 AND m.normalized_predicate = ?2 \
                   AND m.memory_type = 'semantic' \
                   AND m.t_invalid IS NULL \
                 GROUP BY m.id",
            )?;
            let mut rows = stmt.query(rusqlite::params![subject, predicate])?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                let entity_names: Option<String> = row.get("entity_names")?;
                let entities: Vec<String> = entity_names
                    .map(|s| s.split('\u{1f}').map(String::from).collect())
                    .unwrap_or_default();
                memories.push(row_to_memory(row, entities)?);
            }
            Ok(memories)
        })
        .await
    }

    async fn invalidate_memory(
        &self,
        id: &str,
        t_invalid: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let id = id.to_string();
        self.with_writer(move |conn| {
            let mut stmt = conn.prepare_cached(
                "UPDATE memories SET t_invalid = ?1, updated_at = ?1 WHERE id = ?2",
            )?;
            stmt.execute(rusqlite::params![t_invalid.to_rfc3339(), id])?;
            Ok(())
        })
        .await
    }

    async fn insert_pending_op(&self, op: &PendingOp) -> Result<(), StoreError> {
        let op = op.clone();
        self.with_writer(move |conn| {
            conn.prepare_cached(
                "INSERT INTO pending_ops (op_id, kind, memory_id, namespace, target_id, qdrant_written, started_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(rusqlite::params![
                op.op_id,
                op.kind.as_str(),
                op.memory_id,
                op.namespace,
                op.target_id,
                i64::from(op.qdrant_written),
                op.started_at.to_rfc3339(),
            ])?;
            Ok(())
        })
        .await
    }

    async fn mark_pending_op_qdrant_written(&self, op_id: &str) -> Result<(), StoreError> {
        let op_id = op_id.to_string();
        self.with_writer(move |conn| {
            conn.prepare_cached("UPDATE pending_ops SET qdrant_written = 1 WHERE op_id = ?1")?
                .execute(rusqlite::params![op_id])?;
            Ok(())
        })
        .await
    }

    async fn clear_pending_op(&self, op_id: &str) -> Result<(), StoreError> {
        let op_id = op_id.to_string();
        self.with_writer(move |conn| {
            conn.prepare_cached("DELETE FROM pending_ops WHERE op_id = ?1")?
                .execute(rusqlite::params![op_id])?;
            Ok(())
        })
        .await
    }

    async fn list_pending_ops(&self) -> Result<Vec<PendingOp>, StoreError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT op_id, kind, memory_id, namespace, target_id, qdrant_written, started_at \
                 FROM pending_ops ORDER BY started_at",
            )?;
            let mut rows = stmt.query([])?;
            let mut ops = Vec::new();
            while let Some(row) = rows.next()? {
                let kind_str: String = row.get("kind")?;
                let Some(kind) = PendingOpKind::parse(&kind_str) else {
                    tracing::warn!(kind = %kind_str, "unknown pending op kind, skipping");
                    continue;
                };
                ops.push(PendingOp {
                    op_id: row.get("op_id")?,
                    kind,
                    memory_id: row.get("memory_id")?,
                    namespace: row.get("namespace")?,
                    target_id: row.get("target_id")?,
                    qdrant_written: row.get::<_, i64>("qdrant_written")? != 0,
                    started_at: parse_dt(&row.get::<_, String>("started_at")?),
                });
            }
            Ok(ops)
        })
        .await
    }

    async fn count_pending_ops_older_than(
        &self,
        age: std::time::Duration,
    ) -> Result<u64, StoreError> {
        let cutoff = (Utc::now()
            - chrono::Duration::from_std(age).unwrap_or_else(|_| chrono::Duration::zero()))
        .to_rfc3339();
        self.with_reader(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pending_ops WHERE started_at < ?1",
                rusqlite::params![cutoff],
                |row| row.get(0),
            )?;
            Ok(count.cast_unsigned())
        })
        .await
    }

    async fn set_normalized_predicate(&self, id: &str, normalized: &str) -> Result<(), StoreError> {
        let id = id.to_string();
        let normalized = normalized.to_string();
        self.with_writer(move |conn| {
            conn.prepare_cached(
                "UPDATE memories SET normalized_predicate = ?1, updated_at = ?2 WHERE id = ?3",
            )?
            .execute(rusqlite::params![normalized, Utc::now().to_rfc3339(), id])?;
            Ok(())
        })
        .await
    }

    async fn semantic_rows_missing_normalized_predicate(
        &self,
        namespace: Option<&str>,
    ) -> Result<Vec<Memory>, StoreError> {
        let ns = namespace.map(ToString::to_string);
        self.with_reader(move |conn| {
            let sql = match ns {
                Some(ref n) => {
                    let mut stmt = conn.prepare_cached(
                        "SELECT * FROM memories \
                         WHERE memory_type = 'semantic' AND normalized_predicate IS NULL \
                         AND namespace = ?1",
                    )?;
                    let mut rows = stmt.query(rusqlite::params![n])?;
                    let mut out = Vec::new();
                    while let Some(row) = rows.next()? {
                        out.push(row_to_memory(row, vec![])?);
                    }
                    return Ok(out);
                }
                None => {
                    "SELECT * FROM memories \
                     WHERE memory_type = 'semantic' AND normalized_predicate IS NULL"
                }
            };
            let mut stmt = conn.prepare_cached(sql)?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row_to_memory(row, vec![])?);
            }
            Ok(out)
        })
        .await
    }

    async fn list_all_memory_ids(&self, namespace: &str) -> Result<Vec<String>, StoreError> {
        let namespace = namespace.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id FROM memories WHERE namespace = ?1 AND t_invalid IS NULL",
            )?;
            let ids: Vec<String> = stmt
                .query_map(rusqlite::params![namespace], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(ids)
        })
        .await
    }

    async fn get_stale_candidates(
        &self,
        namespace: &str,
        accessed_before: DateTime<Utc>,
    ) -> Result<Vec<Memory>, StoreError> {
        let ns = namespace.to_string();
        let before = accessed_before.to_rfc3339();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM memories WHERE t_invalid IS NULL AND namespace = ?1 \
                 AND last_accessed < ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![ns, before])?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                memories.push(row_to_memory(row, vec![])?);
            }
            Ok(memories)
        })
        .await
    }

    async fn get_unvalidated_memories(&self, namespace: &str) -> Result<Vec<Memory>, StoreError> {
        let ns = namespace.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM memories WHERE last_validated IS NULL \
                 AND t_invalid IS NULL AND namespace = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![ns])?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                memories.push(row_to_memory(row, vec![])?);
            }
            Ok(memories)
        })
        .await
    }

    async fn get_low_access_memories(
        &self,
        namespace: &str,
        max_count: i64,
    ) -> Result<Vec<Memory>, StoreError> {
        let ns = namespace.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM memories WHERE access_count <= ?1 \
                 AND t_invalid IS NULL AND namespace = ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![max_count, ns])?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next()? {
                memories.push(row_to_memory(row, vec![])?);
            }
            Ok(memories)
        })
        .await
    }

    async fn update_last_validated(
        &self,
        ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        let ts = now.to_rfc3339();
        self.with_writer(move |conn| {
            let tx = conn.transaction()?;
            for chunk in ids.chunks(IN_CLAUSE_BATCH_SIZE) {
                let placeholders: Vec<String> =
                    (1..=chunk.len()).map(|i| format!("?{}", i + 1)).collect();
                let sql = format!(
                    "UPDATE memories SET last_validated = ?1 WHERE id IN ({})",
                    placeholders.join(", ")
                );
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    Vec::with_capacity(chunk.len() + 1);
                params.push(Box::new(ts.clone()));
                params.extend(
                    chunk
                        .iter()
                        .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::types::ToSql>),
                );
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(std::convert::AsRef::as_ref).collect();
                tx.execute(&sql, param_refs.as_slice())?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn memory_count_by_type(
        &self,
        namespace: &str,
    ) -> Result<HashMap<String, u64>, StoreError> {
        let ns = namespace.to_string();
        self.with_reader(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT memory_type, COUNT(*) FROM memories \
                 WHERE namespace = ?1 AND t_invalid IS NULL GROUP BY memory_type",
            )?;
            let mut map = HashMap::new();
            let mut rows = stmt.query(rusqlite::params![ns])?;
            while let Some(row) = rows.next()? {
                let mt: String = row.get(0)?;
                let count: u64 = row.get::<_, i64>(1)?.cast_unsigned();
                map.insert(mt, count);
            }
            Ok(map)
        })
        .await
    }

    async fn storage_size_bytes(&self) -> Result<u64, StoreError> {
        self.with_reader(move |conn| {
            let page_count: u64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))?
                .cast_unsigned();
            let page_size: u64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))?
                .cast_unsigned();
            Ok(page_count * page_size)
        })
        .await
    }

    async fn entity_count(&self, namespace: &str) -> Result<u64, StoreError> {
        let ns = namespace.to_string();
        self.with_reader(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT e.id) FROM entities e \
                 JOIN memory_entities me ON e.id = me.entity_id \
                 JOIN memories m ON m.id = me.memory_id \
                 WHERE m.namespace = ?1",
                rusqlite::params![ns],
                |r| r.get(0),
            )?;
            Ok(count.cast_unsigned())
        })
        .await
    }

    async fn complete_op(&self, op: &crate::journal::CompletedOp) -> Result<(), StoreError> {
        let op = op.clone();
        self.with_writer(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO completed_ops \
                 (op_id, kind, memory_id, namespace, started_at, completed_at, duration_ms, outcome) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    op.op_id,
                    op.kind.as_str(),
                    op.memory_id,
                    op.namespace,
                    op.started_at.to_rfc3339(),
                    op.completed_at.to_rfc3339(),
                    op.duration_ms,
                    op.outcome,
                ],
            )?;
            conn.execute("DELETE FROM pending_ops WHERE op_id = ?1", [&op.op_id])?;
            Ok(())
        })
        .await
    }

    async fn list_completed_ops(
        &self,
        status: Option<&str>,
        limit: usize,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<crate::journal::CompletedOp>, StoreError> {
        let status = status.map(String::from);
        let since = since.map(|s| s.to_rfc3339());
        self.with_reader(move |conn| {
            let mut sql = String::from(
                "SELECT op_id, kind, memory_id, namespace, started_at, completed_at, duration_ms, outcome \
                 FROM completed_ops WHERE 1=1",
            );
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];

            if let Some(ref s) = status {
                if s == "failed" {
                    sql.push_str(" AND outcome != 'ok'");
                } else if s == "completed" {
                    sql.push_str(" AND outcome = 'ok'");
                }
            }
            if let Some(ref since_str) = since {
                sql.push_str(" AND completed_at >= ?");
                params.push(Box::new(since_str.clone()));
            }
            sql.push_str(" ORDER BY completed_at DESC LIMIT ?");
            #[allow(clippy::cast_possible_wrap)]
            params.push(Box::new(limit as i64));

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(AsRef::as_ref).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok(crate::journal::CompletedOp {
                        op_id: row.get(0)?,
                        kind: crate::journal::PendingOpKind::parse(
                            &row.get::<_, String>(1)?,
                        )
                        .unwrap_or(crate::journal::PendingOpKind::Store),
                        memory_id: row.get(2)?,
                        namespace: row.get(3)?,
                        started_at: row.get::<_, String>(4)?.parse().unwrap_or_default(),
                        completed_at: row.get::<_, String>(5)?.parse().unwrap_or_default(),
                        duration_ms: row.get(6)?,
                        outcome: row.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    async fn prune_completed_ops(
        &self,
        max_count: usize,
        max_age: chrono::Duration,
    ) -> Result<u64, StoreError> {
        let cutoff = (Utc::now() - max_age).to_rfc3339();
        self.with_writer(move |conn| {
            let before: i64 =
                conn.query_row("SELECT COUNT(*) FROM completed_ops", [], |r| r.get(0))?;
            conn.execute(
                "DELETE FROM completed_ops WHERE completed_at < ?1",
                [&cutoff],
            )?;
            conn.execute(
                "DELETE FROM completed_ops WHERE op_id NOT IN \
                 (SELECT op_id FROM completed_ops ORDER BY completed_at DESC LIMIT ?1)",
                #[allow(clippy::cast_possible_wrap)]
                [max_count as i64],
            )?;
            let after: i64 =
                conn.query_row("SELECT COUNT(*) FROM completed_ops", [], |r| r.get(0))?;
            #[allow(clippy::cast_sign_loss)]
            Ok((before - after).max(0) as u64)
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
            normalized_predicate: None,
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

        let recent = store.recent_memories("default", 5).await.unwrap();
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
    async fn test_reader_pool_serves_concurrent_requests() {
        let store = std::sync::Arc::new(SqliteStore::open(":memory:").unwrap());
        store.insert_memory(&test_memory("c-1")).await.unwrap();
        store.insert_memory(&test_memory("c-2")).await.unwrap();
        store.insert_memory(&test_memory("c-3")).await.unwrap();

        let mut handles = vec![];
        for _ in 0..8 {
            let s = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(
                async move { s.get_memory("c-1").await.unwrap() },
            ));
        }
        for h in handles {
            assert!(h.await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn test_prepare_cached_does_not_break_repeated_inserts() {
        let store = SqliteStore::open(":memory:").unwrap();
        for i in 0..50 {
            store
                .insert_memory(&test_memory(&format!("pc-{i}")))
                .await
                .unwrap();
        }
        assert_eq!(store.memory_count().await.unwrap(), 50);
    }

    #[tokio::test]
    async fn test_get_memories_by_ids_hydrates_entities_in_single_query() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = Utc::now();
        let entity = Entity {
            id: "ent-gc".into(),
            name: "rust".into(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();
        for i in 1..=3 {
            let m = test_memory(&format!("gc-{i}"));
            store.insert_memory(&m).await.unwrap();
            store.link_memory_entity(&m.id, "ent-gc").await.unwrap();
        }
        let ids: Vec<String> = (1..=3).map(|i| format!("gc-{i}")).collect();
        let memories = store.get_memories_by_ids(&ids).await.unwrap();
        assert_eq!(memories.len(), 3);
        for m in &memories {
            assert_eq!(m.entities, vec!["rust".to_string()]);
        }
    }

    #[tokio::test]
    async fn test_alias_lookup_via_index_table() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = Utc::now();
        let entity = Entity {
            id: "idx-1".into(),
            name: "postgres".into(),
            aliases: vec![],
            entity_type: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_entity(&entity).await.unwrap();
        store.add_entity_alias("idx-1", "pg").await.unwrap();
        store.add_entity_alias("idx-1", "postgresql").await.unwrap();

        let by_alias = store.get_entity_by_name("pg").await.unwrap().unwrap();
        assert_eq!(by_alias.id, "idx-1");
        let by_alias2 = store
            .get_entity_by_name("postgresql")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_alias2.id, "idx-1");
        let by_name = store.get_entity_by_name("postgres").await.unwrap().unwrap();
        assert_eq!(by_name.id, "idx-1");
        assert_eq!(by_name.aliases.len(), 2);
        assert!(by_name.aliases.contains(&"pg".to_string()));
        assert!(by_name.aliases.contains(&"postgresql".to_string()));
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

    #[tokio::test]
    async fn test_delete_memories_removes_rows() {
        let store = SqliteStore::open(":memory:").unwrap();
        store.insert_memory(&test_memory("d-1")).await.unwrap();
        store.insert_memory(&test_memory("d-2")).await.unwrap();
        let deleted = store
            .delete_memories(&["d-1".into(), "d-nope".into()])
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert!(store.get_memory("d-1").await.unwrap().is_none());
        assert!(store.get_memory("d-2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_get_memories_by_subject_predicate_filters_active_only() {
        let store = SqliteStore::open(":memory:").unwrap();
        let mut m1 = test_memory("sp-1");
        m1.memory_type = MemoryType::Semantic;
        m1.subject = Some("api".into());
        m1.predicate = Some("uses".into());
        m1.object = Some("tokio".into());
        m1.normalized_predicate = Some("depends_on".into());
        store.insert_memory(&m1).await.unwrap();

        let mut m2 = test_memory("sp-2");
        m2.memory_type = MemoryType::Semantic;
        m2.subject = Some("api".into());
        m2.predicate = Some("uses".into());
        m2.object = Some("async-std".into());
        m2.normalized_predicate = Some("depends_on".into());
        m2.t_invalid = Some(Utc::now());
        store.insert_memory(&m2).await.unwrap();

        let hits = store
            .get_memories_by_subject_predicate("api", "depends_on")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sp-1");
    }

    #[tokio::test]
    async fn test_invalidate_memory_sets_t_invalid() {
        let store = SqliteStore::open(":memory:").unwrap();
        store.insert_memory(&test_memory("inv-1")).await.unwrap();
        let t = Utc::now();
        store.invalidate_memory("inv-1", t).await.unwrap();
        let m = store.get_memory("inv-1").await.unwrap().unwrap();
        assert!(m.t_invalid.is_some());
    }

    #[tokio::test]
    async fn test_pending_op_lifecycle() {
        let store = SqliteStore::open(":memory:").unwrap();
        let op = PendingOp {
            op_id: "op-1".into(),
            kind: PendingOpKind::Store,
            memory_id: "mem-1".into(),
            namespace: "default".into(),
            target_id: None,
            qdrant_written: false,
            started_at: Utc::now(),
        };
        store.insert_pending_op(&op).await.unwrap();

        let listed = store.list_pending_ops().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].op_id, "op-1");
        assert_eq!(listed[0].namespace, "default");
        assert!(!listed[0].qdrant_written);

        store.mark_pending_op_qdrant_written("op-1").await.unwrap();
        let listed = store.list_pending_ops().await.unwrap();
        assert!(listed[0].qdrant_written);

        store.clear_pending_op("op-1").await.unwrap();
        assert!(store.list_pending_ops().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_count_pending_ops_older_than() {
        let store = SqliteStore::open(":memory:").unwrap();
        let old = PendingOp {
            op_id: "op-old".into(),
            kind: PendingOpKind::Forget,
            memory_id: "mem-1".into(),
            namespace: "default".into(),
            target_id: None,
            qdrant_written: false,
            started_at: Utc::now() - chrono::Duration::hours(2),
        };
        let fresh = PendingOp {
            op_id: "op-fresh".into(),
            kind: PendingOpKind::Store,
            memory_id: "mem-2".into(),
            namespace: "default".into(),
            target_id: None,
            qdrant_written: false,
            started_at: Utc::now(),
        };
        store.insert_pending_op(&old).await.unwrap();
        store.insert_pending_op(&fresh).await.unwrap();

        let count = store
            .count_pending_ops_older_than(std::time::Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    fn make_test_memory(id: &str, memory_type: MemoryType) -> Memory {
        Memory {
            id: id.into(),
            namespace: "test".into(),
            memory_type,
            content: Some("test content".into()),
            subject: if memory_type == MemoryType::Semantic {
                Some("subj".into())
            } else {
                None
            },
            predicate: if memory_type == MemoryType::Semantic {
                Some("pred".into())
            } else {
                None
            },
            object: if memory_type == MemoryType::Semantic {
                Some("obj".into())
            } else {
                None
            },
            confidence: 1.0,
            source: None,
            context: None,
            entities: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            t_valid: None,
            t_invalid: None,
            last_accessed: Utc::now(),
            last_validated: None,
            access_count: 0,
            normalized_predicate: None,
        }
    }

    #[tokio::test]
    async fn test_get_stale_candidates() {
        let store = SqliteStore::open(":memory:").unwrap();
        let now = Utc::now();
        let mut old = make_test_memory("old", MemoryType::Episodic);
        old.last_accessed = now - chrono::Duration::days(60);
        old.created_at = now - chrono::Duration::days(60);
        old.updated_at = now - chrono::Duration::days(60);
        store.insert_memory(&old).await.unwrap();

        let mut fresh = make_test_memory("fresh", MemoryType::Episodic);
        fresh.last_validated = Some(now);
        store.insert_memory(&fresh).await.unwrap();

        let cutoff = now - chrono::Duration::days(30);
        let stale = store.get_stale_candidates("test", cutoff).await.unwrap();
        assert!(
            stale.iter().any(|m| m.id == "old"),
            "old memory should be in stale candidates"
        );
        assert!(
            !stale.iter().any(|m| m.id == "fresh"),
            "fresh memory should not be in stale candidates"
        );
    }

    #[tokio::test]
    async fn test_get_unvalidated_memories() {
        let store = SqliteStore::open(":memory:").unwrap();
        let mem = make_test_memory("unval", MemoryType::Semantic);
        store.insert_memory(&mem).await.unwrap();

        let unvalidated = store.get_unvalidated_memories("test").await.unwrap();
        assert_eq!(unvalidated.len(), 1);
        assert_eq!(unvalidated[0].id, "unval");
    }

    #[tokio::test]
    async fn test_update_last_validated() {
        let store = SqliteStore::open(":memory:").unwrap();
        let mem = make_test_memory("val-test", MemoryType::Episodic);
        store.insert_memory(&mem).await.unwrap();

        let now = Utc::now();
        store
            .update_last_validated(&["val-test".into()], now)
            .await
            .unwrap();

        let fetched = store.get_memory("val-test").await.unwrap().unwrap();
        assert!(fetched.last_validated.is_some());
    }

    #[tokio::test]
    async fn test_memory_count_by_type() {
        let store = SqliteStore::open(":memory:").unwrap();
        store
            .insert_memory(&make_test_memory("ep1", MemoryType::Episodic))
            .await
            .unwrap();
        store
            .insert_memory(&make_test_memory("ep2", MemoryType::Episodic))
            .await
            .unwrap();
        store
            .insert_memory(&make_test_memory("sem1", MemoryType::Semantic))
            .await
            .unwrap();

        let counts = store.memory_count_by_type("test").await.unwrap();
        assert_eq!(counts.get("episodic"), Some(&2));
        assert_eq!(counts.get("semantic"), Some(&1));
    }

    #[tokio::test]
    async fn test_entity_count() {
        let store = SqliteStore::open(":memory:").unwrap();
        let mem = make_test_memory("ent-test", MemoryType::Episodic);
        store.insert_memory(&mem).await.unwrap();
        let entity = Entity {
            id: "e1".into(),
            name: "api-server".into(),
            aliases: vec![],
            entity_type: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.insert_entity(&entity).await.unwrap();
        store.link_memory_entity("ent-test", "e1").await.unwrap();

        let count = store.entity_count("test").await.unwrap();
        assert_eq!(count, 1);
    }
}
