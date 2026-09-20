use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult, Transaction};
use std::path::Path;

pub const STORAGE_FORMAT_VERSION: u32 = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct UsageRecord {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ContextRecord {
    pub budget: u64,
    pub estimated_tokens: u64,
    pub fingerprint: String,
    pub selected_count: usize,
    pub dropped_count: usize,
    pub truncated_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EventRecord {
    pub sequence: i64,
    pub kind: String,
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AuditRecord {
    pub id: i64,
    pub session_id: i64,
    pub turn_id: i64,
    pub kind: String,
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: i64,
    pub objective: String,
    pub workspace: String,
    pub archived: bool,
    pub deleted_at: Option<i64>,
}

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open_paths(paths: &agent_paths::AppPaths) -> SqlResult<Self> {
        if let Some(parent) = paths.db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        Self::open(&paths.db_path)
    }

    pub fn open(path: impl AsRef<Path>) -> SqlResult<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let database = Self { connection };
        database.migrate()?;
        Ok(database)
    }

    pub fn in_memory() -> SqlResult<Self> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let database = Self { connection };
        database.migrate()?;
        Ok(database)
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn user_version(&self) -> SqlResult<u32> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
    }

    fn migrate(&self) -> SqlResult<()> {
        let version = self.user_version()?;
        if version < 1 {
            self.connection.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS sessions (id INTEGER PRIMARY KEY, objective TEXT NOT NULL, workspace TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS turns (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id), turn_index INTEGER NOT NULL, created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'running', error TEXT);
                 CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id), turn_id INTEGER REFERENCES turns(id), role TEXT NOT NULL, content TEXT NOT NULL, created_at INTEGER NOT NULL);
                 CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content, content='messages', content_rowid='id');
                 CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content); END;
                 CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content); END;
                 CREATE TABLE IF NOT EXISTS memory (id INTEGER PRIMARY KEY, scope TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, content TEXT NOT NULL, confidence REAL NOT NULL, importance INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, last_accessed_at INTEGER NOT NULL, access_count INTEGER NOT NULL DEFAULT 0, expires_at INTEGER, source_session_id INTEGER REFERENCES sessions(id), status TEXT NOT NULL DEFAULT 'active', superseded_by INTEGER REFERENCES memory(id));
                 CREATE INDEX IF NOT EXISTS memory_scope_key ON memory(scope, kind, key);
                 CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(key, content, content='memory', content_rowid='id');
                 CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN INSERT INTO memory_fts(rowid, key, content) VALUES (new.id, new.key, new.content); END;
                 CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN INSERT INTO memory_fts(memory_fts, rowid, key, content) VALUES ('delete', old.id, old.key, old.content); END;
                 CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory BEGIN INSERT INTO memory_fts(memory_fts, rowid, key, content) VALUES ('delete', old.id, old.key, old.content); INSERT INTO memory_fts(rowid, key, content) VALUES (new.id, new.key, new.content); END;
                 PRAGMA user_version = 3;
                 COMMIT;"
            )?;
        }
        if version == 1 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN workspace TEXT NOT NULL DEFAULT ''; ALTER TABLE turns ADD COLUMN status TEXT NOT NULL DEFAULT 'complete'; ALTER TABLE turns ADD COLUMN error TEXT; PRAGMA user_version = 3;",
            )?;
        }
        if version == 2 {
            self.connection.execute_batch(
                "ALTER TABLE turns ADD COLUMN status TEXT NOT NULL DEFAULT 'complete'; ALTER TABLE turns ADD COLUMN error TEXT; PRAGMA user_version = 3;",
            )?;
        }
        self.connection.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(key, content, content='memory', content_rowid='id');
             CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN INSERT INTO memory_fts(rowid, key, content) VALUES (new.id, new.key, new.content); END;
             CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN INSERT INTO memory_fts(memory_fts, rowid, key, content) VALUES ('delete', old.id, old.key, old.content); END;
             CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory BEGIN INSERT INTO memory_fts(memory_fts, rowid, key, content) VALUES ('delete', old.id, old.key, old.content); INSERT INTO memory_fts(rowid, key, content) VALUES (new.id, new.key, new.content); END;"
        )?;
        if version < 4 {
            self.connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS turn_usage (turn_id INTEGER PRIMARY KEY REFERENCES turns(id), input_tokens TEXT NOT NULL, output_tokens TEXT NOT NULL, cache_read_tokens TEXT NOT NULL, cache_write_tokens TEXT NOT NULL, reasoning_tokens TEXT NOT NULL, total_tokens TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS context_plans (turn_id INTEGER PRIMARY KEY REFERENCES turns(id), budget INTEGER NOT NULL, estimated_tokens INTEGER NOT NULL, fingerprint TEXT NOT NULL, selected_count INTEGER NOT NULL, dropped_count INTEGER NOT NULL, truncated_count INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS lifecycle_events (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id), turn_id INTEGER NOT NULL REFERENCES turns(id), sequence INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(turn_id, sequence));
                 PRAGMA user_version = 4;"
            )?;
        }
        if version < 5 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN deleted_at INTEGER;
                 PRAGMA user_version = 5;",
            )?;
        }
        Ok(())
    }

    pub fn create_session(&self, objective: &str) -> SqlResult<i64> {
        self.connection.execute(
            "INSERT INTO sessions(objective, created_at) VALUES (?1, strftime('%s','now'))",
            params![objective],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn create_session_for_workspace(&self, objective: &str, workspace: &str) -> SqlResult<i64> {
        self.connection.execute(
            "INSERT INTO sessions(objective, workspace, created_at) VALUES (?1, ?2, strftime('%s','now'))",
            params![objective, workspace],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn session(&self, id: i64) -> SqlResult<Option<SessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, objective, workspace, archived, deleted_at FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        objective: row.get(1)?,
                        workspace: row.get(2)?,
                        archived: row.get::<_, i64>(3)? != 0,
                        deleted_at: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    pub fn latest_session(&self, workspace: &str) -> SqlResult<Option<SessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, objective, workspace, archived, deleted_at FROM sessions WHERE workspace = ?1 AND archived = 0 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1",
                params![workspace],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        objective: row.get(1)?,
                        workspace: row.get(2)?,
                        archived: row.get::<_, i64>(3)? != 0,
                        deleted_at: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    pub fn latest_session_any(&self) -> SqlResult<Option<SessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, objective, workspace, archived, deleted_at FROM sessions WHERE archived = 0 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        objective: row.get(1)?,
                        workspace: row.get(2)?,
                        archived: row.get::<_, i64>(3)? != 0,
                        deleted_at: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    pub fn list_sessions(&self, workspace: &str) -> SqlResult<Vec<SessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, objective, workspace, archived, deleted_at FROM sessions WHERE workspace = ?1 AND archived = 0 AND deleted_at IS NULL ORDER BY id DESC",
        )?;
        let rows = statement.query_map(params![workspace], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                objective: row.get(1)?,
                workspace: row.get(2)?,
                archived: row.get::<_, i64>(3)? != 0,
                deleted_at: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn archive_session(&self, id: i64, workspace: &str, archived: bool) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET archived = ?1 WHERE id = ?2 AND workspace = ?3 AND deleted_at IS NULL",
            params![archived as i64, id, workspace],
        )? == 1)
    }

    pub fn delete_session(&self, id: i64, workspace: &str) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET deleted_at = strftime('%s','now') WHERE id = ?1 AND workspace = ?2 AND deleted_at IS NULL",
            params![id, workspace],
        )? == 1)
    }

    pub fn session_messages(&self, session_id: i64) -> SqlResult<Vec<(String, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id")?;
        let rows =
            statement.query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    pub fn session_turn_count(&self, session_id: i64) -> SqlResult<i64> {
        self.connection.query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
    }

    pub fn append_turn(&self, session_id: i64, turn_index: i64) -> SqlResult<i64> {
        self.connection.execute("INSERT INTO turns(session_id, turn_index, created_at) VALUES (?1, ?2, strftime('%s','now'))", params![session_id, turn_index])?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn update_turn_status(
        &self,
        turn_id: i64,
        status: &str,
        error: Option<&str>,
    ) -> SqlResult<()> {
        self.connection.execute(
            "UPDATE turns SET status = ?1, error = ?2 WHERE id = ?3",
            params![status, error, turn_id],
        )?;
        Ok(())
    }

    pub fn turn_status(&self, turn_id: i64) -> SqlResult<Option<(String, Option<String>)>> {
        self.connection
            .query_row(
                "SELECT status, error FROM turns WHERE id = ?1",
                params![turn_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub fn append_message(
        &self,
        session_id: i64,
        turn_id: i64,
        role: &str,
        content: &str,
    ) -> SqlResult<i64> {
        self.connection.execute("INSERT INTO messages(session_id, turn_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))", params![session_id, turn_id, role, content])?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn search_messages(
        &self,
        query: &str,
        limit: usize,
    ) -> SqlResult<Vec<(i64, String, String)>> {
        let mut statement = self.connection.prepare("SELECT m.id, m.role, m.content FROM messages_fts f JOIN messages m ON m.id = f.rowid WHERE messages_fts MATCH ?1 ORDER BY bm25(messages_fts) LIMIT ?2")?;
        let rows = statement.query_map(params![query, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect()
    }

    pub fn search_messages_for_workspace(
        &self,
        query: &str,
        workspace: &str,
        limit: usize,
    ) -> SqlResult<Vec<(i64, String, String)>> {
        let mut statement = self.connection.prepare("SELECT m.id, m.role, m.content FROM messages_fts f JOIN messages m ON m.id = f.rowid JOIN sessions s ON s.id = m.session_id WHERE s.workspace = ?1 AND messages_fts MATCH ?2 ORDER BY bm25(messages_fts) LIMIT ?3")?;
        let rows = statement.query_map(params![workspace, query, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect()
    }

    pub fn memory_count(&self) -> SqlResult<usize> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM memory WHERE status = 'active'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
    }

    pub fn transactional<F, T>(&mut self, operation: F) -> SqlResult<T>
    where
        F: FnOnce(&Transaction<'_>) -> SqlResult<T>,
    {
        let transaction = self.connection.transaction()?;
        let result = operation(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn insert_usage(
        transaction: &Transaction<'_>,
        turn_id: i64,
        usage: &UsageRecord,
    ) -> SqlResult<()> {
        transaction.execute(
            "INSERT OR REPLACE INTO turn_usage(turn_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, total_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![turn_id, usage.input_tokens.to_string(), usage.output_tokens.to_string(), usage.cache_read_tokens.to_string(), usage.cache_write_tokens.to_string(), usage.reasoning_tokens.to_string(), usage.total_tokens.to_string()],
        )?;
        Ok(())
    }

    pub fn insert_context(
        transaction: &Transaction<'_>,
        turn_id: i64,
        context: &ContextRecord,
    ) -> SqlResult<()> {
        transaction.execute(
            "INSERT OR REPLACE INTO context_plans(turn_id, budget, estimated_tokens, fingerprint, selected_count, dropped_count, truncated_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![turn_id, context.budget as i64, context.estimated_tokens as i64, context.fingerprint, context.selected_count as i64, context.dropped_count as i64, context.truncated_count as i64],
        )?;
        Ok(())
    }

    pub fn insert_event(
        transaction: &Transaction<'_>,
        session_id: i64,
        turn_id: i64,
        sequence: i64,
        kind: &str,
        payload: &str,
    ) -> SqlResult<()> {
        transaction.execute(
            "INSERT OR REPLACE INTO lifecycle_events(session_id, turn_id, sequence, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))",
            params![session_id, turn_id, sequence, kind, bounded_event_payload(payload)],
        )?;
        Ok(())
    }

    pub fn usage_for_session(&self, session_id: i64) -> SqlResult<UsageRecord> {
        let mut statement = self.connection.prepare("SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, total_tokens FROM turn_usage WHERE turn_id IN (SELECT id FROM turns WHERE session_id = ?1)")?;
        let mut rows = statement.query(params![session_id])?;
        let mut total = UsageRecord::default();
        while let Some(row) = rows.next()? {
            total.input_tokens = total
                .input_tokens
                .saturating_add(row.get::<_, String>(0)?.parse().unwrap_or(0));
            total.output_tokens = total
                .output_tokens
                .saturating_add(row.get::<_, String>(1)?.parse().unwrap_or(0));
            total.cache_read_tokens = total
                .cache_read_tokens
                .saturating_add(row.get::<_, String>(2)?.parse().unwrap_or(0));
            total.cache_write_tokens = total
                .cache_write_tokens
                .saturating_add(row.get::<_, String>(3)?.parse().unwrap_or(0));
            total.reasoning_tokens = total
                .reasoning_tokens
                .saturating_add(row.get::<_, String>(4)?.parse().unwrap_or(0));
            total.total_tokens = total
                .total_tokens
                .saturating_add(row.get::<_, String>(5)?.parse().unwrap_or(0));
        }
        Ok(total)
    }

    pub fn context_for_session(&self, session_id: i64) -> SqlResult<Vec<ContextRecord>> {
        let mut statement = self.connection.prepare("SELECT budget, estimated_tokens, fingerprint, selected_count, dropped_count, truncated_count FROM context_plans WHERE turn_id IN (SELECT id FROM turns WHERE session_id = ?1) ORDER BY turn_id")?;
        let rows = statement.query_map(params![session_id], |row| {
            Ok(ContextRecord {
                budget: row.get::<_, i64>(0)?.max(0) as u64,
                estimated_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                fingerprint: row.get(2)?,
                selected_count: row.get::<_, i64>(3)?.max(0) as usize,
                dropped_count: row.get::<_, i64>(4)?.max(0) as usize,
                truncated_count: row.get::<_, i64>(5)?.max(0) as usize,
            })
        })?;
        rows.collect()
    }

    pub fn events_for_session(&self, session_id: i64) -> SqlResult<Vec<EventRecord>> {
        let mut statement = self.connection.prepare("SELECT sequence, kind, payload FROM lifecycle_events WHERE session_id = ?1 ORDER BY turn_id, sequence")?;
        let rows = statement.query_map(params![session_id], |row| {
            Ok(EventRecord {
                sequence: row.get(0)?,
                kind: row.get(1)?,
                payload: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn audit_records(
        &self,
        session_id: Option<i64>,
        kind: Option<&str>,
        limit: usize,
    ) -> SqlResult<Vec<AuditRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, session_id, turn_id, kind, payload FROM lifecycle_events
             WHERE (?1 IS NULL OR session_id = ?1) AND (?2 IS NULL OR kind = ?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows =
            statement.query_map(params![session_id, kind, limit.min(10_000) as i64], |row| {
                Ok(AuditRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    turn_id: row.get(2)?,
                    kind: row.get(3)?,
                    payload: row.get(4)?,
                })
            })?;
        rows.collect()
    }

    pub fn active_memory_id(&self, scope: &str, kind: &str, key: &str) -> SqlResult<Option<i64>> {
        self.connection.query_row("SELECT id FROM memory WHERE scope = ?1 AND kind = ?2 AND key = ?3 AND status = 'active' ORDER BY id DESC LIMIT 1", params![scope, kind, key], |row| row.get(0)).optional()
    }
}

pub fn bounded_event_payload(payload: &str) -> String {
    let lower = payload.to_ascii_lowercase();
    let secret_like = ["api_key", "authorization", "bearer ", "secret", "token="]
        .iter()
        .any(|marker| lower.contains(marker));
    let value = if secret_like { "[redacted]" } else { payload };
    let bounded: String = value.chars().take(2048).collect();
    serde_json::json!({ "text": bounded }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn path() -> std::path::PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agent-storage-{id}.db"));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn migrations_are_idempotent_and_persist_after_reopen() {
        let path = path();
        {
            let db = Database::open(&path).expect("open");
            assert_eq!(db.user_version().expect("version"), STORAGE_FORMAT_VERSION);
            db.migrate().expect("second migration");
            let session = db.create_session("persist").expect("session");
            let turn = db.append_turn(session, 0).expect("turn");
            db.update_turn_status(turn, "failed", Some("bounded failure"))
                .expect("status");
            assert_eq!(
                db.turn_status(turn).expect("turn status"),
                Some(("failed".into(), Some("bounded failure".into())))
            );
            db.append_message(session, turn, "user", "find persistent fact")
                .expect("message");
        }
        let db = Database::open(&path).expect("reopen");
        assert_eq!(
            db.search_messages("persistent", 10).expect("search").len(),
            1
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let mut db = Database::in_memory().expect("db");
        let result: SqlResult<()> = db.transactional(|transaction| {
            transaction.execute(
                "INSERT INTO sessions(objective, created_at) VALUES ('rollback', 0)",
                [],
            )?;
            Err(rusqlite::Error::InvalidQuery)
        });
        assert!(result.is_err());
        let count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 0);
    }

    #[test]
    fn observability_roundtrips_and_redacts_bounded_payloads() {
        let mut db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("observe", "/workspace")
            .expect("session");
        let turn = db.append_turn(session, 0).expect("turn");
        db.transactional(|transaction| {
            Database::insert_usage(
                transaction,
                turn,
                &UsageRecord {
                    input_tokens: u64::MAX,
                    output_tokens: 2,
                    total_tokens: u64::MAX,
                    ..UsageRecord::default()
                },
            )?;
            Database::insert_context(
                transaction,
                turn,
                &ContextRecord {
                    budget: 100,
                    estimated_tokens: 42,
                    fingerprint: "stable".into(),
                    selected_count: 2,
                    dropped_count: 1,
                    truncated_count: 1,
                },
            )?;
            Database::insert_event(
                transaction,
                session,
                turn,
                0,
                "tool_output",
                &"x".repeat(5000),
            )?;
            Database::insert_event(
                transaction,
                session,
                turn,
                1,
                "secret",
                "Authorization: bearer hidden",
            )?;
            Ok(())
        })
        .expect("observability");
        assert_eq!(
            db.usage_for_session(session).expect("usage").total_tokens,
            u64::MAX
        );
        assert_eq!(
            db.context_for_session(session).expect("context")[0].fingerprint,
            "stable"
        );
        let events = db.events_for_session(session).expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&events[0].payload).expect("json")["text"]
                .as_str()
                .expect("text")
                .len(),
            2048
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&events[1].payload).expect("json")["text"],
            "[redacted]"
        );
    }

    #[test]
    fn sessions_archive_and_delete_are_workspace_scoped_and_hidden_from_list() {
        let db = Database::in_memory().expect("db");
        let first = db
            .create_session_for_workspace("first", "/workspace")
            .expect("first");
        let second = db
            .create_session_for_workspace("second", "/workspace")
            .expect("second");
        assert!(db
            .archive_session(first, "/workspace", true)
            .expect("archive"));
        assert!(db
            .list_sessions("/workspace")
            .expect("list")
            .iter()
            .all(|item| item.id != first));
        assert!(!db
            .archive_session(second, "/other", true)
            .expect("wrong workspace"));
        assert!(db.delete_session(second, "/workspace").expect("delete"));
        assert!(db.list_sessions("/workspace").expect("list").is_empty());
    }
}
