use rusqlite::Connection;

use crate::StoreError;

pub fn migrate(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 268435456;
         PRAGMA cache_size = -20000;",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memories (
            id TEXT PRIMARY KEY,
            namespace TEXT NOT NULL,
            memory_type TEXT NOT NULL,
            content TEXT,
            subject TEXT,
            predicate TEXT,
            object TEXT,
            confidence REAL NOT NULL DEFAULT 1.0,
            source TEXT,
            context TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            t_valid TEXT,
            t_invalid TEXT,
            last_accessed TEXT NOT NULL,
            last_validated TEXT,
            access_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
        CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memory_type);
        CREATE INDEX IF NOT EXISTS idx_memories_subject ON memories(subject);
        CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);

        CREATE TABLE IF NOT EXISTS entities (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            aliases TEXT NOT NULL DEFAULT '[]',
            entity_type TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS memory_entities (
            memory_id TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            PRIMARY KEY (memory_id, entity_id),
            FOREIGN KEY (memory_id) REFERENCES memories(id) ON DELETE CASCADE,
            FOREIGN KEY (entity_id) REFERENCES entities(id) ON DELETE CASCADE
        );
        -- PK leads with memory_id; entity_id lookups need this index.
        CREATE INDEX IF NOT EXISTS idx_memory_entities_entity ON memory_entities(entity_id);

        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS pending_ops (
            op_id          TEXT PRIMARY KEY,
            kind           TEXT NOT NULL,
            memory_id      TEXT NOT NULL,
            namespace      TEXT NOT NULL,
            target_id      TEXT,
            qdrant_written INTEGER NOT NULL DEFAULT 0,
            started_at     TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pending_ops_started ON pending_ops(started_at);

        CREATE TABLE IF NOT EXISTS entity_aliases (
            entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            alias     TEXT NOT NULL,
            PRIMARY KEY (entity_id, alias)
        );
        CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias ON entity_aliases(alias);",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS completed_ops (
            op_id          TEXT PRIMARY KEY,
            kind           TEXT NOT NULL,
            memory_id      TEXT NOT NULL,
            namespace      TEXT NOT NULL,
            started_at     TEXT NOT NULL,
            completed_at   TEXT NOT NULL,
            duration_ms    INTEGER NOT NULL,
            outcome        TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_completed_ops_completed ON completed_ops(completed_at);",
    )?;

    let has_col: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('memories') WHERE name='normalized_predicate'")?
        .query([])?
        .next()?
        .is_some();
    if !has_col {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN normalized_predicate TEXT")?;
    }

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_conflict \
         ON memories(namespace, memory_type, subject, normalized_predicate) \
         WHERE t_invalid IS NULL",
    )?;

    // Backfill entity_aliases from the legacy JSON column.
    let mut stmt = conn.prepare(
        "SELECT id, aliases FROM entities \
         WHERE NOT EXISTS (SELECT 1 FROM entity_aliases WHERE entity_id = entities.id) \
         AND aliases IS NOT NULL AND aliases != '[]'",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for (entity_id, aliases_json) in rows {
        let aliases: Vec<String> = match serde_json::from_str(&aliases_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    entity_id = %entity_id,
                    raw = %aliases_json,
                    error = %e,
                    "skipping entity_aliases backfill: malformed legacy JSON"
                );
                continue;
            }
        };
        for alias in aliases {
            conn.execute(
                "INSERT OR IGNORE INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
                rusqlite::params![entity_id, alias],
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_migrate_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert!(tables.contains(&"memories".to_string()));
        assert!(tables.contains(&"entities".to_string()));
        assert!(tables.contains(&"memory_entities".to_string()));
        assert!(tables.contains(&"metadata".to_string()));
    }

    #[test]
    fn test_migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn test_migrate_sets_synchronous_normal() {
        // WAL is a no-op on :memory:, so we only check synchronous here.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "PRAGMA synchronous should be NORMAL (1)");
    }

    #[test]
    fn test_pending_ops_namespace_is_not_null() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let res = conn.execute(
            "INSERT INTO pending_ops (op_id, kind, memory_id, namespace, qdrant_written, started_at) \
             VALUES ('op1', 'store', 'mem1', NULL, 0, '2026-01-01T00:00:00Z')",
            [],
        );
        assert!(res.is_err(), "inserting NULL namespace must fail");
    }

    #[test]
    fn test_migrate_creates_pending_ops_table() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pending_ops'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_pending_ops_has_namespace_column() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let has_col: bool = conn
            .prepare("SELECT name FROM pragma_table_info('pending_ops') WHERE name='namespace'")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count()
            == 1;
        assert!(has_col, "pending_ops must have a namespace column");
    }

    #[test]
    fn test_migrate_creates_entity_aliases_table_and_index() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entity_aliases'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_entity_aliases_alias'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_migrate_adds_normalized_predicate_column() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let has_col: bool = conn
            .prepare(
                "SELECT name FROM pragma_table_info('memories') WHERE name='normalized_predicate'",
            )
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count()
            == 1;
        assert!(has_col);
    }

    #[test]
    fn test_migrate_creates_completed_ops_table() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='completed_ops'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_alias_backfill_populates_table() {
        use chrono::Utc;
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO entities (id, name, aliases, entity_type, created_at, updated_at) \
             VALUES ('e1', 'tokio', '[\"tokio-rs\",\"async-rt\"]', NULL, ?1, ?1)",
            rusqlite::params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute("DELETE FROM entity_aliases", []).unwrap();
        migrate(&conn).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entity_aliases WHERE entity_id='e1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
    }
}
