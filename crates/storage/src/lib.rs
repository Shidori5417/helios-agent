use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult, Transaction};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub const STORAGE_FORMAT_VERSION: u32 = 12;

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
    pub title: Option<String>,
    pub workspace: String,
    pub archived: bool,
    pub deleted_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionSummary {
    pub id: i64,
    pub objective: String,
    pub title: Option<String>,
    pub workspace: String,
    pub created_at: i64,
    pub last_activity: Option<i64>,
    pub turn_count: i64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub git_branch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionOverride {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionToolOverride {
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingMemoryRecord {
    pub id: i64,
    pub scope: String,
    pub kind: String,
    pub key: String,
    pub content: String,
    pub confidence: f64,
    pub importance: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueueItem {
    pub id: i64,
    pub session_id: i64,
    pub prompt: String,
    pub position: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CheckpointRecord {
    pub id: i64,
    pub session_id: i64,
    pub message_id: i64,
    pub workspace: String,
    pub label: String,
    pub reason: String,
    pub created_at: i64,
    pub pinned: bool,
}

pub const CHECKPOINT_FILE_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CheckpointFileRecord {
    pub id: i64,
    pub checkpoint_id: i64,
    pub workspace: String,
    pub relative_path: String,
    pub existed: bool,
    pub mode: Option<u32>,
    pub size: u64,
    pub pre_hash: String,
    pub post_hash: Option<String>,
    pub content: Option<Vec<u8>>,
    pub available: bool,
}

pub struct Database {
    connection: Connection,
}

fn hash_bytes(content: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(content);
    format!("{:x}", digest.finalize())
}

fn sensitive_checkpoint_path(path: &str) -> bool {
    path.split('/').any(|part| {
        let lower = part.to_ascii_lowercase();
        lower == ".env"
            || lower.contains("secret")
            || lower.contains("token")
            || lower.contains("password")
            || lower == "id_rsa"
    })
}

fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
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
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            let parent_existed = parent.is_dir();
            std::fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            #[cfg(unix)]
            if !parent_existed {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            }
        }
        let connection = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
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
        if version < 6 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN provider TEXT;
                 ALTER TABLE sessions ADD COLUMN model TEXT;
                 ALTER TABLE sessions ADD COLUMN effort TEXT;
                 ALTER TABLE sessions ADD COLUMN git_branch TEXT;
                 PRAGMA user_version = 6;",
            )?;
        }
        if version < 7 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN title TEXT;
                 CREATE TABLE IF NOT EXISTS context_boundaries (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id), message_id INTEGER NOT NULL, reason TEXT NOT NULL, created_at INTEGER NOT NULL);
                 CREATE INDEX IF NOT EXISTS context_boundaries_session ON context_boundaries(session_id, id);
                 PRAGMA user_version = 7;",
            )?;
        }
        if version < 8 {
            self.connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_memory (id INTEGER PRIMARY KEY, scope TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, content TEXT NOT NULL, confidence REAL NOT NULL, importance INTEGER NOT NULL, created_at INTEGER NOT NULL);
                PRAGMA user_version = 8;",
            )?;
        }
        if version < 9 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN tools_enabled TEXT NOT NULL DEFAULT '[]';
                 ALTER TABLE sessions ADD COLUMN tools_disabled TEXT NOT NULL DEFAULT '[]';
                 PRAGMA user_version = 9;",
            )?;
        }
        if version < 10 {
            self.connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS foreground_queue (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id), prompt TEXT NOT NULL, position INTEGER NOT NULL, created_at INTEGER NOT NULL);
                 CREATE INDEX IF NOT EXISTS foreground_queue_session_position ON foreground_queue(session_id, position);
                 PRAGMA user_version = 10;",
            )?;
        }
        if version < 11 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN parent_session_id INTEGER REFERENCES sessions(id);
                 ALTER TABLE sessions ADD COLUMN lineage_kind TEXT NOT NULL DEFAULT 'root';
                 CREATE TABLE IF NOT EXISTS checkpoints (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id), message_id INTEGER NOT NULL, workspace TEXT NOT NULL, label TEXT NOT NULL, reason TEXT NOT NULL, created_at INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0);
                 CREATE INDEX IF NOT EXISTS checkpoints_session ON checkpoints(session_id, id);
                 PRAGMA user_version = 11;",
            )?;
        }
        if version < 12 {
            self.connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS checkpoint_files (id INTEGER PRIMARY KEY, checkpoint_id INTEGER NOT NULL REFERENCES checkpoints(id) ON DELETE CASCADE, session_id INTEGER NOT NULL REFERENCES sessions(id), workspace TEXT NOT NULL, relative_path TEXT NOT NULL, existed INTEGER NOT NULL, mode INTEGER, size INTEGER NOT NULL, pre_hash TEXT NOT NULL, post_hash TEXT, content BLOB, available INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, UNIQUE(checkpoint_id, relative_path));
                 CREATE INDEX IF NOT EXISTS checkpoint_files_checkpoint ON checkpoint_files(checkpoint_id, id);
                 PRAGMA user_version = 12;",
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

    pub fn create_lineage_session(
        &self,
        objective: &str,
        workspace: &str,
        parent: i64,
        kind: &str,
        message_id: i64,
    ) -> SqlResult<i64> {
        let parent_workspace: Option<String> = self
            .connection
            .query_row(
                "SELECT workspace FROM sessions WHERE id=?1",
                params![parent],
                |row| row.get(0),
            )
            .optional()?;
        if parent_workspace.as_deref() != Some(workspace) {
            return Err(rusqlite::Error::InvalidParameterName(
                "lineage workspace mismatch".into(),
            ));
        }
        self.connection.execute("INSERT INTO sessions(objective,workspace,created_at,parent_session_id,lineage_kind) VALUES (?1,?2,strftime('%s','now'),?3,?4)", params![objective, workspace, parent, kind])?;
        let id = self.connection.last_insert_rowid();
        let mut statement = self.connection.prepare(
            "SELECT role,content FROM messages WHERE session_id=?1 AND id<=?2 ORDER BY id",
        )?;
        let messages = statement
            .query_map(params![parent, message_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<SqlResult<Vec<_>>>()?;
        for (role, content) in messages.into_iter().take(256) {
            self.connection.execute("INSERT INTO messages(session_id,role,content,created_at) VALUES (?1,?2,?3,strftime('%s','now'))", params![id,role,content])?;
        }
        Ok(id)
    }

    pub fn create_checkpoint(
        &self,
        session_id: i64,
        workspace: &str,
        label: &str,
        reason: &str,
    ) -> SqlResult<i64> {
        let session_workspace: Option<String> = self
            .connection
            .query_row(
                "SELECT workspace FROM sessions WHERE id=?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        if session_workspace.as_deref() != Some(workspace) {
            return Err(rusqlite::Error::InvalidParameterName(
                "checkpoint workspace mismatch".into(),
            ));
        }
        let message_id: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(id),0) FROM messages WHERE session_id=?1",
            params![session_id],
            |row| row.get(0),
        )?;
        if label.len() > 256 || reason.len() > 1024 {
            return Err(rusqlite::Error::InvalidParameterName(
                "checkpoint metadata bounds".into(),
            ));
        }
        self.connection.execute("INSERT INTO checkpoints(session_id,message_id,workspace,label,reason,created_at) VALUES (?1,?2,?3,?4,?5,strftime('%s','now'))", params![session_id,message_id,workspace,label,reason])?;
        let id = self.connection.last_insert_rowid();
        self.prune_checkpoints(session_id, workspace, 64)?;
        Ok(id)
    }

    pub fn checkpoints(
        &self,
        session_id: i64,
        workspace: &str,
    ) -> SqlResult<Vec<CheckpointRecord>> {
        let mut statement = self.connection.prepare("SELECT id,session_id,message_id,workspace,label,reason,created_at,pinned FROM checkpoints WHERE session_id=?1 AND workspace=?2 ORDER BY id DESC LIMIT 128")?;
        let rows = statement
            .query_map(params![session_id, workspace], |row| {
                Ok(CheckpointRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    message_id: row.get(2)?,
                    workspace: row.get(3)?,
                    label: row.get(4)?,
                    reason: row.get(5)?,
                    created_at: row.get(6)?,
                    pinned: row.get::<_, i64>(7)? != 0,
                })
            })?
            .collect();
        rows
    }

    pub fn checkpoint(
        &self,
        id: i64,
        session_id: i64,
        workspace: &str,
    ) -> SqlResult<Option<CheckpointRecord>> {
        self.checkpoints(session_id, workspace)
            .map(|items| items.into_iter().find(|item| item.id == id))
    }

    pub fn record_checkpoint_file(
        &self,
        checkpoint_id: i64,
        workspace: &str,
        path: &Path,
        existed: bool,
        content: &[u8],
    ) -> SqlResult<i64> {
        let workspace_path = Path::new(workspace)
            .canonicalize()
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let candidate =
            if path.exists() {
                path.canonicalize()
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?
            } else {
                let parent = path
                    .parent()
                    .unwrap_or(&workspace_path)
                    .canonicalize()
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                parent.join(path.file_name().ok_or_else(|| {
                    rusqlite::Error::InvalidParameterName("checkpoint path".into())
                })?)
            };
        let relative = candidate.strip_prefix(&workspace_path).map_err(|_| {
            rusqlite::Error::InvalidParameterName("checkpoint path outside workspace".into())
        })?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.is_empty() || relative.contains("..") || sensitive_checkpoint_path(&relative) {
            return Err(rusqlite::Error::InvalidParameterName(
                "checkpoint path rejected".into(),
            ));
        }
        let checkpoint_workspace: String = self.connection.query_row(
            "SELECT workspace FROM checkpoints WHERE id=?1",
            params![checkpoint_id],
            |row| row.get(0),
        )?;
        if checkpoint_workspace != workspace {
            return Err(rusqlite::Error::InvalidParameterName(
                "checkpoint workspace mismatch".into(),
            ));
        }
        let hash = hash_bytes(content);
        let available = !existed || (path.is_file() && content.len() <= CHECKPOINT_FILE_MAX_BYTES);
        let stored = available.then(|| content.to_vec());
        let mode = file_mode(path);
        self.connection.execute(
            "INSERT OR IGNORE INTO checkpoint_files(checkpoint_id,session_id,workspace,relative_path,existed,mode,size,pre_hash,content,available,created_at) SELECT ?1,session_id,workspace,?2,?3,?4,?5,?6,?7,?8,strftime('%s','now') FROM checkpoints WHERE id=?1",
            params![checkpoint_id, relative, existed, mode.map(i64::from), content.len() as i64, hash, stored, available],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn update_checkpoint_file_after(
        &self,
        checkpoint_id: i64,
        workspace: &str,
        path: &Path,
        content: &[u8],
    ) -> SqlResult<bool> {
        let workspace_path = Path::new(workspace)
            .canonicalize()
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let candidate = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let relative = candidate
            .strip_prefix(&workspace_path)
            .map_err(|_| {
                rusqlite::Error::InvalidParameterName("checkpoint path outside workspace".into())
            })?
            .to_string_lossy()
            .replace('\\', "/");
        let changed = self.connection.execute(
            "UPDATE checkpoint_files SET post_hash=?1 WHERE checkpoint_id=?2 AND workspace=?3 AND relative_path=?4",
            params![hash_bytes(content), checkpoint_id, workspace, relative],
        )?;
        Ok(changed != 0)
    }

    pub fn checkpoint_files(
        &self,
        checkpoint_id: i64,
        workspace: &str,
    ) -> SqlResult<Vec<CheckpointFileRecord>> {
        let mut statement = self.connection.prepare("SELECT id,checkpoint_id,workspace,relative_path,existed,mode,size,pre_hash,post_hash,content,available FROM checkpoint_files WHERE checkpoint_id=?1 AND workspace=?2 ORDER BY id LIMIT 256")?;
        let rows = statement
            .query_map(params![checkpoint_id, workspace], |row| {
                Ok(CheckpointFileRecord {
                    id: row.get(0)?,
                    checkpoint_id: row.get(1)?,
                    workspace: row.get(2)?,
                    relative_path: row.get(3)?,
                    existed: row.get::<_, i64>(4)? != 0,
                    mode: row.get::<_, Option<i64>>(5)?.map(|value| value as u32),
                    size: row.get::<_, i64>(6)? as u64,
                    pre_hash: row.get(7)?,
                    post_hash: row.get(8)?,
                    content: row.get(9)?,
                    available: row.get::<_, i64>(10)? != 0,
                })
            })?
            .collect();
        rows
    }

    pub fn pin_checkpoint(
        &self,
        id: i64,
        session_id: i64,
        workspace: &str,
        pinned: bool,
    ) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "UPDATE checkpoints SET pinned=?1 WHERE id=?2 AND session_id=?3 AND workspace=?4",
            params![pinned, id, session_id, workspace],
        )? != 0)
    }

    pub fn prune_checkpoints(
        &self,
        session_id: i64,
        workspace: &str,
        keep: usize,
    ) -> SqlResult<usize> {
        self.connection.execute(
            "DELETE FROM checkpoints WHERE id IN (SELECT id FROM checkpoints WHERE session_id=?1 AND workspace=?2 AND pinned=0 ORDER BY id DESC LIMIT -1 OFFSET ?3)",
            params![session_id, workspace, keep as i64],
        )
    }

    pub fn session(&self, id: i64) -> SqlResult<Option<SessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, objective, title, workspace, archived, deleted_at FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        objective: row.get(1)?,
                        title: row.get(2)?,
                        workspace: row.get(3)?,
                        archived: row.get::<_, i64>(4)? != 0,
                        deleted_at: row.get(5)?,
                    })
                },
            )
            .optional()
    }

    pub fn latest_session(&self, workspace: &str) -> SqlResult<Option<SessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, objective, title, workspace, archived, deleted_at FROM sessions WHERE workspace = ?1 AND archived = 0 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1",
                params![workspace],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        objective: row.get(1)?,
                        title: row.get(2)?,
                        workspace: row.get(3)?,
                        archived: row.get::<_, i64>(4)? != 0,
                        deleted_at: row.get(5)?,
                    })
                },
            )
            .optional()
    }

    pub fn latest_session_any(&self) -> SqlResult<Option<SessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, objective, title, workspace, archived, deleted_at FROM sessions WHERE archived = 0 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        objective: row.get(1)?,
                        title: row.get(2)?,
                        workspace: row.get(3)?,
                        archived: row.get::<_, i64>(4)? != 0,
                        deleted_at: row.get(5)?,
                    })
                },
            )
            .optional()
    }

    pub fn list_sessions(&self, workspace: &str) -> SqlResult<Vec<SessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, objective, title, workspace, archived, deleted_at FROM sessions WHERE workspace = ?1 AND archived = 0 AND deleted_at IS NULL ORDER BY id DESC",
        )?;
        let rows = statement.query_map(params![workspace], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                objective: row.get(1)?,
                title: row.get(2)?,
                workspace: row.get(3)?,
                archived: row.get::<_, i64>(4)? != 0,
                deleted_at: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_session_summaries(&self, workspace: &str) -> SqlResult<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.objective, s.title, s.workspace, s.created_at, MAX(t.created_at), COUNT(t.id), s.provider, s.model, s.effort, s.git_branch
             FROM sessions s LEFT JOIN turns t ON t.session_id = s.id
             WHERE s.workspace = ?1 AND s.archived = 0 AND s.deleted_at IS NULL
             GROUP BY s.id ORDER BY s.id DESC",
        )?;
        let rows = statement.query_map(params![workspace], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                objective: row.get(1)?,
                title: row.get(2)?,
                workspace: row.get(3)?,
                created_at: row.get(4)?,
                last_activity: row.get(5)?,
                turn_count: row.get(6)?,
                provider: row.get(7)?,
                model: row.get(8)?,
                effort: row.get(9)?,
                git_branch: row.get(10)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_session_summaries_page(
        &self,
        workspace: &str,
        limit: usize,
        offset: usize,
    ) -> SqlResult<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.objective, s.title, s.workspace, s.created_at, MAX(t.created_at), COUNT(t.id), s.provider, s.model, s.effort, s.git_branch
             FROM sessions s LEFT JOIN turns t ON t.session_id = s.id
             WHERE s.workspace = ?1 AND s.archived = 0 AND s.deleted_at IS NULL
             GROUP BY s.id ORDER BY s.id DESC LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(params![workspace, limit as i64, offset as i64], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                objective: row.get(1)?,
                title: row.get(2)?,
                workspace: row.get(3)?,
                created_at: row.get(4)?,
                last_activity: row.get(5)?,
                turn_count: row.get(6)?,
                provider: row.get(7)?,
                model: row.get(8)?,
                effort: row.get(9)?,
                git_branch: row.get(10)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_session_override(
        &self,
        id: i64,
        workspace: &str,
        provider: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        git_branch: Option<&str>,
    ) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET provider = COALESCE(?1, provider), model = COALESCE(?2, model), effort = COALESCE(?3, effort), git_branch = COALESCE(?4, git_branch) WHERE id = ?5 AND workspace = ?6 AND archived = 0 AND deleted_at IS NULL",
            params![provider, model, effort, git_branch, id, workspace],
        )? == 1)
    }

    pub fn session_tool_override(
        &self,
        id: i64,
        workspace: &str,
    ) -> SqlResult<Option<SessionToolOverride>> {
        self.connection.query_row(
            "SELECT tools_enabled, tools_disabled FROM sessions WHERE id = ?1 AND workspace = ?2 AND archived = 0 AND deleted_at IS NULL",
            params![id, workspace],
            |row| {
                let enabled = serde_json::from_str(&row.get::<_, String>(0)?).unwrap_or_default();
                let disabled = serde_json::from_str(&row.get::<_, String>(1)?).unwrap_or_default();
                Ok(SessionToolOverride { enabled, disabled })
            },
        ).optional()
    }

    pub fn set_session_tool_override(
        &self,
        id: i64,
        workspace: &str,
        override_: &SessionToolOverride,
    ) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET tools_enabled = ?1, tools_disabled = ?2 WHERE id = ?3 AND workspace = ?4 AND archived = 0 AND deleted_at IS NULL",
            params![serde_json::to_string(&override_.enabled).unwrap_or_else(|_| "[]".into()), serde_json::to_string(&override_.disabled).unwrap_or_else(|_| "[]".into()), id, workspace],
        )? == 1)
    }

    pub fn set_session_title(&self, id: i64, workspace: &str, title: &str) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2 AND workspace = ?3 AND archived = 0 AND deleted_at IS NULL",
            params![title, id, workspace],
        )? == 1)
    }

    pub fn create_context_boundary(&self, session_id: i64, reason: &str) -> SqlResult<i64> {
        let message_id: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO context_boundaries(session_id, message_id, reason, created_at) VALUES (?1, ?2, ?3, strftime('%s','now'))",
            params![session_id, message_id, reason],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn create_context_boundary_at(
        &self,
        session_id: i64,
        message_id: i64,
        reason: &str,
    ) -> SqlResult<i64> {
        self.connection.execute("INSERT INTO context_boundaries(session_id,message_id,reason,created_at) VALUES (?1,?2,?3,strftime('%s','now'))", params![session_id,message_id,reason])?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn persist_compaction_checkpoint(
        &self,
        session_id: i64,
        reason: &str,
        summary: &str,
    ) -> SqlResult<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let message_id: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO context_boundaries(session_id, message_id, reason, created_at) VALUES (?1, ?2, ?3, strftime('%s','now'))",
            params![session_id, message_id, reason],
        )?;
        transaction.execute(
            "INSERT INTO messages(session_id, turn_id, role, content, created_at) VALUES (?1, NULL, 'system', ?2, strftime('%s','now'))",
            params![session_id, summary.chars().take(4096).collect::<String>()],
        )?;
        transaction.commit()
    }

    pub fn session_context_messages(&self, session_id: i64) -> SqlResult<Vec<(String, String)>> {
        let boundary: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(message_id), 0) FROM context_boundaries WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let mut statement = self.connection.prepare(
            "SELECT role, content FROM messages WHERE session_id = ?1 AND id > ?2 ORDER BY id",
        )?;
        let rows = statement.query_map(params![session_id, boundary], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    pub fn last_user_message(&self, session_id: i64) -> SqlResult<Option<String>> {
        self.connection.query_row(
            "SELECT content FROM messages WHERE session_id = ?1 AND role = 'user' ORDER BY id DESC LIMIT 1",
            params![session_id],
            |row| row.get(0),
        ).optional()
    }

    pub fn create_boundary_before_last_exchange(
        &self,
        session_id: i64,
        reason: &str,
    ) -> SqlResult<bool> {
        let message_id: Option<i64> = self.connection.query_row(
            "SELECT id FROM messages WHERE session_id = ?1 AND role = 'user' ORDER BY id DESC LIMIT 1",
            params![session_id],
            |row| row.get(0),
        ).optional()?;
        let Some(_message_id) = message_id else {
            return Ok(false);
        };
        let boundary: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO context_boundaries(session_id, message_id, reason, created_at) VALUES (?1, ?2, ?3, strftime('%s','now'))",
            params![session_id, boundary, reason],
        )?;
        Ok(true)
    }

    pub fn session_override(&self, id: i64, workspace: &str) -> SqlResult<Option<SessionOverride>> {
        self.connection
            .query_row(
                "SELECT provider, model, effort FROM sessions WHERE id = ?1 AND workspace = ?2 AND archived = 0 AND deleted_at IS NULL",
                params![id, workspace],
                |row| {
                    Ok(SessionOverride {
                        provider: row.get(0)?,
                        model: row.get(1)?,
                        effort: row.get(2)?,
                    })
                },
            )
            .optional()
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

    pub fn enqueue_foreground(&self, session_id: i64, prompt: &str) -> SqlResult<i64> {
        if prompt.trim().is_empty() || prompt.len() > 32 * 1024 {
            return Err(rusqlite::Error::InvalidParameterName(
                "queue prompt bounds".into(),
            ));
        }
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM foreground_queue WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        if count >= 32 {
            return Err(rusqlite::Error::InvalidParameterName(
                "queue capacity".into(),
            ));
        }
        let position: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM foreground_queue WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO foreground_queue(session_id,prompt,position,created_at) VALUES (?1,?2,?3,strftime('%s','now'))",
            params![session_id, prompt, position])?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn foreground_queue(&self, session_id: i64) -> SqlResult<Vec<QueueItem>> {
        let mut statement = self.connection.prepare("SELECT id,session_id,prompt,position FROM foreground_queue WHERE session_id=?1 ORDER BY position,id")?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(QueueItem {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    prompt: row.get(2)?,
                    position: row.get(3)?,
                })
            })?
            .collect();
        rows
    }

    pub fn remove_foreground(&self, session_id: i64, id: i64) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "DELETE FROM foreground_queue WHERE session_id=?1 AND id=?2",
            params![session_id, id],
        )? > 0)
    }

    pub fn edit_foreground(&self, session_id: i64, id: i64, prompt: &str) -> SqlResult<bool> {
        if prompt.trim().is_empty() || prompt.len() > 32 * 1024 {
            return Err(rusqlite::Error::InvalidParameterName(
                "queue prompt bounds".into(),
            ));
        }
        Ok(self.connection.execute(
            "UPDATE foreground_queue SET prompt=?1 WHERE session_id=?2 AND id=?3",
            params![prompt, session_id, id],
        )? > 0)
    }

    pub fn move_foreground(&self, session_id: i64, id: i64, new_position: i64) -> SqlResult<bool> {
        if !(0..32).contains(&new_position) {
            return Ok(false);
        }
        let changed = self.connection.execute(
            "UPDATE foreground_queue SET position=?1 WHERE session_id=?2 AND id=?3",
            params![new_position, session_id, id],
        )?;
        Ok(changed > 0)
    }

    pub fn clear_foreground(&self, session_id: i64) -> SqlResult<usize> {
        self.connection.execute(
            "DELETE FROM foreground_queue WHERE session_id=?1",
            params![session_id],
        )
    }

    pub fn pop_foreground(&self, session_id: i64) -> SqlResult<Option<String>> {
        let item = self.foreground_queue(session_id)?.into_iter().next();
        if let Some(item) = item {
            self.remove_foreground(session_id, item.id)?;
            Ok(Some(item.prompt))
        } else {
            Ok(None)
        }
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

    pub fn add_pending_memory(
        &self,
        scope: &str,
        kind: &str,
        key: &str,
        content: &str,
        confidence: f64,
        importance: i64,
    ) -> SqlResult<i64> {
        self.connection.execute(
            "INSERT INTO pending_memory(scope, kind, key, content, confidence, importance, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))",
            params![scope, kind, key, content, confidence, importance],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn pending_memory(&self, scope: &str, limit: usize) -> SqlResult<Vec<PendingMemoryRecord>> {
        let mut statement = self.connection.prepare("SELECT id, scope, kind, key, content, confidence, importance FROM pending_memory WHERE scope = ?1 ORDER BY id LIMIT ?2")?;
        let rows = statement.query_map(params![scope, limit.min(1_000) as i64], |row| {
            Ok(PendingMemoryRecord {
                id: row.get(0)?,
                scope: row.get(1)?,
                kind: row.get(2)?,
                key: row.get(3)?,
                content: row.get(4)?,
                confidence: row.get(5)?,
                importance: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn approve_pending_memory(&self, id: i64, scope: &str) -> SqlResult<bool> {
        let transaction = self.connection.unchecked_transaction()?;
        let pending: Option<(String, String, String, String, f64, i64)> = transaction.query_row(
            "SELECT scope, kind, key, content, confidence, importance FROM pending_memory WHERE id = ?1 AND scope = ?2",
            params![id, scope],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let Some((scope, kind, key, content, confidence, importance)) = pending else {
            return Ok(false);
        };
        let normalized: String = key
            .chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let old: Option<i64> = {
            let mut statement = transaction.prepare(
                "SELECT id, key FROM memory WHERE scope = ?1 AND kind = ?2 AND status = 'active' ORDER BY id DESC",
            )?;
            let rows = statement
                .query_map(params![&scope, &kind], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<SqlResult<Vec<_>>>()?;
            rows.into_iter()
                .find(|(_, key)| normalize_memory_key(key) == normalized)
                .map(|(id, _)| id)
        };
        transaction.execute("INSERT INTO memory(scope, kind, key, content, confidence, importance, created_at, updated_at, last_accessed_at, access_count, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'), strftime('%s','now'), strftime('%s','now'), 0, 'active')", params![&scope, &kind, &normalized, &content, confidence, importance])?;
        let new_id = transaction.last_insert_rowid();
        if let Some(old_id) = old {
            transaction.execute("UPDATE memory SET status = 'superseded', superseded_by = ?1, updated_at = strftime('%s','now') WHERE id = ?2", params![new_id, old_id])?;
        }
        transaction.execute(
            "DELETE FROM pending_memory WHERE id = ?1 AND scope = ?2",
            params![id, scope],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn reject_pending_memory(&self, id: i64, scope: &str) -> SqlResult<bool> {
        Ok(self.connection.execute(
            "DELETE FROM pending_memory WHERE id = ?1 AND scope = ?2",
            params![id, scope],
        )? == 1)
    }
}

fn normalize_memory_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
    fn session_summary_and_override_are_workspace_scoped() {
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("inspect", "/workspace")
            .expect("session");
        db.append_turn(session, 0).expect("turn");
        assert!(db
            .set_session_override(
                session,
                "/workspace",
                Some("fake"),
                Some("gpt-5"),
                None,
                Some("main"),
            )
            .expect("override"));
        assert_eq!(
            db.session_override(session, "/workspace").expect("read"),
            Some(SessionOverride {
                provider: Some("fake".into()),
                model: Some("gpt-5".into()),
                effort: None,
            })
        );
        assert!(db
            .session_override(session, "/other")
            .expect("wrong workspace")
            .is_none());
        let summaries = db.list_session_summaries("/workspace").expect("summaries");
        assert_eq!(summaries[0].id, session);
        assert_eq!(summaries[0].turn_count, 1);
        assert_eq!(summaries[0].git_branch.as_deref(), Some("main"));
        let page = db
            .list_session_summaries_page("/workspace", 1, 0)
            .expect("bounded page");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, session);
    }

    #[test]
    fn session_tool_override_is_workspace_scoped_and_persistent() {
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("tools", "/workspace")
            .expect("session");
        let override_ = SessionToolOverride {
            enabled: vec!["shell".into()],
            disabled: vec!["read".into()],
        };
        assert!(db
            .set_session_tool_override(session, "/workspace", &override_)
            .expect("write"));
        assert_eq!(
            db.session_tool_override(session, "/workspace")
                .expect("read"),
            Some(override_)
        );
        assert!(db
            .session_tool_override(session, "/other")
            .expect("wrong scope")
            .is_none());
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

    #[test]
    fn session_title_and_context_boundaries_survive_reopen() {
        let path = std::env::temp_dir().join(format!("helios-storage-c1-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).expect("open");
        let session = db
            .create_session_for_workspace("objective", "/workspace")
            .expect("session");
        let first_turn = db.append_turn(session, 0).expect("turn");
        db.append_message(session, first_turn, "user", "old prompt")
            .expect("old");
        db.append_message(session, first_turn, "assistant", "old answer")
            .expect("old answer");
        assert!(db
            .set_session_title(session, "/workspace", "named")
            .expect("title"));
        assert_eq!(
            db.session(session)
                .expect("session")
                .expect("record")
                .title
                .as_deref(),
            Some("named")
        );
        db.create_context_boundary(session, "clear")
            .expect("boundary");
        let second_turn = db.append_turn(session, 1).expect("turn");
        db.append_message(session, second_turn, "user", "new prompt")
            .expect("new");
        assert_eq!(
            db.session_context_messages(session).expect("context").len(),
            1
        );
        drop(db);
        let reopened = Database::open(&path).expect("reopen");
        assert_eq!(
            reopened
                .session(session)
                .expect("session")
                .expect("record")
                .title
                .as_deref(),
            Some("named")
        );
        assert_eq!(
            reopened.session_context_messages(session).expect("context")[0].1,
            "new prompt"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retry_boundary_keeps_durable_messages_but_hides_last_exchange() {
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("retry", "/workspace")
            .expect("session");
        let turn = db.append_turn(session, 0).expect("turn");
        db.append_message(session, turn, "user", "try this")
            .expect("user");
        db.append_message(session, turn, "assistant", "failed")
            .expect("assistant");
        assert!(db
            .create_boundary_before_last_exchange(session, "retry")
            .expect("boundary"));
        assert!(db.session_messages(session).expect("journal").len() == 2);
        assert!(db
            .session_context_messages(session)
            .expect("context")
            .is_empty());
        assert_eq!(
            db.last_user_message(session).expect("last").as_deref(),
            Some("try this")
        );
    }

    #[test]
    fn compaction_checkpoint_replaces_active_context_without_erasing_journal() {
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("compact", "/workspace")
            .expect("session");
        let turn = db.append_turn(session, 0).expect("turn");
        db.append_message(session, turn, "user", "old")
            .expect("user");
        db.append_message(session, turn, "assistant", "old answer")
            .expect("assistant");
        db.persist_compaction_checkpoint(session, "compact", "checkpoint summary")
            .expect("checkpoint");
        assert_eq!(db.session_messages(session).expect("journal").len(), 3);
        assert_eq!(
            db.session_context_messages(session).expect("context")[0].1,
            "checkpoint summary"
        );
    }

    #[test]
    fn open_creates_xdg_style_parent_without_workspace_files() {
        let root =
            std::env::temp_dir().join(format!("helios-storage-first-use-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let db_path = root.join("data/helios/state.db");
        let db = Database::open(&db_path).expect("first open");
        drop(db);
        assert!(db_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&db_path)
                    .expect("db metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(db_path.parent().expect("parent"))
                    .expect("parent metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pending_memory_roundtrip_is_workspace_scoped() {
        let db = Database::in_memory().expect("db");
        let id = db
            .add_pending_memory("/workspace", "fact", "key", "value", 1.0, 2)
            .expect("pending");
        assert_eq!(db.pending_memory("/workspace", 10).expect("list")[0].id, id);
        assert!(db.pending_memory("/other", 10).expect("other").is_empty());
        assert!(db
            .approve_pending_memory(id, "/workspace")
            .expect("approve"));
        assert!(db
            .pending_memory("/workspace", 10)
            .expect("empty")
            .is_empty());
        assert_eq!(db.memory_count().expect("memory"), 1);
    }

    #[test]
    fn foreground_queue_is_bounded_ordered_and_persistent() {
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("run", "/workspace")
            .expect("session");
        let first = db.enqueue_foreground(session, "first").expect("queue");
        let second = db.enqueue_foreground(session, "second").expect("queue");
        assert_eq!(
            db.foreground_queue(session)
                .expect("list")
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(
            db.pop_foreground(session).expect("pop").as_deref(),
            Some("first")
        );
        assert!(db.edit_foreground(session, second, "edited").expect("edit"));
        assert!(db.move_foreground(session, second, 0).expect("move"));
        assert!(db.remove_foreground(session, second).expect("remove"));
        assert!(db.foreground_queue(session).expect("empty").is_empty());
        assert!(db
            .enqueue_foreground(session, &"x".repeat(32 * 1024 + 1))
            .is_err());
    }

    #[test]
    fn checkpoints_and_lineage_are_workspace_scoped() {
        let db = Database::in_memory().expect("db");
        let parent = db
            .create_session_for_workspace("root", "/workspace")
            .expect("session");
        let turn = db.append_turn(parent, 0).expect("turn");
        db.append_message(parent, turn, "user", "inspect")
            .expect("message");
        let checkpoint = db
            .create_checkpoint(parent, "/workspace", "before-edit", "test")
            .expect("checkpoint");
        let child = db
            .create_lineage_session("branch", "/workspace", parent, "branch", i64::MAX)
            .expect("lineage");
        assert_ne!(parent, child);
        assert_eq!(
            db.checkpoints(parent, "/workspace").expect("list")[0].id,
            checkpoint
        );
        assert!(db
            .checkpoint(checkpoint, parent, "/other")
            .expect("scope")
            .is_none());
    }

    #[test]
    fn checkpoint_files_store_bounded_preimage_and_post_hash() {
        let root =
            std::env::temp_dir().join(format!("helios-checkpoint-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let file = root.join("file.txt");
        fs::write(&file, "before").expect("file");
        let workspace = root
            .canonicalize()
            .expect("canonical")
            .display()
            .to_string();
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("run", &workspace)
            .expect("session");
        let checkpoint = db
            .create_checkpoint(session, &workspace, "auto", "test")
            .expect("checkpoint");
        db.record_checkpoint_file(checkpoint, &workspace, &file, true, b"before")
            .expect("record");
        fs::write(&file, "after").expect("mutate");
        db.update_checkpoint_file_after(checkpoint, &workspace, &file, b"after")
            .expect("post");
        let record = &db.checkpoint_files(checkpoint, &workspace).expect("files")[0];
        assert_eq!(record.content.as_deref(), Some(b"before".as_slice()));
        let expected_hash = hash_bytes(b"after");
        assert_eq!(record.post_hash.as_deref(), Some(expected_hash.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_retention_keeps_pinned_entries() {
        let db = Database::in_memory().expect("db");
        let session = db
            .create_session_for_workspace("run", "/workspace")
            .expect("session");
        let pinned = db
            .create_checkpoint(session, "/workspace", "pinned", "test")
            .expect("checkpoint");
        assert!(db
            .pin_checkpoint(pinned, session, "/workspace", true)
            .expect("pin"));
        for index in 0..70 {
            db.create_checkpoint(session, "/workspace", &format!("cp-{index}"), "test")
                .expect("checkpoint");
        }
        let checkpoints = db.checkpoints(session, "/workspace").expect("list");
        assert!(checkpoints
            .iter()
            .any(|item| item.id == pinned && item.pinned));
        assert!(checkpoints.len() <= 65);
    }
}
