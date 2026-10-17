//! Session, database, audit, usage, context and memory commands plus their storage helpers.

use super::*;

pub(crate) fn default_db_path() -> Result<PathBuf, String> {
    Ok(app_paths()?.db_path)
}

pub(crate) fn session_option(summary: SessionSummary) -> agent_tui::SessionOption {
    agent_tui::SessionOption {
        id: summary.id,
        title: summary.objective,
        turn_count: summary.turn_count,
        last_activity: summary.last_activity,
        model: summary.model,
        git_branch: summary.git_branch,
    }
}

pub(crate) fn add_usage(total: &mut UsageRecord, value: &str) {
    for field in value.split_whitespace() {
        let Some((name, number)) = field.split_once('=') else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        match name {
            "input" => total.input_tokens = total.input_tokens.saturating_add(number),
            "output" => total.output_tokens = total.output_tokens.saturating_add(number),
            "cache_read" => {
                total.cache_read_tokens = total.cache_read_tokens.saturating_add(number)
            }
            "cache_write" => {
                total.cache_write_tokens = total.cache_write_tokens.saturating_add(number)
            }
            "reasoning" => total.reasoning_tokens = total.reasoning_tokens.saturating_add(number),
            "total" => total.total_tokens = total.total_tokens.saturating_add(number),
            _ => {}
        }
    }
}

pub(crate) fn sessions_command(command: SessionCommand, db_path: PathBuf) -> Result<(), String> {
    match command {
        SessionCommand::List => list_sessions(db_path),
        SessionCommand::Show(id) => session_show(db_path, id),
        SessionCommand::Archive(id, archived) => session_archive(db_path, id, archived),
        SessionCommand::Delete(id) => session_delete(db_path, id),
        SessionCommand::Export(id, output) => session_export(db_path, id, output),
        SessionCommand::Import(input) => session_import(db_path, input),
    }
}

pub(crate) fn session_show(db_path: PathBuf, id: i64) -> Result<(), String> {
    let db = open_database(&db_path)?;
    let session = db
        .session(id)
        .map_err(|error| error.to_string())?
        .ok_or("session not found")?;
    if session.workspace != current_workspace()?.display().to_string() {
        return Err("session belongs to another workspace".into());
    }
    let messages = db.session_messages(id).map_err(|error| error.to_string())?;
    println!(
        "session={} archived={} deleted={}",
        id,
        session.archived,
        session.deleted_at.is_some()
    );
    println!("objective={}", redact_export_text(&session.objective));
    for (role, content) in messages {
        println!("{role}\t{}", redact_export_text(&content));
    }
    Ok(())
}

pub(crate) fn session_archive(db_path: PathBuf, id: i64, archived: bool) -> Result<(), String> {
    let workspace = current_workspace()?;
    let db = open_database(&db_path)?;
    if !db
        .archive_session(id, &workspace.display().to_string(), archived)
        .map_err(|error| error.to_string())?
    {
        return Err("session not found or already deleted".into());
    }
    println!("session={} archived={archived}", id);
    Ok(())
}

pub(crate) fn session_delete(db_path: PathBuf, id: i64) -> Result<(), String> {
    let workspace = current_workspace()?;
    let db = open_database(&db_path)?;
    if !db
        .delete_session(id, &workspace.display().to_string())
        .map_err(|error| error.to_string())?
    {
        return Err("session not found or already deleted".into());
    }
    println!("session={id} deleted=true");
    Ok(())
}

pub(crate) fn session_export(db_path: PathBuf, id: i64, output: PathBuf) -> Result<(), String> {
    let db = open_database(&db_path)?;
    let session = db
        .session(id)
        .map_err(|error| error.to_string())?
        .ok_or("session not found")?;
    if session.workspace != current_workspace()?.display().to_string() {
        return Err("session belongs to another workspace".into());
    }
    let messages = db
        .session_messages(id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(role, content)| serde_json::json!({"role": role, "content": redact_export_text(&content)}))
        .collect::<Vec<_>>();
    let document = serde_json::json!({
        "schema_version": 1,
        "workspace": session.workspace,
        "objective": redact_export_text(&session.objective),
        "messages": messages,
    });
    atomic_write(
        &output,
        serde_json::to_vec_pretty(&document)
            .map_err(|error| error.to_string())?
            .as_slice(),
    )
}

pub(crate) fn session_import(db_path: PathBuf, input: PathBuf) -> Result<(), String> {
    let workspace = current_workspace()?;
    let bytes = fs::read(&input).map_err(|error| error.to_string())?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid session export: {error}"))?;
    if document
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || document
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            != Some(&workspace.display().to_string())
    {
        return Err("session export schema or workspace mismatch".into());
    }
    let objective = document
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .ok_or("export objective missing")?;
    let messages = document
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .ok_or("export messages missing")?;
    for message in messages {
        if message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_none()
        {
            return Err("invalid message in session export".into());
        }
    }
    let db = open_database(&db_path)?;
    let session_id = db
        .create_session_for_workspace(objective, &workspace.display().to_string())
        .map_err(|error| error.to_string())?;
    let tx = db
        .connection()
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    for message in messages {
        let role = message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .ok_or("invalid message role")?;
        let content = message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or("invalid message content")?;
        tx.execute("INSERT INTO messages(session_id, role, content, created_at) VALUES (?1, ?2, ?3, strftime('%s','now'))", rusqlite::params![session_id, role, content]).map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())?;
    println!("{session_id}");
    Ok(())
}

pub(crate) fn redact_export_text(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if ["api_key", "authorization", "bearer ", "secret", "token="]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        "[redacted]".into()
    } else {
        value.chars().take(8192).collect()
    }
}

pub(crate) fn list_sessions(db_path: PathBuf) -> Result<(), String> {
    let workspace = current_workspace()?;
    let db = open_database(&db_path)?;
    for session in db
        .list_sessions(&workspace.display().to_string())
        .map_err(|error| error.to_string())?
    {
        println!("{}\t{}", session.id, session.objective);
    }
    Ok(())
}

pub(crate) fn search_sessions(db_path: PathBuf, query: &str) -> Result<(), String> {
    let workspace = workspace_root()?;
    let db = open_database(&db_path)?;
    for (id, role, content) in db
        .search_messages_for_workspace(query, &workspace.display().to_string(), 20)
        .map_err(|error| error.to_string())?
    {
        println!("{id}\t{role}\t{content}");
    }
    Ok(())
}

pub(crate) fn database_command(command: DatabaseCommand) -> Result<(), String> {
    match command {
        DatabaseCommand::Backup { input, output } => {
            if output.exists() {
                return Err("backup output already exists".into());
            }
            let db = open_database(&input)?;
            let output = output.canonicalize().unwrap_or(output);
            db.connection()
                .execute(
                    "VACUUM INTO ?1",
                    rusqlite::params![output.display().to_string()],
                )
                .map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&output, fs::Permissions::from_mode(0o600));
            }
            println!("{}", output.display());
            Ok(())
        }
        DatabaseCommand::Restore { input, workspace } => {
            let workspace = workspace
                .canonicalize()
                .map_err(|error| error.to_string())?;
            if workspace != current_workspace()? {
                return Err("--confirm-workspace must match current workspace".into());
            }
            let source = Database::open(&input)
                .map_err(|error| format!("invalid source database: {error}"))?;
            let integrity: String = source
                .connection()
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if integrity != "ok"
                || source.user_version().map_err(|error| error.to_string())?
                    < agent_storage::STORAGE_FORMAT_VERSION
            {
                return Err("source database failed integrity/schema validation".into());
            }
            let target = app_paths()?.db_path;
            if input.canonicalize().ok() == target.canonicalize().ok() {
                return Err("restore input must differ from active database".into());
            }
            let backup = target.with_extension("db.restore-backup");
            if target.exists() {
                fs::rename(&target, &backup).map_err(|error| error.to_string())?;
            }
            if let Err(error) = fs::copy(&input, &target) {
                if backup.exists() {
                    let _ = fs::rename(&backup, &target);
                }
                return Err(error.to_string());
            }
            let _ = fs::remove_file(backup);
            println!("{}", target.display());
            Ok(())
        }
    }
}

pub(crate) fn audit_command(command: AuditCommand) -> Result<(), String> {
    let (db_path, session, event, limit, output) = match command {
        AuditCommand::List {
            db_path,
            session,
            event,
            limit,
        } => (db_path, session, event, limit, None),
        AuditCommand::Show { db_path, id } => {
            let db = open_database(&db_path)?;
            let record = db
                .audit_records(None, None, 10_000)
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|record| record.id == id)
                .ok_or("audit record not found")?;
            println!(
                "{}",
                serde_json::to_string(&record).map_err(|error| error.to_string())?
            );
            return Ok(());
        }
        AuditCommand::Export {
            db_path,
            output,
            session,
            limit,
        } => (db_path, session, None, limit, Some(output)),
    };
    let db = open_database(&db_path)?;
    let records = db
        .audit_records(session, event.as_deref(), limit)
        .map_err(|error| error.to_string())?;
    let encoded = serde_json::to_vec_pretty(&records).map_err(|error| error.to_string())?;
    if let Some(output) = output {
        atomic_write(&output, &encoded)?;
    } else {
        println!("{}", String::from_utf8_lossy(&encoded));
    }
    Ok(())
}

pub(crate) fn resolve_observability_session(
    database: &Database,
    workspace: &str,
    selector: Option<&str>,
) -> Result<i64, String> {
    let record = match selector {
        Some("latest") | None => database.latest_session(workspace),
        Some(value) => database.session(
            value
                .parse::<i64>()
                .map_err(|_| "--session expects an integer id or latest")?,
        ),
    }
    .map_err(|error| error.to_string())?
    .ok_or("session not found")?;
    if record.workspace != workspace {
        return Err("session belongs to another workspace".into());
    }
    Ok(record.id)
}

pub(crate) fn context_command(
    db_path: PathBuf,
    selector: Option<String>,
    json: bool,
) -> Result<(), String> {
    let workspace = workspace_root()?;
    let database = open_database(&db_path)?;
    let session_id = resolve_observability_session(
        &database,
        &workspace.display().to_string(),
        selector.as_deref(),
    )?;
    let records = database
        .context_for_session(session_id)
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&records).map_err(|error| error.to_string())?
        );
    } else {
        for (index, record) in records.iter().enumerate() {
            println!(
                "turn={} budget={} estimated={} selected={} dropped={} truncated={}{} fingerprint={}",
                index + 1,
                record.budget,
                record.estimated_tokens,
                record.selected_count,
                record.dropped_count,
                record.truncated_count,
                context_warning(record.estimated_tokens, record.budget),
                record.fingerprint
            );
        }
    }
    Ok(())
}

pub(crate) fn context_warning(estimated: u64, budget: u64) -> String {
    if budget == 0 {
        return "".into();
    }
    let percent = estimated
        .saturating_mul(100)
        .checked_div(budget)
        .unwrap_or(100);
    if percent >= 95 {
        " warning=95%".into()
    } else if percent >= 85 {
        " warning=85%".into()
    } else if percent >= 70 {
        " warning=70%".into()
    } else {
        String::new()
    }
}

pub(crate) fn usage_command(
    db_path: PathBuf,
    selector: Option<String>,
    json: bool,
) -> Result<(), String> {
    let workspace = workspace_root()?;
    let database = open_database(&db_path)?;
    let session_id = resolve_observability_session(
        &database,
        &workspace.display().to_string(),
        selector.as_deref(),
    )?;
    let usage = database
        .usage_for_session(session_id)
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&usage_json(&usage)).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "input={} output={} cache_read={} cache_write={} reasoning={} total={}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            usage.reasoning_tokens,
            usage.total_tokens
        );
    }
    Ok(())
}

pub(crate) fn usage_json(usage: &UsageRecord) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "total_tokens": usage.total_tokens,
    })
}

pub(crate) fn memory_command(command: MemoryCommand) -> Result<(), String> {
    let workspace = workspace_root()?.display().to_string();
    match command {
        MemoryCommand::List { db_path, limit } => {
            let store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            for memory in store
                .list(&workspace, limit)
                .map_err(|error| error.to_string())?
            {
                println!(
                    "{}	{}	{}",
                    memory.id, memory.entry.key, memory.entry.content
                );
            }
        }
        MemoryCommand::Search {
            db_path,
            query,
            limit,
        } => {
            let store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            for memory in store
                .search_scoped(&workspace, &query, limit)
                .map_err(|error| error.to_string())?
            {
                println!(
                    "{}	{}	{}",
                    memory.id, memory.entry.key, memory.entry.content
                );
            }
        }
        MemoryCommand::Add {
            db_path,
            key,
            content,
            kind,
            importance,
            pending,
        } => {
            let mut store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            let entry = MemoryEntry {
                scope: workspace,
                kind,
                key,
                content,
                confidence: 1.0,
                importance,
                source_session_id: None,
            };
            let id = if pending {
                store.propose(entry)
            } else {
                store.put(entry)
            }
            .map_err(|error| error.to_string())?;
            println!("{id}");
        }
        MemoryCommand::Supersede { db_path, id } => {
            let mut store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            store
                .supersede_scoped(&workspace, id)
                .map_err(|error| error.to_string())?;
        }
        MemoryCommand::Delete { db_path, id } => {
            let mut store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            store
                .delete_scoped(&workspace, id)
                .map_err(|error| error.to_string())?;
        }
        MemoryCommand::Pending { db_path, limit } => {
            let store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            for memory in store
                .pending(&workspace, limit)
                .map_err(|error| error.to_string())?
            {
                println!(
                    "{}\t{}\t{}",
                    memory.id, memory.entry.key, memory.entry.content
                );
            }
        }
        MemoryCommand::Approve { db_path, id } => {
            let mut store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            if !store
                .approve_pending(&workspace, id)
                .map_err(|error| error.to_string())?
            {
                return Err("pending memory entry not found in this workspace".into());
            }
        }
        MemoryCommand::Reject { db_path, id } => {
            let mut store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            if !store
                .reject_pending(&workspace, id)
                .map_err(|error| error.to_string())?
            {
                return Err("pending memory entry not found in this workspace".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn load_startup_memory(
    db_path: &PathBuf,
    workspace: &str,
    query: &str,
) -> Result<Vec<RetrievedMemory>, String> {
    let store =
        SqliteMemoryStore::open(db_path, 128, 16 * 1024).map_err(|error| error.to_string())?;
    Ok(store
        .search_scoped(workspace, query, 8)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|memory| RetrievedMemory {
            id: memory.id.to_string(),
            key: memory.entry.key,
            content: memory.entry.content,
        })
        .collect())
}

pub(crate) struct StaticMemory(pub(crate) Vec<RetrievedMemory>);

impl MemoryRetriever for StaticMemory {
    fn retrieve(
        &self,
        _scope: &str,
        _query: &str,
        limit: usize,
    ) -> Result<Vec<RetrievedMemory>, String> {
        Ok(self.0.iter().take(limit).cloned().collect())
    }
}

pub(crate) fn open_database(path: &PathBuf) -> Result<Database, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let db = Database::open(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(db)
}
