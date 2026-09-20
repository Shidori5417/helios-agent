use agent_storage::Database;
use rusqlite::{params, OptionalExtension, Result as SqlResult};
use std::path::Path;

pub const STARTUP_MEMORY_TOKEN_TARGET: usize = 1_000;
pub const MEMORY_KINDS: &[&str] = &[
    "user_preference",
    "project_fact",
    "architecture_decision",
    "workflow_knowledge",
    "temporary",
    "fact",
];

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryEntry {
    pub scope: String,
    pub kind: String,
    pub key: String,
    pub content: String,
    pub confidence: f64,
    pub importance: i64,
    pub source_session_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredMemory {
    pub id: i64,
    pub entry: MemoryEntry,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("memory content exceeds configured limit of {limit} bytes")]
    ContentTooLarge { limit: usize },
    #[error("memory entry limit reached ({limit})")]
    EntryLimit { limit: usize },
    #[error("secret-like memory content is not allowed")]
    SecretLike,
    #[error("unsupported memory category: {0}")]
    InvalidKind(String),
}

pub trait MemoryStore {
    fn put(&mut self, entry: MemoryEntry) -> Result<i64, MemoryError>;
    fn propose(&mut self, entry: MemoryEntry) -> Result<i64, MemoryError> {
        self.put(entry)
    }
    fn search(&self, query: &str, limit: usize) -> Result<Vec<StoredMemory>, MemoryError>;
    fn search_scoped(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<StoredMemory>, MemoryError> {
        Ok(self
            .search(query, limit.saturating_mul(4).max(limit))?
            .into_iter()
            .filter(|memory| memory.entry.scope == scope)
            .take(limit)
            .collect())
    }
    fn list(&self, scope: &str, limit: usize) -> Result<Vec<StoredMemory>, MemoryError>;
    fn supersede(&mut self, id: i64) -> Result<(), MemoryError>;
    fn delete(&mut self, id: i64) -> Result<(), MemoryError>;
    fn supersede_scoped(&mut self, scope: &str, id: i64) -> Result<(), MemoryError> {
        if self
            .list(scope, 1_000_000)?
            .iter()
            .any(|memory| memory.id == id)
        {
            self.supersede(id)?;
        }
        Ok(())
    }
    fn delete_scoped(&mut self, scope: &str, id: i64) -> Result<(), MemoryError> {
        if self
            .list(scope, 1_000_000)?
            .iter()
            .any(|memory| memory.id == id)
        {
            self.delete(id)?;
        }
        Ok(())
    }
    fn count(&self) -> Result<usize, MemoryError>;
    fn pending(&self, _scope: &str, _limit: usize) -> Result<Vec<StoredMemory>, MemoryError> {
        Ok(Vec::new())
    }
    fn approve_pending(&mut self, _scope: &str, _id: i64) -> Result<bool, MemoryError> {
        Ok(false)
    }
    fn reject_pending(&mut self, _scope: &str, _id: i64) -> Result<bool, MemoryError> {
        Ok(false)
    }
}

pub struct SqliteMemoryStore {
    database: Database,
    max_entries: usize,
    max_content_bytes: usize,
}

impl SqliteMemoryStore {
    pub fn open(
        path: impl AsRef<Path>,
        max_entries: usize,
        max_content_bytes: usize,
    ) -> Result<Self, MemoryError> {
        Ok(Self {
            database: Database::open(path)?,
            max_entries,
            max_content_bytes,
        })
    }

    pub fn in_memory(max_entries: usize, max_content_bytes: usize) -> Result<Self, MemoryError> {
        Ok(Self {
            database: Database::in_memory()?,
            max_entries,
            max_content_bytes,
        })
    }
}

impl MemoryStore for SqliteMemoryStore {
    fn put(&mut self, entry: MemoryEntry) -> Result<i64, MemoryError> {
        if !MEMORY_KINDS.contains(&entry.kind.as_str()) {
            return Err(MemoryError::InvalidKind(entry.kind));
        }
        if entry.content.len() > self.max_content_bytes {
            return Err(MemoryError::ContentTooLarge {
                limit: self.max_content_bytes,
            });
        }
        if is_secret_like(&entry.key) || is_secret_like(&entry.content) {
            return Err(MemoryError::SecretLike);
        }
        let max_entries = self.max_entries;
        let scope = entry.scope;
        let kind = entry.kind;
        let key = entry.key;
        let content = entry.content;
        let confidence = entry.confidence;
        let importance = entry.importance;
        let source_session_id = entry.source_session_id;
        let id = self.database.transactional(|transaction| {
            let count: i64 = transaction.query_row("SELECT COUNT(*) FROM memory WHERE status = 'active'", [], |row| row.get(0))?;
            let existing: Option<i64> = transaction.query_row("SELECT id FROM memory WHERE scope = ?1 AND kind = ?2 AND key = ?3 AND status = 'active' ORDER BY id DESC LIMIT 1", params![&scope, &kind, &key], |row| row.get(0)).optional()?;
            if existing.is_none() && count as usize >= max_entries { return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(LimitReached { limit: max_entries }))); }
            if let Some(old_id) = existing { transaction.execute("UPDATE memory SET status = 'superseded' WHERE id = ?1", params![old_id])?; }
            transaction.execute("INSERT INTO memory(scope, kind, key, content, confidence, importance, source_session_id, created_at, updated_at, last_accessed_at, access_count, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'), strftime('%s','now'), strftime('%s','now'), 0, 'active')", params![&scope, &kind, &key, &content, confidence, importance, source_session_id])?;
            Ok(transaction.last_insert_rowid())
        }).map_err(|error| match error { rusqlite::Error::ToSqlConversionFailure(source) if source.downcast_ref::<LimitReached>().is_some() => MemoryError::EntryLimit { limit: max_entries }, other => MemoryError::Database(other) })?;
        Ok(id)
    }

    fn propose(&mut self, entry: MemoryEntry) -> Result<i64, MemoryError> {
        if !MEMORY_KINDS.contains(&entry.kind.as_str()) {
            return Err(MemoryError::InvalidKind(entry.kind));
        }
        if entry.content.len() > self.max_content_bytes {
            return Err(MemoryError::ContentTooLarge {
                limit: self.max_content_bytes,
            });
        }
        if is_secret_like(&entry.key) || is_secret_like(&entry.content) {
            return Err(MemoryError::SecretLike);
        }
        Ok(self.database.add_pending_memory(
            &entry.scope,
            &entry.kind,
            &entry.key,
            &entry.content,
            entry.confidence,
            entry.importance,
        )?)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<StoredMemory>, MemoryError> {
        let mut statement = self.database.connection().prepare("SELECT m.id, m.scope, m.kind, m.key, m.content, m.confidence, m.importance, m.source_session_id FROM memory_fts f JOIN memory m ON m.id = f.rowid WHERE memory_fts MATCH ?1 AND m.status = 'active' ORDER BY bm25(memory_fts), m.importance DESC LIMIT ?2")?;
        let rows = statement.query_map(params![query, limit as i64], |row| {
            Ok(StoredMemory {
                id: row.get(0)?,
                entry: MemoryEntry {
                    scope: row.get(1)?,
                    kind: row.get(2)?,
                    key: row.get(3)?,
                    content: row.get(4)?,
                    confidence: row.get(5)?,
                    importance: row.get(6)?,
                    source_session_id: row.get(7)?,
                },
            })
        })?;
        Ok(rows.collect::<SqlResult<Vec<_>>>()?)
    }

    fn count(&self) -> Result<usize, MemoryError> {
        Ok(self.database.memory_count()?)
    }

    fn pending(&self, scope: &str, limit: usize) -> Result<Vec<StoredMemory>, MemoryError> {
        Ok(self
            .database
            .pending_memory(scope, limit)?
            .into_iter()
            .map(|memory| StoredMemory {
                id: memory.id,
                entry: MemoryEntry {
                    scope: memory.scope,
                    kind: memory.kind,
                    key: memory.key,
                    content: memory.content,
                    confidence: memory.confidence,
                    importance: memory.importance,
                    source_session_id: None,
                },
            })
            .collect())
    }

    fn approve_pending(&mut self, scope: &str, id: i64) -> Result<bool, MemoryError> {
        Ok(self.database.approve_pending_memory(id, scope)?)
    }

    fn reject_pending(&mut self, scope: &str, id: i64) -> Result<bool, MemoryError> {
        Ok(self.database.reject_pending_memory(id, scope)?)
    }

    fn list(&self, scope: &str, limit: usize) -> Result<Vec<StoredMemory>, MemoryError> {
        let mut statement = self.database.connection().prepare(
            "SELECT id, scope, kind, key, content, confidence, importance, source_session_id FROM memory WHERE scope = ?1 AND status = 'active' ORDER BY importance DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![scope, limit as i64], |row| {
            Ok(StoredMemory {
                id: row.get(0)?,
                entry: MemoryEntry {
                    scope: row.get(1)?,
                    kind: row.get(2)?,
                    key: row.get(3)?,
                    content: row.get(4)?,
                    confidence: row.get(5)?,
                    importance: row.get(6)?,
                    source_session_id: row.get(7)?,
                },
            })
        })?;
        Ok(rows.collect::<SqlResult<Vec<_>>>()?)
    }

    fn supersede(&mut self, id: i64) -> Result<(), MemoryError> {
        self.database
            .connection()
            .execute(
                "UPDATE memory SET status = 'superseded', updated_at = strftime('%s','now') WHERE id = ?1 AND status = 'active'",
                params![id],
            )?;
        Ok(())
    }

    fn delete(&mut self, id: i64) -> Result<(), MemoryError> {
        self.database
            .connection()
            .execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        Ok(())
    }
}

#[derive(Debug)]
struct LimitReached {
    limit: usize,
}
impl std::fmt::Display for LimitReached {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "memory entry limit {}", self.limit)
    }
}
impl std::error::Error for LimitReached {}

fn is_secret_like(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "api_token",
        "access_token",
        "refresh_token",
        "token",
        "password",
        "secret",
        "credential",
        "private_key",
        "authorization",
        "bearer ",
        "sk-",
    ]
    .iter()
    .any(|pattern| value.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn entry(key: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            scope: "project".into(),
            kind: "fact".into(),
            key: key.into(),
            content: content.into(),
            confidence: 1.0,
            importance: 1,
            source_session_id: None,
        }
    }
    fn path() -> std::path::PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agent-memory-{id}.db"));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn persists_and_searches_after_reopen() {
        let path = path();
        {
            let mut store = SqliteMemoryStore::open(&path, 4, 1024).expect("store");
            store
                .put(entry("build", "cargo test is the project check"))
                .expect("put");
        }
        let store = SqliteMemoryStore::open(&path, 4, 1024).expect("reopen");
        assert_eq!(store.search("cargo", 10).expect("search").len(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bounds_reject_large_and_excess_entries() {
        let mut store = SqliteMemoryStore::in_memory(1, 4).expect("store");
        assert!(matches!(
            store.put(entry("too-large", "12345")),
            Err(MemoryError::ContentTooLarge { .. })
        ));
        store.put(entry("one", "1234")).expect("first");
        assert!(matches!(
            store.put(entry("two", "5678")),
            Err(MemoryError::EntryLimit { .. })
        ));
    }

    #[test]
    fn secret_like_content_is_rejected() {
        let mut store = SqliteMemoryStore::in_memory(4, 1024).expect("store");
        assert!(matches!(
            store.put(entry("api_token", "do not persist")),
            Err(MemoryError::SecretLike)
        ));
        assert!(matches!(
            store.put(entry("safe", "Bearer abc")),
            Err(MemoryError::SecretLike)
        ));
    }

    #[test]
    fn pending_entries_require_explicit_approval() {
        let mut store = SqliteMemoryStore::in_memory(4, 1024).expect("store");
        let id = store
            .propose(entry("candidate", "safe fact"))
            .expect("propose");
        assert_eq!(store.pending("project", 10).expect("pending").len(), 1);
        assert!(store.approve_pending("project", id).expect("approve"));
        assert_eq!(store.list("project", 10).expect("list").len(), 1);
    }

    #[test]
    fn memory_categories_are_typed() {
        let mut store = SqliteMemoryStore::in_memory(4, 1024).expect("store");
        assert!(store.put(entry("x", "safe")).is_ok());
        let mut invalid = entry("x2", "safe");
        invalid.kind = "arbitrary".into();
        assert!(matches!(
            store.put(invalid),
            Err(MemoryError::InvalidKind(_))
        ));
    }
}
