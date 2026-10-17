//! Interactive TUI session: slash commands, checkpoints, MCP and custom prompts, and TUI settings.

use super::*;

pub(crate) fn interactive_command() -> Result<(), String> {
    interactive_command_with_resume(None)
}

pub(crate) fn interactive_command_with_resume(selector: Option<String>) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive mode requires a TTY; use `helios exec` for headless operation".into(),
        );
    }
    let paths = app_paths()?;
    let settings = if paths.effective_project_config().is_file() {
        load_tui_settings(paths.effective_project_config())?
    } else {
        agent_tui::TuiSettings::default()
    };
    let tui_context = interactive_tui_context(&paths);
    let (sender, receiver) = agent_tui::bounded_event_channel(settings.channel_capacity);
    let approval = ApprovalBroker::new(sender.clone());
    let approval_for_ui = approval.clone();
    let rollback_approval = approval.clone();
    let ui_cancellation = CancellationToken::default();
    let first_turn = Arc::new(AtomicBool::new(true));
    let session_model = Arc::new(Mutex::new(None::<String>));
    let session_provider = Arc::new(Mutex::new(None::<String>));
    let session_effort = Arc::new(Mutex::new(None::<String>));
    let active_session = Arc::new(Mutex::new(None::<i64>));
    let db_path = paths.db_path.clone();
    if let Some(selector) = selector {
        let workspace_id = paths.workspace.display().to_string();
        let db = Database::open(&db_path).map_err(|error| error.to_string())?;
        let summaries = db
            .list_session_summaries(&workspace_id)
            .map_err(|error| error.to_string())?;
        let selected = if selector.eq_ignore_ascii_case("latest") {
            summaries.into_iter().next()
        } else if let Ok(id) = selector.parse::<i64>() {
            summaries.into_iter().find(|summary| summary.id == id)
        } else {
            let matches = summaries
                .into_iter()
                .filter(|summary| {
                    summary
                        .title
                        .as_deref()
                        .unwrap_or(&summary.objective)
                        .eq_ignore_ascii_case(&selector)
                })
                .collect::<Vec<_>>();
            if matches.len() == 1 {
                matches.into_iter().next()
            } else {
                None
            }
        }
        .ok_or("session not found or title is ambiguous in this workspace")?;
        *active_session
            .lock()
            .map_err(|_| "session state unavailable")? = Some(selected.id);
        first_turn.store(false, Ordering::Release);
        *session_provider
            .lock()
            .map_err(|_| "session state unavailable")? = selected.provider;
        *session_model
            .lock()
            .map_err(|_| "session state unavailable")? = selected.model;
        *session_effort
            .lock()
            .map_err(|_| "session state unavailable")? = selected.effort;
    }
    let steering = SteeringChannel::bounded(8, 8 * 1024);
    let controller = ForegroundTaskController::default();
    let project_config_path = paths.effective_project_config().to_path_buf();
    let db_path = paths.db_path;
    let workspace_path = paths.workspace.clone();
    let steering_for_ui = steering.clone();
    let controller_for_ui = controller.clone();
    agent_tui::run_interactive_with_context_and_approval(
        receiver,
        sender.clone(),
        ui_cancellation,
        settings,
        tui_context,
        move |prompt, events, turn_cancellation| {
            let command = prompt.trim().to_ascii_lowercase();
            let mut prompt_for_run = prompt.clone();
            if command == "/exit" || command == "/quit" {
                return agent_tui::SubmitAction::Exit;
            }
            if let Some(objective) = prompt.strip_prefix("/bg ").map(str::trim).filter(|v| !v.is_empty()) {
                match start_background_task(&workspace_path, objective.to_owned()) {
                    Ok(task) => { let _ = events.send(agent_protocol::Event::ModelDelta(format!("background task started: {}", task.id))); }
                    Err(error) => { let _ = events.send(agent_protocol::Event::Error(redact_output(&error))); }
                }
                return agent_tui::SubmitAction::Continue;
            }
            if command == "/tasks" || command == "/agents" {
                return agent_tui::SubmitAction::OpenOverlay(if command == "/tasks" { agent_tui::OverlayKind::Tasks } else { agent_tui::OverlayKind::Agents });
            }
            if command.starts_with("/tasks ") || command.starts_with("/agents ") {
                let args = prompt.split_whitespace().skip(1).map(str::to_owned).collect::<Vec<_>>();
                let parsed = if command.starts_with("/agents ") { parse_agents_command(&args) } else { parse_tasks_command(&args) };
                match parsed.and_then(|command| match command { Command::Tasks(task) => tasks_command(task, None), Command::Agents(task) => tasks_command(task, Some("subagent")), _ => unreachable!() }) {
                    Ok(()) => {}, Err(error) => { let _ = events.send(agent_protocol::Event::Error(redact_output(&error))); }
                }
                return agent_tui::SubmitAction::Continue;
            }
            if command == "/checkpoint" || command == "/rollback" || command == "/rewind" {
                return agent_tui::SubmitAction::OpenOverlay(agent_tui::OverlayKind::Checkpoints);
            }
            if let Some(value) = command.strip_prefix("/rollback-confirm ") {
                let id = value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "rollback checkpoint ID is invalid".to_owned());
                let mode = load_runtime_policy(&project_config_path).map(|value| value.mode).unwrap_or(PermissionMode::ReadOnly);
                let result = id.and_then(|checkpoint| rollback_checkpoint(&db_path, checkpoint, &workspace_path, mode, &rollback_approval, &turn_cancellation));
                let _ = events.send(agent_protocol::Event::ModelDelta(result.unwrap_or_else(|error| format!("rollback error: {error}"))));
                return agent_tui::SubmitAction::Continue;
            }
            if command == "/stop" {
                let _ = controller_for_ui.cancel();
                turn_cancellation.cancel();
                let _ = events.send(agent_protocol::Event::Error("active turn cancellation requested".into()));
                return agent_tui::SubmitAction::Continue;
            }
            if let Some(instruction) = prompt.strip_prefix("/steer ") {
                let instruction = instruction.trim();
                if instruction.is_empty() {
                    let _ = events.send(agent_protocol::Event::Error("/steer requires an instruction".into()));
                } else if controller_for_ui.steer(instruction.chars().take(4096).collect::<String>()).unwrap_or(false) {
                    let _ = events.send(agent_protocol::Event::ModelDelta("steer queued for the next safe iteration boundary".into()));
                } else {
                    let _ = events.send(agent_protocol::Event::Error("steer queue is full or instruction is too large".into()));
                }
                return agent_tui::SubmitAction::Continue;
            }
            if command == "/checkpoint" || command.starts_with("/checkpoint ") {
                let label = prompt.strip_prefix("/checkpoint").unwrap_or("").trim();
                let result = Database::open(&db_path).and_then(|db| {
                    let session = active_session.lock().ok().and_then(|value| *value)
                        .or_else(|| db.latest_session(&workspace_path.display().to_string()).ok().flatten().map(|s| s.id))
                        .ok_or_else(|| rusqlite::Error::InvalidParameterName("no active session".into()))?;
                    if label == "list" {
                        return db.checkpoints(session, &workspace_path.display().to_string()).map(|items| items.into_iter().map(|item| format!("{} {}{}", item.id, item.label, if item.pinned { " [pinned]" } else { "" })).collect::<Vec<_>>().join("\n"));
                    }
                    if let Some(id) = label.strip_prefix("show ").and_then(|value| value.trim().parse::<i64>().ok()) {
                        return db.checkpoint(id, session, &workspace_path.display().to_string()).map(|item| item.map(|value| format!("checkpoint={} label={} reason={} pinned={}", value.id, value.label, value.reason, value.pinned)).unwrap_or_else(|| "checkpoint not found".into()));
                    }
                    if let Some(id) = label.strip_prefix("pin ").and_then(|value| value.trim().parse::<i64>().ok()) {
                        return db.pin_checkpoint(id, session, &workspace_path.display().to_string(), true).map(|changed| if changed { "checkpoint pinned".into() } else { "checkpoint not found".into() });
                    }
                    if let Some(id) = label.strip_prefix("unpin ").and_then(|value| value.trim().parse::<i64>().ok()) {
                        return db.pin_checkpoint(id, session, &workspace_path.display().to_string(), false).map(|changed| if changed { "checkpoint unpinned".into() } else { "checkpoint not found".into() });
                    }
                    db.create_checkpoint(session, &workspace_path.display().to_string(), if label.is_empty() { "manual" } else { label }, "explicit user checkpoint").map(|id| format!("checkpoint={id}"))
                });
                let _ = events.send(agent_protocol::Event::ModelDelta(result.unwrap_or_else(|error| format!("checkpoint error: {error}"))));
                return agent_tui::SubmitAction::Continue;
            }
            if command.starts_with("/branch") || command.starts_with("/fork") {
                let kind = if command.starts_with("/fork") { "fork" } else { "branch" };
                let title = prompt.split_once(' ').map(|(_, value)| value.trim()).filter(|value| !value.is_empty()).unwrap_or(kind);
                let result = Database::open(&db_path).and_then(|db| {
                    let parent = active_session.lock().ok().and_then(|value| *value)
                        .or_else(|| db.latest_session(&workspace_path.display().to_string()).ok().flatten().map(|s| s.id))
                        .ok_or_else(|| rusqlite::Error::InvalidParameterName("no active session".into()))?;
                    let message_id: i64 = db.connection().query_row("SELECT COALESCE(MAX(id),0) FROM messages WHERE session_id=?1", rusqlite::params![parent], |row| row.get(0))?;
                    db.create_lineage_session(title, &workspace_path.display().to_string(), parent, kind, message_id)
                });
                if let Ok(id) = result {
                    if let Ok(mut active) = active_session.lock() { *active = Some(id); }
                    let _ = events.send(agent_protocol::Event::ModelDelta(format!("{kind} session={id}")));
                } else { let _ = events.send(agent_protocol::Event::Error(format!("{kind} failed"))); }
                return agent_tui::SubmitAction::Continue;
            }
            if command.starts_with("/rollback") || command.starts_with("/rewind") {
                let id = prompt.split_whitespace().nth(1).and_then(|value| value.parse::<i64>().ok());
                let filesystem = command.starts_with("/rollback") || prompt.split_whitespace().nth(2).is_some_and(|value| matches!(value, "filesystem" | "all"));
                if filesystem {
                    if let Some(checkpoint) = id {
                        return agent_tui::SubmitAction::OpenCheckpointConfirm(checkpoint);
                    }
                    return agent_tui::SubmitAction::OpenOverlay(agent_tui::OverlayKind::Checkpoints);
                }
                let result = Database::open(&db_path).and_then(|db| {
                    let session = active_session.lock().ok().and_then(|value| *value).ok_or_else(|| rusqlite::Error::InvalidParameterName("no active session".into()))?;
                    let checkpoint = id.and_then(|checkpoint_id| db.checkpoint(checkpoint_id, session, &workspace_path.display().to_string()).ok().flatten()).ok_or_else(|| rusqlite::Error::InvalidParameterName("checkpoint not found".into()))?;
                    db.create_context_boundary_at(session, checkpoint.message_id, "rewind conversation")
                });
                let _ = events.send(agent_protocol::Event::ModelDelta(result.map(|boundary| format!("conversation rewind boundary={boundary}")).unwrap_or_else(|error| format!("rewind error: {error}"))));
                return agent_tui::SubmitAction::Continue;
            }
            if let Some(queue_command) = prompt.strip_prefix("/queue") {
                let action = queue_command.trim();
                let result = Database::open(&db_path).and_then(|db| {
                    let session = active_session.lock().ok().and_then(|value| *value)
                        .or_else(|| db.latest_session(&workspace_path.display().to_string()).ok().flatten().map(|s| s.id));
                    let Some(session) = session else { return Ok("queue unavailable until the first session turn".into()); };
                    if action.is_empty() || action == "list" {
                        let items = db.foreground_queue(session)?;
                        return Ok(if items.is_empty() { "queue empty".into() } else { items.into_iter().map(|item| format!("{}: {}", item.id, redact_output(&item.prompt))).collect::<Vec<_>>().join("\n") });
                    }
                    if action == "clear" { db.clear_foreground(session)?; return Ok("queue cleared".into()); }
                    if let Some(value) = action.strip_prefix("edit ") {
                        let mut parts = value.splitn(2, ' ');
                        let id = parts.next().and_then(|text| text.parse::<i64>().ok()).ok_or_else(|| rusqlite::Error::InvalidParameterName("queue id".into()))?;
                        let text = parts.next().ok_or_else(|| rusqlite::Error::InvalidParameterName("queue prompt".into()))?;
                        return Ok(if db.edit_foreground(session, id, text)? { "queue item edited".into() } else { "queue item not found".into() });
                    }
                    if let Some(value) = action.strip_prefix("move ") {
                        let mut parts = value.split_whitespace();
                        let id = parts.next().and_then(|text| text.parse::<i64>().ok()).ok_or_else(|| rusqlite::Error::InvalidParameterName("queue id".into()))?;
                        let position = parts.next().and_then(|text| text.parse::<i64>().ok()).ok_or_else(|| rusqlite::Error::InvalidParameterName("queue position".into()))?;
                        return Ok(if db.move_foreground(session, id, position)? { "queue item moved".into() } else { "queue item not found".into() });
                    }
                    if let Some(text) = action.strip_prefix("remove ") { let id = text.trim().parse::<i64>().map_err(|_| rusqlite::Error::InvalidParameterName("queue id".into()))?; return Ok(if db.remove_foreground(session,id)? { "queue item removed".into() } else { "queue item not found".into() }); }
                    db.enqueue_foreground(session, action).map(|id| format!("queued {id}"))
                });
                let output = result.map_err(|error| error.to_string()).unwrap_or_else(|error| error);
                let _ = events.send(agent_protocol::Event::ModelDelta(output));
                return agent_tui::SubmitAction::Continue;
            }
            if let Some(side_prompt) = prompt.strip_prefix("/btw ") {
                let side_prompt = side_prompt.trim();
                if side_prompt.is_empty() {
                    let _ = events.send(agent_protocol::Event::Error("usage: /btw QUESTION".into()));
                    return agent_tui::SubmitAction::Continue;
                }
                let side_events = events.clone();
                let side_db = db_path.clone();
                let side_config = project_config_path.clone();
                let side_prompt = side_prompt.to_owned();
                std::thread::spawn(move || {
                    let (side_sender, side_receiver) = std::sync::mpsc::sync_channel(256);
                    let mut options = default_run_options(side_prompt, side_db, None, Some(side_sender));
                    options.config = Some(side_config);
                    options.exec = true;
                    let result = run_command_with_cancellation(options, CancellationToken::default());
                    let mut response = String::new();
                    for event in side_receiver.try_iter() {
                        if let agent_protocol::Event::ModelDelta(text) = event { response.push_str(&text); }
                    }
                    let summary = match result {
                        Ok(()) if !response.is_empty() => format!("BTW (isolated): {response}"),
                        Ok(()) => "BTW (isolated): completed without text".into(),
                        Err(error) => format!("BTW (isolated) error: {}", redact_output(&error)),
                    };
                    let _ = side_events.send(agent_protocol::Event::ModelDelta(summary));
                });
                return agent_tui::SubmitAction::Continue;
            }
            if command.starts_with('!') {
                let input = prompt.trim().to_owned();
                let worker_events = events.clone();
                let worker_workspace = workspace_path.clone();
                let worker_config = project_config_path.clone();
                let worker_approval = approval.clone();
                let worker_cancellation = turn_cancellation.clone();
                let worker_session = active_session.lock().ok().and_then(|value| *value);
                std::thread::spawn(move || {
                    let argv = match parse_direct_argv(&input) {
                        Ok(argv) => argv,
                        Err(error) => {
                            let _ = worker_events.send(agent_protocol::Event::Error(error));
                            return;
                        }
                    };
                    let policy = app_paths()
                        .ok()
                        .and_then(|paths| {
                            load_runtime_policy(paths.effective_project_config()).ok()
                        })
                        .unwrap_or_default();
                    let registry = match ToolRegistry::new(&worker_workspace, policy.mode) {
                        Ok(registry) => registry
                            .with_disabled_tools(
                                effective_disabled_tool_names_for_session(&worker_config, worker_session, None).unwrap_or_default(),
                            )
                            .with_cancellation(worker_cancellation.clone())
                            .with_approval_broker(worker_approval),
                        Err(error) => {
                            let _ =
                                worker_events.send(agent_protocol::Event::Error(error.to_string()));
                            return;
                        }
                    };
                    let name = argv.first().cloned().unwrap_or_default();
                    let _ = worker_events.send(agent_protocol::Event::ToolStarted(name.clone()));
                    let args = serde_json::json!({
                        "program": name,
                        "args": argv.iter().skip(1).cloned().collect::<Vec<_>>(),
                        "cwd": worker_workspace.display().to_string(),
                        "requires_pty": false,
                    });
                    match registry.execute("shell", args, &worker_cancellation) {
                        Ok(result) => {
                            let _ = worker_events
                                .send(agent_protocol::Event::ToolOutputDelta(result.data));
                            let _ = worker_events.send(agent_protocol::Event::ToolCompleted(
                                format!("{:?}", result.status),
                            ));
                        }
                        Err(error) => {
                            let _ = worker_events.send(agent_protocol::Event::Error(
                                redact_output(&error.to_string()),
                            ));
                            let _ = worker_events
                                .send(agent_protocol::Event::ToolCompleted("error".into()));
                        }
                    }
                });
                            return agent_tui::SubmitAction::Continue;
            }
            let local_output = if command == "/new" || command.starts_with("/new ") {
                let title = prompt.trim().strip_prefix("/new").unwrap_or("").trim();
                let title = if title.is_empty() {
                    "New session"
                } else {
                    title
                };
                match Database::open(&db_path).and_then(|db| {
                    db.create_session_for_workspace(title, &workspace_path.display().to_string())
                }) {
                    Ok(id) => {
                        if let Ok(mut active) = active_session.lock() {
                            *active = Some(id);
                        }
                        first_turn.store(false, Ordering::Release);
                        let _ = events.send(agent_protocol::Event::SessionReset(format!(
                            "new session #{id}"
                        )));
                        Some(format!("new session #{id}: {}", redact_output(title)))
                    }
                    Err(error) => Some(format!("new session failed: {error}")),
                }
            } else if command == "/clear" {
                let id = active_session
                    .lock()
                    .ok()
                    .and_then(|value| *value)
                    .or_else(|| {
                        Database::open(&db_path).ok().and_then(|db| {
                            db.latest_session(&workspace_path.display().to_string())
                                .ok()
                                .flatten()
                                .map(|s| s.id)
                        })
                    });
                match id.map(|id| {
                    Database::open(&db_path).and_then(|db| db.create_context_boundary(id, "clear"))
                }) {
                    Some(Ok(_)) => {
                        let _ = events.send(agent_protocol::Event::SessionReset(
                            "conversation context cleared; durable history retained".into(),
                        ));
                        Some("conversation context cleared (history retained)".into())
                    }
                    Some(Err(error)) => Some(format!("clear failed: {error}")),
                    None => Some("no active session".into()),
                }
            } else if command == "/title" || command.starts_with("/title ") {
                let title = prompt.trim().strip_prefix("/title").unwrap_or("").trim();
                if title.is_empty() {
                    Some("title requires text".into())
                } else {
                    let result = active_session
                        .lock()
                        .ok()
                        .and_then(|value| *value)
                        .or_else(|| {
                            Database::open(&db_path).ok().and_then(|db| {
                                db.latest_session(&workspace_path.display().to_string())
                                    .ok()
                                    .flatten()
                                    .map(|s| s.id)
                            })
                        })
                        .ok_or_else(|| "no active session".to_string())
                        .and_then(|id| {
                            Database::open(&db_path)
                                .map_err(|e| e.to_string())
                                .and_then(|db| {
                                    db.set_session_title(
                                        id,
                                        &workspace_path.display().to_string(),
                                        title,
                                    )
                                    .map_err(|e| e.to_string())
                                    .and_then(|changed| {
                                        if changed {
                                            Ok(())
                                        } else {
                                            Err("session not found in this workspace".into())
                                        }
                                    })
                                })
                        });
                    Some(
                        result
                            .map(|_| format!("title={}", redact_output(title)))
                            .unwrap_or_else(|error| format!("title failed: {error}")),
                    )
                }
            } else if command == "/resume"
                || command.starts_with("/resume ")
                || command == "/continue"
                || command.starts_with("/session-resume ")
            {
                let selector = prompt
                    .trim()
                    .split_once(' ')
                    .map(|(_, value)| value.trim())
                    .unwrap_or("");
                let selected = Database::open(&db_path).ok().and_then(|db| {
                    let workspace = workspace_path.display().to_string();
                    let sessions = db.list_session_summaries(&workspace).ok()?;
                    if selector.is_empty() || selector.eq_ignore_ascii_case("latest") {
                        return sessions.into_iter().next();
                    }
                    if let Ok(id) = selector.parse::<i64>() {
                        return sessions.into_iter().find(|s| s.id == id);
                    }
                    let matches = sessions
                        .into_iter()
                        .filter(|s| {
                            s.title
                                .as_deref()
                                .unwrap_or(&s.objective)
                                .eq_ignore_ascii_case(selector)
                        })
                        .collect::<Vec<_>>();
                    if matches.len() == 1 {
                        matches.into_iter().next()
                    } else {
                        None
                    }
                });
                match selected {
                    Some(summary) => {
                        if let Ok(mut active) = active_session.lock() {
                            *active = Some(summary.id);
                        }
                        first_turn.store(false, Ordering::Release);
                        if let Ok(mut provider) = session_provider.lock() {
                            *provider = summary.provider.clone();
                        }
                        if let Ok(mut model) = session_model.lock() {
                            *model = summary.model.clone();
                        }
                        if let Ok(mut effort) = session_effort.lock() {
                            *effort = summary.effort.clone();
                        }
                        let _ = events.send(agent_protocol::Event::SessionReset(format!(
                            "resumed session #{}",
                            summary.id
                        )));
                        Some(format!(
                            "resumed session #{}: {}",
                            summary.id,
                            redact_output(summary.title.as_deref().unwrap_or(&summary.objective))
                        ))
                    }
                    None => {
                        Some("session not found or title is ambiguous in this workspace".into())
                    }
                }
            } else if command == "/history" || command.starts_with("/history ") {
                let query = prompt
                    .trim()
                    .strip_prefix("/history")
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                let history = active_session
                    .lock()
                    .ok()
                    .and_then(|value| *value)
                    .or_else(|| {
                        Database::open(&db_path).ok().and_then(|db| {
                            db.latest_session(&workspace_path.display().to_string())
                                .ok()
                                .flatten()
                                .map(|s| s.id)
                        })
                    })
                    .and_then(|id| {
                        Database::open(&db_path)
                            .ok()
                            .and_then(|db| db.session_context_messages(id).ok())
                    })
                    .unwrap_or_default();
                let lines = history
                    .into_iter()
                    .filter(|(_, content)| {
                        query.is_empty() || content.to_ascii_lowercase().contains(&query)
                    })
                    .rev()
                    .take(100)
                    .map(|(role, content)| format!("{role}: {}", redact_output(&content)))
                    .collect::<Vec<_>>();
                Some(if lines.is_empty() {
                    "history=0".into()
                } else {
                    format!(
                        "history={}\n{}",
                        lines.len(),
                        lines.into_iter().rev().collect::<Vec<_>>().join("\n")
                    )
                })
            } else if command == "/undo" || command == "/retry" {
                let id = active_session
                    .lock()
                    .ok()
                    .and_then(|value| *value)
                    .or_else(|| {
                        Database::open(&db_path).ok().and_then(|db| {
                            db.latest_session(&workspace_path.display().to_string())
                                .ok()
                                .flatten()
                                .map(|s| s.id)
                        })
                    });
                let result = Database::open(&db_path).ok().zip(id).map(|(db, id)| {
                        let previous = db.last_user_message(id).ok().flatten();
                        let boundary = db.create_boundary_before_last_exchange(
                            id,
                            if command == "/retry" { "retry" } else { "undo" },
                        );
                        (previous, boundary)
                    });
                match result {
                    Some((previous, Ok(true))) if command == "/retry" => {
                        prompt_for_run = previous.unwrap_or_default();
                        Some(String::new())
                    }
                    Some((_, Ok(true))) => {
                        let _ = events.send(agent_protocol::Event::SessionReset(
                            "last exchange hidden from active context; durable history retained"
                                .into(),
                        ));
                        Some("undid last exchange (filesystem unchanged)".into())
                    }
                    Some((_, Ok(false))) => Some("no exchange to undo".into()),
                    Some((_, Err(error))) => Some(format!("conversation boundary failed: {error}")),
                    None => Some("no active session".into()),
                }
            } else if command == "/compact" || command.starts_with("/compact ") {
                let focus = prompt.trim().strip_prefix("/compact").unwrap_or("").trim();
                let objective = active_session
                    .lock()
                    .ok()
                    .and_then(|value| *value)
                    .and_then(|id| {
                        Database::open(&db_path)
                            .ok()
                            .and_then(|db| db.session(id).ok().flatten().map(|s| s.objective))
                    })
                    .unwrap_or_else(|| "interactive session".into());
                let accepted_constraints = if focus.is_empty() {
                    Vec::new()
                } else {
                    vec![focus.into()]
                };
                let request = CompactionRequest {
                    objective,
                    accepted_constraints,
                    decisions: Vec::new(),
                    unresolved_failures: Vec::new(),
                    verification_results: Vec::new(),
                    relevant_memory_ids: Vec::new(),
                };
                let mut compactor = LocalCompactor;
                match compactor.compact(request) {
                    Ok(result) => {
                        if let Some(id) = active_session.lock().ok().and_then(|value| *value) {
                            if let Ok(db) = Database::open(&db_path) {
                                let _ = db.persist_compaction_checkpoint(
                                    id,
                                    "compact",
                                    &result.summary,
                                );
                            }
                        }
                        let _ = events.send(agent_protocol::Event::ContextCompacted(
                            result.summary.clone(),
                        ));
                        Some(format!("compacted: {}", redact_output(&result.summary)))
                    }
                    Err(error) => Some(format!("compaction failed safely: {error}")),
                }
            } else if let Some(selector) = command.strip_prefix("/session-resume ") {
                match selector.parse::<i64>() {
                    Ok(id) => match Database::open(&db_path)
                        .ok()
                        .and_then(|db| db.session(id).ok().flatten())
                    {
                        Some(record)
                            if record.workspace == workspace_path.display().to_string() =>
                        {
                            if let Ok(mut active) = active_session.lock() {
                                *active = Some(id);
                            }
                            if let Ok(mut effort) = session_effort.lock() {
                                *effort = Database::open(&db_path)
                                    .ok()
                                    .and_then(|db| {
                                        db.session_override(
                                            id,
                                            &workspace_path.display().to_string(),
                                        )
                                        .ok()
                                        .flatten()
                                    })
                                    .and_then(|value| value.effort);
                            }
                            first_turn.store(false, Ordering::Release);
                            Some(format!("resumed session #{}: {}", id, record.objective))
                        }
                        _ => Some("session not found in this workspace".into()),
                    },
                    Err(_) => Some("session resume requires an exact numeric id".into()),
                }
            } else if let Some(model) = command.strip_prefix("/model ") {
                if model.is_empty()
                    || !model.chars().all(|character| {
                        character.is_ascii_alphanumeric() || "._:-/".contains(character)
                    })
                {
                    Some("model name must be a safe provider/model identifier".into())
                } else {
                    let (provider, model_name) = model
                        .split_once('/')
                        .map(|(provider, model)| (Some(provider), model))
                        .unwrap_or((None, model));
                    if let Ok(mut selected) = session_model.lock() {
                        *selected = Some(model_name.to_owned());
                    }
                    if let Some(provider) = provider {
                        if let Ok(mut selected) = session_provider.lock() {
                            *selected = Some(provider.to_owned());
                        }
                    }
                    if let Ok(active) = active_session.lock() {
                        if let Some(session_id) = *active {
                            let _ = Database::open(&db_path).and_then(|db| {
                                db.set_session_override(
                                    session_id,
                                    &workspace_path.display().to_string(),
                                    provider,
                                    Some(model_name),
                                    None,
                                    None,
                                )
                            });
                        }
                    }
                    Some(format!("session model={model}"))
                }
            } else if let Some(effort) = command.strip_prefix("/effort ") {
                match validate_effort(effort) {
                    Ok(()) => {
                        if let Ok(mut selected) = session_effort.lock() {
                            *selected = Some(effort.to_owned());
                        }
                        if let Ok(active) = active_session.lock() {
                            if let Some(session_id) = *active {
                                let _ = Database::open(&db_path).and_then(|db| {
                                    db.set_session_override(
                                        session_id,
                                        &workspace_path.display().to_string(),
                                        None,
                                        None,
                                        Some(effort),
                                        None,
                                    )
                                });
                            }
                        }
                        Some(format!("effort={effort}"))
                    }
                    Err(error) => Some(error),
                }
            } else if let Some(theme) = command.strip_prefix("/theme ") {
                if matches!(theme, "dark" | "light" | "system" | "monochrome") {
                    let result = tui_config_command(
                        &["set".into(), "--theme".into(), theme.into()],
                        &project_config_path,
                    );
                    Some(
                        result
                            .map(|_| format!("theme={theme}"))
                            .unwrap_or_else(|error| error),
                    )
                } else {
                    Some("theme must be dark, light, system, or monochrome".into())
                }
            } else if command == "/plan" {
                let result = runtime_permissions_command(
                    &["set-mode".into(), "read-only".into()],
                    &project_config_path,
                );
                Some(result.map(|_| {
                    let _ = events.send(agent_protocol::Event::PermissionModeChanged("ReadOnly".into()));
                    "plan mode enabled: reads/search/review only".into()
                }).unwrap_or_else(|error| error))
            } else if command == "/permissions workspace-write" {
                let result = runtime_permissions_command(
                    &["set-mode".into(), "workspace-write".into()],
                    &project_config_path,
                );
                Some(result.map(|_| {
                    let _ = events.send(agent_protocol::Event::PermissionModeChanged("WorkspaceWrite".into()));
                    "permission mode=workspace-write".into()
                }).unwrap_or_else(|error| error))
            } else if command == "/review" || command.starts_with("/review ") {
                let focus = command.strip_prefix("/review ").unwrap_or("general");
                let variant = "worktree";
                ToolRegistry::new(&workspace_path, PermissionMode::ReadOnly)
                    .ok()
                    .and_then(|registry| registry.git_diff_variant(variant).ok())
                    .map(|result| format!("review focus={focus} (read-only)\n{}", result.data))
                    .or_else(|| Some("review unavailable: no bounded git diff".into()))
            } else if command == "/config" || command == "/settings" {
                Some(format!("config={} (local; use config/settings commands for changes)", project_config_path.display()))
            } else if command == "/keybindings" {
                Some("keybindings: Enter submit, Alt+Enter newline, Esc close/deny, Ctrl+C cancel, Ctrl+D exit, PageUp/PageDown scroll".into())
            } else if command == "/verbose" || command == "/focus" {
                Some(format!("{} mode enabled for current view", command.trim_start_matches('/')))
            } else if command == "/changelog" {
                Some("Helios local changelog: C3 plan/review/productivity controls are enabled; provider-free commands do not consume tokens.".into())
            } else if let Some(output) = command.strip_prefix("/export ") {
                match resolve_workspace_file(Path::new(output.trim()), &workspace_path) {
                    Err(error) => Some(error),
                    Ok(destination) if destination.exists() => Some("export refuses overwrite; choose a new path".into()),
                    Ok(destination) => active_session.lock().ok().and_then(|value| *value)
                        .map(|id| session_export(db_path.clone(), id, destination).map(|_| "session exported".into()).unwrap_or_else(|error| error))
                        .or_else(|| Some("no active session to export".into())),
                }
            } else if matches!(command.as_str(), "/editor" | "/compose" | "/prompt") {
                match edit_external_prompt("", &workspace_path) {
                    Ok(text) => return agent_tui::SubmitAction::ReplaceComposer(text),
                    Err(error) => Some(error),
                }
            } else if let Some(copy_target) = command.strip_prefix("/copy ") {
                Some(copy_last_text(copy_target, &workspace_path))
            } else if let Some(custom_output) = custom_exec_command(
                &command,
                &workspace_path,
                &project_config_path,
                approval.clone(),
                turn_cancellation.clone(),
            ) {
                Some(custom_output.unwrap_or_else(|error| error))
            } else if let Some(action) = command.strip_prefix("/tools ") {
                let mut parts = action.split_whitespace();
                match parts.next() {
                    Some("enable") | Some("disable") => {
                        let mode = action.split_whitespace().next().unwrap_or_default();
                        let name = parts.next().ok_or_else(|| "tool name required".to_string());
                        let scope = action.split_whitespace().skip(2).collect::<Vec<_>>().windows(2).find(|pair| pair[0] == "--scope").map(|pair| pair[1]);
                        let mut args: Vec<String> = vec![mode.into(), "--id".into()];
                        if let Ok(name) = name.clone() { args.push(name.to_string()); }
                        if scope == Some("session") {
                            args.extend(["--scope".into(), "session".into()]);
                            if let Some(id) = active_session.lock().ok().and_then(|value| *value) { args.extend(["--session".into(), id.to_string()]); }
                        }
                        Some(name.and_then(|name| runtime_command(RuntimeCommand::Tools(args)).map(|_| format!("tool={name} enabled={}", mode == "enable"))).unwrap_or_else(|error| error))
                    }
                    Some("info") => {
                        let name = parts.next().unwrap_or_default();
                        let policy = load_runtime_policy(&project_config_path).unwrap_or_default();
                        ToolRegistry::new(&workspace_path, policy.mode).ok().and_then(|registry| registry.metadata(name)).map(|metadata| format!("{} permission={:?} schema_cost=estimated:{} budget={} timeout_ms={}", metadata.id, metadata.permission_class, metadata.context_cost_hint, metadata.output_budget, metadata.timeout.as_millis())).or_else(|| Some("unknown tool".into()))
                    }
                    _ => Some("tools action requires enable, disable, or info".into()),
                }
            } else if let Some(action) = command.strip_prefix("/memory ") {
                let mut parts = action.split_whitespace();
                match parts.next() {
                    Some("show") => {
                        let id = parts.next().and_then(|value| value.parse::<i64>().ok());
                        id.and_then(|id| SqliteMemoryStore::open(&db_path, 128, 16 * 1024).ok().and_then(|store| store.list(&workspace_path.display().to_string(), 128).ok()).and_then(|entries| entries.into_iter().find(|entry| entry.id == id))).map(|entry| format!("memory #{} [{}] {} confidence={} importance={}", entry.id, entry.entry.kind, redact_output(&entry.entry.content), entry.entry.confidence, entry.entry.importance)).or_else(|| Some("memory entry not found".into()))
                    }
                    Some("pending") => slash_command_output("/memory"),
                    _ => Some("memory action requires show or pending".into()),
                }
            } else if let Some(name) = command.strip_prefix("/skill ") {
                Some(skill_body(&workspace_path, name.trim()).unwrap_or_else(|error| error))
            } else if command == "/mcp" || command.starts_with("/mcp ") {
                Some(mcp_interactive_command(&command, &workspace_path, &project_config_path))
            } else if let Some(custom_prompt) = custom_prompt_command(&command, &project_config_path) {
                prompt_for_run = custom_prompt;
                None
            } else if prompt.trim_start().starts_with('@') {
                Some(path_target_output(prompt.trim()))
            } else {
                slash_command_output(&command)
            };
            if let Some(output) = local_output {
                if command == "/retry" && prompt_for_run.is_empty() {
                    return agent_tui::SubmitAction::Continue;
                }
                if command == "/retry" {
                    // Continue into the normal runtime with the restored user prompt.
                } else {
                    let _ = events.send(agent_protocol::Event::TurnStarted);
                    let _ = events.send(agent_protocol::Event::ModelDelta(output));
                    let _ = events.send(agent_protocol::Event::TurnCompleted);
                    return agent_tui::SubmitAction::Continue;
                }
            }
            let resume = if let Some(id) = active_session.lock().ok().and_then(|value| *value) {
                Some(id.to_string())
            } else if first_turn.swap(false, Ordering::AcqRel) {
                None
            } else {
                Some("latest".into())
            };
            let options = default_run_options(
                prompt_for_run,
                db_path.clone(),
                resume,
                Some(events.clone()),
            );
            let mut options = options;
            options.approval = Some(approval.clone());
            options.steering = Some(steering_for_ui.clone());
            if let Ok(selected) = session_provider.lock() {
                if let Some(provider) = selected.clone() {
                    options.provider = provider;
                    options.provider_explicit = true;
                }
            }
            if let Ok(selected) = session_model.lock() {
                if let Some(model) = selected.clone() {
                    options.model = model;
                    options.model_explicit = true;
                }
            }
            if let Ok(selected) = session_effort.lock() {
                if let Some(effort) = selected.clone() {
                    options.effort = Some(effort);
                }
            }
            let options = match resolve_runtime_options(options) {
                Ok((options, _policy, _workspace)) => options,
                Err(error) => {
                    let _ = events.send(agent_protocol::Event::Error(redact_output(&error)));
                    return agent_tui::SubmitAction::Continue;
                }
            };
            let task_turn_id = output_timestamp();
            let task_session_id = active_session.lock().ok().and_then(|value| *value).unwrap_or(0);
            if controller_for_ui.start(task_turn_id, task_session_id).is_err() {
                let _ = events.send(agent_protocol::Event::Error("foreground task is already active".into()));
                return agent_tui::SubmitAction::Continue;
            }
            let worker_db_path = db_path.clone();
            let worker_workspace = workspace_path.clone();
            let worker_active_session = active_session.clone();
            let worker_approval = approval.clone();
            let worker_steering = steering_for_ui.clone();
            let worker_controller = controller_for_ui.clone();
            std::thread::spawn(move || {
                let run_cancel = turn_cancellation.clone();
                let result = run_command_with_cancellation(options, run_cancel);
                let cancelled = turn_cancellation.is_cancelled();
                if let Err(error) = result {
                    let _ = events.send(agent_protocol::Event::Error(redact_output(&error)));
                    let _ = worker_controller.fail(task_turn_id);
                } else {
                    let _ = worker_controller.complete(task_turn_id);
                }
                let Ok(db) = Database::open(&worker_db_path) else { return; };
                let Ok(Some(record)) = db.latest_session(&worker_workspace.display().to_string()) else { return; };
                if let Ok(mut active) = worker_active_session.lock() { *active = Some(record.id); }
                if cancelled { return; }
                while let Ok(Some(next_prompt)) = db.pop_foreground(record.id) {
                    let mut next = default_run_options(next_prompt, worker_db_path.clone(), Some(record.id.to_string()), Some(events.clone()));
                    next.approval = Some(worker_approval.clone());
                    next.steering = Some(worker_steering.clone());
                    let Ok((next, _, _)) = resolve_runtime_options(next) else { break; };
                    let next_cancel = CancellationToken::default();
                    if let Err(error) = run_command_with_cancellation(next, next_cancel) {
                        let _ = events.send(agent_protocol::Event::Error(redact_output(&error)));
                    }
                }
            });
            agent_tui::SubmitAction::Continue
        },
        move |id, decision| {
            let mapped = match decision {
                agent_tui::PermissionDecision::AllowOnce => ApprovalDecision::AllowOnce,
                agent_tui::PermissionDecision::AllowSession => ApprovalDecision::AllowSession,
                agent_tui::PermissionDecision::Deny => ApprovalDecision::Deny,
            };
            let _ = approval_for_ui.decide(id, mapped);
        },
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn checkpoint_hash(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

pub(crate) fn rollback_checkpoint(
    db_path: &Path,
    checkpoint_id: i64,
    workspace: &Path,
    mode: PermissionMode,
    approval: &ApprovalBroker,
    cancellation: &CancellationToken,
) -> Result<String, String> {
    let engine = PermissionEngine::new(workspace, mode).map_err(|error| error.to_string())?;
    engine
        .authorize(PermissionClass::WorkspaceWrite)
        .map_err(|error| error.to_string())?;
    let workspace = workspace
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let db = Database::open(db_path).map_err(|error| error.to_string())?;
    let workspace_id = workspace.display().to_string();
    let records = db
        .checkpoint_files(checkpoint_id, &workspace_id)
        .map_err(|error| error.to_string())?;
    if records.is_empty() {
        return Err("checkpoint has no filesystem snapshots".into());
    }
    let mut prepared = Vec::new();
    for record in &records {
        let relative = Path::new(&record.relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err("checkpoint path escapes workspace".into());
        }
        let path = workspace.join(relative);
        if path.exists() {
            let canonical = path.canonicalize().map_err(|error| error.to_string())?;
            if !canonical.starts_with(&workspace) {
                return Err("checkpoint symlink escapes workspace".into());
            }
        }
        let current = path
            .exists()
            .then(|| fs::read(&path))
            .transpose()
            .map_err(|error| error.to_string())?;
        let current_hash = current.as_deref().map(checkpoint_hash);
        if current_hash.as_deref() == Some(record.pre_hash.as_str()) {
            continue;
        }
        if record.post_hash.as_deref() != current_hash.as_deref() {
            return Err(format!("rollback conflict: {}", record.relative_path));
        }
        if record.existed && !record.available {
            return Err(format!(
                "snapshot unavailable (over cap): {}",
                record.relative_path
            ));
        }
        prepared.push((record, path, current));
    }
    if !approval
        .request_approval(
            PermissionClass::WorkspaceWrite,
            format!(
                "rollback checkpoint {checkpoint_id} ({} files)",
                prepared.len()
            ),
            cancellation,
        )
        .map_err(|error| error.to_string())?
    {
        return Err("rollback denied or timed out".into());
    }
    let mut backups = Vec::new();
    for (_, path, current) in &prepared {
        backups.push((path.clone(), current.clone()));
    }
    for (record, path, _) in &prepared {
        let result = if record.existed {
            let content = record
                .content
                .as_deref()
                .ok_or_else(|| "snapshot content unavailable".to_owned())?;
            let parent = path.parent().ok_or("invalid checkpoint path")?;
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let temp = parent.join(format!(
                ".helios-rollback-{}-{}",
                std::process::id(),
                record.id
            ));
            fs::write(&temp, content)
                .and_then(|_| fs::rename(&temp, path))
                .map_err(|error| error.to_string())
        } else if path.exists() {
            fs::remove_file(path).map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        if let Err(error) = result {
            for (backup_path, backup) in &backups {
                match backup {
                    Some(bytes) => {
                        let _ = fs::write(backup_path, bytes);
                    }
                    None => {
                        let _ = fs::remove_file(backup_path);
                    }
                }
            }
            return Err(format!(
                "rollback aborted and restored prior state: {error}"
            ));
        }
        if let Some(mode) = record.mode {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(format!("rollback restored {} file(s)", prepared.len()))
}

pub(crate) fn interactive_tui_context(paths: &AppPaths) -> agent_tui::TuiContext {
    let config_path = paths.effective_project_config();
    let config = load_user_config(config_path).ok();
    let policy = load_runtime_policy(config_path).ok();
    let (branch, dirty_files) = git_state_snapshot(&paths.workspace);
    let mut tasks = Vec::new();
    let mut agents = Vec::new();
    if let Ok(entries) = fs::read_dir(&paths.jobs_dir) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".task.json") {
                if let Ok(mut task) = read_task(&paths.workspace, id) {
                    if task.workspace != paths.workspace.display().to_string() {
                        continue;
                    }
                    if let Ok(job) = read_job(&paths.workspace, &task.job_id) {
                        task.state = task_state_from_job(&job).into();
                    }
                    let option = agent_tui::TaskOption {
                        id: task.id,
                        state: task.state,
                        objective: task.objective,
                        result: task.result,
                    };
                    if task.kind == "subagent" {
                        agents.push(option.clone());
                    }
                    tasks.push(option);
                }
            }
        }
    }
    let mut palette = [
        ("help", "show categorized local help"),
        ("commands", "open the local command palette"),
        ("status", "show the current local status"),
        ("context", "inspect current context estimates"),
        ("permissions", "show effective permissions"),
        ("sessions", "browse workspace sessions"),
        ("diff", "show bounded read-only git diff"),
        ("model", "show the configured model"),
        ("effort", "show provider effort support"),
        ("usage", "show normalized provider usage"),
        ("cost", "show configured cost or unavailable"),
        ("tools", "inspect enabled tool catalog"),
        ("memory", "inspect bounded project memory"),
        ("version", "show Helios version"),
        ("new", "start a new workspace session"),
        ("clear", "clear active context and retain durable history"),
        ("resume", "resume a workspace session"),
        ("continue", "continue the latest workspace session"),
        ("history", "browse bounded active conversation history"),
        ("title", "set the active session title"),
        ("retry", "retry the last user turn"),
        ("undo", "hide the last exchange from active context"),
        ("compact", "compact active context with the local compactor"),
        ("queue", "queue prompts for the active foreground turn"),
        ("steer", "steer the active turn at a safe boundary"),
        ("stop", "cancel the active turn and remain in the session"),
        ("btw", "run an isolated read-only side question"),
        ("checkpoint", "create a durable conversation checkpoint"),
        ("branch", "create a new session lineage branch"),
        ("fork", "fork a new session from the current lineage"),
        ("rewind", "restore conversation context to a checkpoint"),
        (
            "rollback",
            "restore checkpoint files after explicit approval",
        ),
        ("exit", "close the interactive session"),
        ("quit", "close the interactive session"),
    ]
    .into_iter()
    .map(|(name, description)| agent_tui::PaletteCommand {
        name: name.into(),
        description: description.into(),
        category: "Session".into(),
        arguments: String::new(),
        local: true,
    })
    .collect::<Vec<_>>();
    for definition in command_registry() {
        if palette.iter().any(|item| item.name == definition.name) {
            continue;
        }
        palette.push(agent_tui::PaletteCommand {
            name: definition.name.to_owned(),
            description: definition.description.to_owned(),
            category: help_category(definition.category).into(),
            arguments: definition.arguments.to_owned(),
            local: definition.local,
        });
    }
    let mut models = Vec::new();
    if let Some(config) = &config {
        if let (Some(provider), Some(model)) = (&config.provider, &config.model) {
            let capabilities = provider_capability_summary(provider);
            models.push(agent_tui::ModelOption {
                provider: provider.clone(),
                model: model.clone(),
                configured: true,
                current: true,
                capabilities,
            });
        }
    }
    if !models
        .iter()
        .any(|model: &agent_tui::ModelOption| model.provider == "fake" && model.model == "gpt-5")
    {
        models.push(agent_tui::ModelOption {
            provider: "fake".into(),
            model: "gpt-5".into(),
            configured: config.as_ref().and_then(|value| value.provider.as_deref()) == Some("fake"),
            current: config.as_ref().is_none_or(|value| {
                value.provider.as_deref() == Some("fake")
                    && value.model.as_deref().unwrap_or("gpt-5") == "gpt-5"
            }),
            capabilities: "tools,streaming,offline".into(),
        });
    }
    let sessions = Database::open(&paths.db_path)
        .ok()
        .and_then(|db| {
            db.list_session_summaries(&paths.workspace.display().to_string())
                .ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(session_option)
        .collect();
    let diff = ToolRegistry::new(&paths.workspace, PermissionMode::ReadOnly)
        .ok()
        .and_then(|registry| registry.git_diff().ok())
        .map(|result| result.data);
    let diff_variants = ToolRegistry::new(&paths.workspace, PermissionMode::ReadOnly)
        .ok()
        .map(|registry| {
            ["worktree", "staged", "all", "stat", "session"]
                .into_iter()
                .filter_map(|variant| {
                    registry
                        .git_diff_variant(variant)
                        .ok()
                        .map(|result| (variant.to_owned(), result.data))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let active_session = Database::open(&paths.db_path)
        .ok()
        .and_then(|db| {
            db.latest_session(&paths.workspace.display().to_string())
                .ok()
                .flatten()
        })
        .map(|session| session.id);
    let checkpoints = active_session
        .and_then(|session_id| {
            Database::open(&paths.db_path).ok().and_then(|db| {
                let workspace = paths.workspace.display().to_string();
                db.checkpoints(session_id, &workspace).ok().map(|items| {
                    let current_id = items.first().map(|item| item.id);
                    items
                        .into_iter()
                        .map(|checkpoint| {
                            let files = db
                                .checkpoint_files(checkpoint.id, &workspace)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|file| agent_tui::CheckpointFileOption {
                                    path: file.relative_path,
                                    action: if !file.existed {
                                        "create".into()
                                    } else if file.post_hash.is_some() {
                                        "modify".into()
                                    } else {
                                        "snapshot".into()
                                    },
                                    size: file.size,
                                    available: file.available,
                                })
                                .collect();
                            agent_tui::CheckpointOption {
                                id: checkpoint.id,
                                label: checkpoint.label,
                                reason: checkpoint.reason,
                                created_at: checkpoint.created_at,
                                pinned: checkpoint.pinned,
                                current: Some(checkpoint.id) == current_id,
                                files,
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
        })
        .unwrap_or_default();
    let session_tool_override = active_session.and_then(|session_id| {
        Database::open(&paths.db_path).ok().and_then(|db| {
            db.session_tool_override(session_id, &paths.workspace.display().to_string())
                .ok()
                .flatten()
        })
    });
    let disabled = effective_disabled_tool_names_for_session(config_path, active_session, None)
        .unwrap_or_default();
    let tools = ToolRegistry::new(
        &paths.workspace,
        policy
            .as_ref()
            .map(|value| value.mode)
            .unwrap_or(PermissionMode::ReadOnly),
    )
    .ok()
    .map(|registry| {
        registry
            .definitions()
            .into_iter()
            .filter_map(|definition| {
                registry
                    .metadata(&definition.name)
                    .map(|metadata| agent_tui::ToolOption {
                        name: definition.name,
                        description: definition.description,
                        enabled: !disabled.iter().any(|name| name == &metadata.id),
                        scope: if session_tool_override.as_ref().is_some_and(|override_| {
                            override_
                                .enabled
                                .iter()
                                .chain(override_.disabled.iter())
                                .any(|name| name == &metadata.id)
                        }) {
                            "session".into()
                        } else if disabled.iter().any(|name| name == &metadata.id) {
                            "project/global".into()
                        } else {
                            "default".into()
                        },
                        permission: format!("{:?}", metadata.permission_class),
                        cost_hint: format!("estimated schema_cost={}", metadata.context_cost_hint),
                    })
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let (memory, usage_summary, context_summary) = Database::open(&paths.db_path)
        .ok()
        .map(|db| {
            let scope = paths.workspace.display().to_string();
            let memory = SqliteMemoryStore::open(&paths.db_path, 128, 16 * 1024)
                .ok()
                .map(|store| {
                    let active =
                        store
                            .list(&scope, 20)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|entry| agent_tui::MemoryOption {
                                id: entry.id,
                                key: entry.entry.key,
                                kind: entry.entry.kind,
                                summary: redact_output(&entry.entry.content),
                                status: "active".into(),
                                confidence: format!("{:.2}", entry.entry.confidence),
                                importance: entry.entry.importance,
                            });
                    let pending = store
                        .pending(&scope, 20)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|entry| agent_tui::MemoryOption {
                            id: entry.id,
                            key: entry.entry.key,
                            kind: entry.entry.kind,
                            summary: redact_output(&entry.entry.content),
                            status: "pending".into(),
                            confidence: format!("{:.2}", entry.entry.confidence),
                            importance: entry.entry.importance,
                        });
                    active.chain(pending).collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let session = db.latest_session(&scope).ok().flatten();
            let usage = session
                .and_then(|session| db.usage_for_session(session.id).ok())
                .map(|usage| {
                    format!(
                        "input={} output={} cache_read={} cache_write={} reasoning={} total={}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                        usage.reasoning_tokens,
                        usage.total_tokens
                    )
                });
            let context = db
                .latest_session(&scope)
                .ok()
                .flatten()
                .and_then(|session| db.context_for_session(session.id).ok())
                .and_then(|plans| plans.last().cloned())
                .map(|plan| {
                    format!(
                        "estimated={} budget={} selected={} dropped={} truncated={} (estimate)",
                        plan.estimated_tokens,
                        plan.budget,
                        plan.selected_count,
                        plan.dropped_count,
                        plan.truncated_count
                    )
                });
            (memory, usage, context)
        })
        .unwrap_or_default();
    let mcp = load_mcp_config(config_path, &paths.workspace)
        .unwrap_or_default()
        .into_iter()
        .map(|server| agent_tui::McpOption {
            name: server.config.name,
            status: if server.config.enabled {
                "not-started".into()
            } else {
                "disabled".into()
            },
            enabled: server.config.enabled,
            tools: server.config.tool_include,
        })
        .collect();
    let (plugins, hooks) = PluginRegistry::discover(plugin_root(&paths.workspace))
        .ok()
        .map(|registry| {
            let plugins = registry
                .plugins()
                .iter()
                .map(|plugin| agent_tui::ExtensionOption {
                    name: plugin.manifest.name.clone(),
                    description: format!(
                        "v{}; {} declarative tool(s)",
                        plugin.manifest.version,
                        plugin.manifest.tools.len()
                    ),
                    status: if plugin.enabled {
                        "enabled"
                    } else {
                        "installed"
                    }
                    .into(),
                    source: "signed plugin manifest".into(),
                })
                .collect::<Vec<_>>();
            let hooks = registry
                .plugins()
                .iter()
                .flat_map(|plugin| {
                    plugin
                        .manifest
                        .hooks
                        .iter()
                        .map(move |hook| agent_tui::ExtensionOption {
                            name: format!("{}:{:?}", plugin.manifest.name, hook.event),
                            description: "lifecycle hook".into(),
                            status: if plugin.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                            .into(),
                            source: "plugin manifest".into(),
                        })
                })
                .collect::<Vec<_>>();
            (plugins, hooks)
        })
        .unwrap_or_default();
    let skills = PermissionEngine::new(&paths.workspace, PermissionMode::ReadOnly)
        .ok()
        .and_then(|permissions| {
            SkillCatalog::discover(paths.workspace.join("skills"), permissions).ok()
        })
        .map(|catalog| {
            catalog
                .metadata()
                .iter()
                .map(|skill| agent_tui::ExtensionOption {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    status: if skill.auto_invoke {
                        "auto"
                    } else {
                        "available"
                    }
                    .into(),
                    source: catalog.root().display().to_string(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    agent_tui::TuiContext {
        workspace_root: Some(paths.workspace.clone()),
        workspace: Some(
            paths
                .workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| paths.workspace.display().to_string()),
        ),
        branch,
        dirty_files,
        model: config.as_ref().and_then(|value| value.model.clone()),
        permission_mode: policy.map(|value| format!("{:?}", value.mode)),
        palette,
        models,
        sessions,
        checkpoints,
        diff,
        diff_variants,
        tools,
        memory,
        mcp,
        plugins,
        skills,
        hooks,
        tasks,
        agents,
        usage_summary,
        cost_summary: Some("cost unavailable (no explicit pricing configured)".into()),
        context_summary,
        ..agent_tui::TuiContext::default()
    }
}

pub(crate) fn help_category(category: &str) -> &'static str {
    match category {
        "workspace" | "control" | "configuration" => "Configuration",
        "runtime" => "Tools",
        "security" => "Permissions",
        "diagnostics" | "ui" => "UI",
        "data" | "sessions" => "Session",
        "tasks" => "Tasks",
        "git" => "Git",
        "context" => "Context",
        "model" => "Model",
        "skills" => "Skills",
        "mcp" => "MCP",
        "coding" => "Coding",
        _ => "Configuration",
    }
}

pub(crate) fn provider_capability_summary(provider: &str) -> String {
    let Some(profile) = profile_for(provider) else {
        return "capabilities=unknown".into();
    };
    let capabilities = profile.capabilities;
    format!(
        "context_window={:?},tools,caching={},compaction={},reasoning={},streaming={}",
        capabilities.context_window,
        capabilities.prompt_caching,
        capabilities.native_compaction,
        capabilities.reasoning_controls,
        capabilities.streaming
    )
}

pub(crate) fn git_state_snapshot(workspace: &Path) -> (Option<String>, Option<usize>) {
    let branch = OsCommand::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty());
    let dirty_files = OsCommand::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).lines().count());
    (branch, dirty_files)
}

pub(crate) fn parse_direct_argv(input: &str) -> Result<Vec<String>, String> {
    let argv = input
        .trim_start_matches('!')
        .split_whitespace()
        .collect::<Vec<_>>();
    if argv.is_empty()
        || argv.iter().any(|part| {
            part.chars()
                .any(|character| matches!(character, '\'' | '"' | '`' | ';' | '|' | '&'))
        })
    {
        return Err("direct command requires explicit unquoted argv".into());
    }
    Ok(argv.into_iter().map(str::to_owned).collect())
}

pub(crate) fn path_target_output(input: &str) -> String {
    let target = input.trim_start_matches('@');
    match workspace_root() {
        Ok(workspace) => {
            let path = workspace.join(target);
            format!("path={} exists={}", path.display(), path.exists())
        }
        Err(error) => format!("error: {error}"),
    }
}

pub(crate) fn slash_command_output(command: &str) -> Option<String> {
    match command {
        "/help" | "/commands" => {
            let mut definitions = command_registry();
            definitions.sort_by_key(|definition| (definition.category, definition.name));
            let mut lines = Vec::new();
            let mut category = "";
            for definition in definitions {
                let mapped_category = help_category(definition.category);
                if mapped_category != category {
                    category = mapped_category;
                    lines.push(format!("[{category}]"));
                }
                lines.push(format!(
                    "/{} {} — {}",
                    definition.name, definition.arguments, definition.description
                ));
            }
            Some(lines.join("\n"))
        }
        "/version" => Some(format!("helios {}", env!("CARGO_PKG_VERSION"))),
        "/model" => app_paths().ok().map(|paths| {
            load_user_config(paths.effective_project_config())
                .ok()
                .and_then(|config| config.model)
                .map(|model| format!("model={model}"))
                .unwrap_or_else(|| "model=unavailable".into())
        }),
        "/effort" => Some(
            app_paths()
                .ok()
                .and_then(|paths| {
                    let db = Database::open(&paths.db_path).ok()?;
                    let workspace = paths.workspace.display().to_string();
                    let session = db.latest_session(&workspace).ok()??;
                    db.session_override(session.id, &workspace).ok()??.effort
                })
                .map(|effort| format!("effort={effort} (session override)"))
                .unwrap_or_else(|| "effort=provider-default (no session override)".into()),
        ),
        "/usage" => app_paths().ok().and_then(|paths| {
            let db = Database::open(&paths.db_path).ok()?;
            let session = db
                .latest_session(&paths.workspace.display().to_string())
                .ok()??;
            let usage = db.usage_for_session(session.id).ok()?;
            Some(format!(
                "usage input={} output={} cache_read={} cache_write={} reasoning={} total={}",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                usage.reasoning_tokens,
                usage.total_tokens
            ))
        }),
        "/cost" => Some("cost=unavailable (configure explicit provider pricing)".into()),
        "/tools" | "/tools list" => app_paths().ok().and_then(|paths| {
            let policy = load_runtime_policy(paths.effective_project_config()).ok()?;
            let registry = ToolRegistry::new(&paths.workspace, policy.mode).ok()?;
            Some(
                registry
                    .definitions()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }),
        "/plugins" => app_paths().ok().map(|paths| {
            PluginRegistry::discover(plugin_root(&paths.workspace))
                .ok()
                .map(|registry| {
                    registry
                        .plugins()
                        .iter()
                        .map(|plugin| {
                            format!(
                                "{} {}",
                                plugin.manifest.name,
                                if plugin.enabled {
                                    "enabled"
                                } else {
                                    "installed"
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "plugins=0".into())
        }),
        "/skills" => app_paths().ok().map(|paths| {
            let Ok(permissions) = PermissionEngine::new(&paths.workspace, PermissionMode::ReadOnly)
            else {
                return "skills=0".into();
            };
            SkillCatalog::discover(paths.workspace.join("skills"), permissions)
                .ok()
                .map(|catalog| {
                    catalog
                        .metadata()
                        .iter()
                        .map(|skill| format!("{} — {}", skill.name, skill.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "skills=0".into())
        }),
        "/hooks" => app_paths().ok().map(|paths| {
            PluginRegistry::discover(plugin_root(&paths.workspace))
                .ok()
                .map(|registry| {
                    registry
                        .plugins()
                        .iter()
                        .flat_map(|plugin| {
                            plugin.manifest.hooks.iter().map(move |hook| {
                                format!("{}:{:?}", plugin.manifest.name, hook.event)
                            })
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "hooks=0".into())
        }),
        "/memory" | "/memory list" => app_paths().ok().and_then(|paths| {
            let store = SqliteMemoryStore::open(&paths.db_path, 128, 16 * 1024).ok()?;
            let entries = store
                .list(&paths.workspace.display().to_string(), 20)
                .ok()?;
            let pending = store
                .pending(&paths.workspace.display().to_string(), 20)
                .ok()?;
            Some(format!(
                "memory={} pending={}",
                entries.len(),
                pending.len()
            ))
        }),
        "/queue" | "/queue list" => app_paths().ok().and_then(|paths| {
            let db = Database::open(&paths.db_path).ok()?;
            let session = db
                .latest_session(&paths.workspace.display().to_string())
                .ok()??;
            let items = db.foreground_queue(session.id).ok()?;
            Some(format!("queue={}", items.len()))
        }),
        "/stop" => Some("stop requested for active turn; session remains open".into()),
        "/status" => app_paths().ok().map(|paths| {
            format!(
                "workspace={}\nconfig={}\ndb_present={}",
                paths.workspace.display(),
                paths.effective_project_config().display(),
                paths.db_path.is_file()
            )
        }),
        "/permissions" => app_paths().ok().and_then(|paths| {
            load_runtime_policy(paths.effective_project_config())
                .ok()
                .map(|policy| format!("permission_mode={:?}", policy.mode))
        }),
        "/sessions" => app_paths().ok().and_then(|paths| {
            Database::open(&paths.db_path)
                .ok()
                .and_then(|db| {
                    db.list_sessions(&paths.workspace.display().to_string())
                        .ok()
                })
                .map(|sessions| {
                    if sessions.is_empty() {
                        return "sessions=0".into();
                    }
                    let mut lines = vec![format!("sessions={}", sessions.len())];
                    lines.extend(sessions.into_iter().map(|session| {
                        format!("#{} {}", session.id, redact_output(&session.objective))
                    }));
                    lines.join("\n")
                })
        }),
        "/context" => app_paths().ok().and_then(|paths| {
            let db = Database::open(&paths.db_path).ok()?;
            let session = db
                .latest_session(&paths.workspace.display().to_string())
                .ok()??;
            let plans = db.context_for_session(session.id).ok()?;
            let plan = plans.last()?;
            Some(format!(
                "context estimated={} budget={} selected={} dropped={} truncated={} (estimated)",
                plan.estimated_tokens,
                plan.budget,
                plan.selected_count,
                plan.dropped_count,
                plan.truncated_count
            ))
        }),
        command if command == "/diff" || command.starts_with("/diff ") => {
            let workspace = workspace_root().ok()?;
            let registry = ToolRegistry::new(workspace, PermissionMode::ReadOnly).ok()?;
            let variant = command.strip_prefix("/diff ").unwrap_or("worktree");
            registry
                .git_diff_variant(variant)
                .ok()
                .map(|result| result.data)
        }
        _ => None,
    }
}

pub(crate) fn mcp_interactive_command(
    command: &str,
    workspace: &Path,
    config_path: &Path,
) -> String {
    let action = command.strip_prefix("/mcp").unwrap_or("").trim();
    let parts = action.split_whitespace().collect::<Vec<_>>();
    let action = parts.first().copied().unwrap_or("list");
    let servers = match load_mcp_config(config_path, workspace) {
        Ok(servers) => servers,
        Err(error) => return format!("mcp config error: {error}"),
    };
    if matches!(action, "enable" | "disable") {
        let Some(id) = parts.get(1) else {
            return format!("/mcp {action} requires server id");
        };
        return runtime_server_array_command(
            "mcp",
            &[action.to_string(), "--id".into(), (*id).into()],
            config_path,
        )
        .map(|_| {
            format!(
                "mcp {id} {}d",
                if action == "enable" {
                    "enable"
                } else {
                    "disable"
                }
            )
        })
        .unwrap_or_else(|error| error);
    }
    if action == "auth" {
        return servers
            .into_iter()
            .map(|server| match server.config.transport {
                agent_mcp::McpTransport::StreamableHttp { auth_env, .. } => format!(
                    "{} auth_env={}",
                    server.config.name,
                    auth_env.unwrap_or_else(|| "none".into())
                ),
                agent_mcp::McpTransport::Stdio => {
                    format!("{} auth=not-configured", server.config.name)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if action == "reload" {
        let Some(id) = parts.get(1) else {
            return "/mcp reload requires server id".into();
        };
        return mcp_discover_server(id, workspace, config_path)
            .map(|result| format!("{result} reloaded"))
            .unwrap_or_else(|error| format!("mcp reload error: {error}"));
    }
    if action == "list" {
        return servers
            .into_iter()
            .map(|server| {
                format!(
                    "{} enabled={} status=not-started",
                    server.config.name, server.config.enabled
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if action == "warm" || action == "discover" {
        let Some(id) = parts.get(1) else {
            return "/mcp warm requires server id".into();
        };
        return mcp_discover_server(id, workspace, config_path)
            .unwrap_or_else(|error| format!("mcp discovery error: {error}"));
    }
    "mcp action requires list, enable, disable, auth, reload, or warm".into()
}

pub(crate) fn mcp_discover_server(
    id: &str,
    workspace: &Path,
    config_path: &Path,
) -> Result<String, String> {
    let server = load_mcp_config(config_path, workspace)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|server| server.config.name == id)
        .ok_or("MCP server not found")?;
    let engine = PermissionEngine::new(workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    let policy = HostAllowlist {
        inner: PermissionPolicy::new(engine),
        allowed_hosts: server.allowed_hosts.clone(),
    };
    let cancellation = CancellationToken::default();
    let hook = mcp_child_hook(workspace, server.network_allowed);
    let mut client = McpClient::new(server.config, policy, 64 * 1024)
        .with_child_hook(hook)
        .with_oauth(server.oauth.clone());
    client
        .initialize(&cancellation)
        .map_err(|error| error.to_string())?;
    let tools = client
        .discover_tools(&cancellation)
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "mcp={} status=ready tools={}",
        id,
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .take(128)
            .collect::<Vec<_>>()
            .join(",")
    ))
}

pub(crate) fn println_result(result: Result<String, String>) -> Result<(), String> {
    println!("{}", result?);
    Ok(())
}

pub(crate) fn mcp_auth_command(config_path: &Path) -> Result<(), String> {
    let workspace = workspace_root()?;
    for server in load_mcp_config(config_path, &workspace).map_err(|error| error.to_string())? {
        match server.config.transport {
            agent_mcp::McpTransport::StreamableHttp { auth_env, .. } => println!(
                "{} auth_env={}",
                server.config.name,
                auth_env.unwrap_or_else(|| "none".into())
            ),
            agent_mcp::McpTransport::Stdio => {
                println!("{} auth=not-configured", server.config.name)
            }
        }
    }
    Ok(())
}

pub(crate) fn custom_prompt_command(command: &str, config_path: &Path) -> Option<String> {
    let name = command.strip_prefix('/')?.split_whitespace().next()?;
    if name.is_empty() || command != format!("/{name}") {
        return None;
    }
    let document: toml::Value = toml::from_str(&fs::read_to_string(config_path).ok()?).ok()?;
    let entry = document.get("commands")?.get(name)?.as_table()?;
    if entry.get("type").and_then(toml::Value::as_str) != Some("prompt") {
        return None;
    }
    let prompt = entry.get("prompt")?.as_str()?.trim();
    (!prompt.is_empty()).then(|| prompt.to_owned())
}

pub(crate) fn custom_exec_command(
    command: &str,
    workspace: &Path,
    project_config: &Path,
    approval: ApprovalBroker,
    cancellation: CancellationToken,
) -> Option<Result<String, String>> {
    let name = command.strip_prefix('/')?.split_whitespace().next()?;
    if name.is_empty()
        || command != format!("/{name}")
        || command_registry().iter().any(|item| item.name == name)
    {
        return None;
    }
    let paths = AppPaths::discover(workspace).ok()?;
    let source = [project_config.to_path_buf(), paths.global_config]
        .into_iter()
        .find_map(|path| {
            let value: toml::Value = toml::from_str(&fs::read_to_string(path).ok()?).ok()?;
            value.get("commands")?.get(name)?.as_table().cloned()
        })?;
    if source.get("type").and_then(toml::Value::as_str) != Some("exec") {
        return None;
    }
    let argv = source
        .get("argv")
        .and_then(toml::Value::as_array)
        .and_then(|values| {
            values
                .iter()
                .map(toml::Value::as_str)
                .collect::<Option<Vec<_>>>()
        })?;
    let (program, args) = argv.split_first()?;
    let policy = load_runtime_policy(project_config).unwrap_or_default();
    let registry = match ToolRegistry::new(workspace, policy.mode) {
        Ok(registry) => registry
            .with_approval_broker(approval)
            .with_cancellation(cancellation.clone()),
        Err(error) => return Some(Err(error.to_string())),
    };
    let result = registry.execute(
        "shell",
        serde_json::json!({"program": program, "args": args, "cwd": workspace.display().to_string(), "requires_pty": false}),
        &cancellation,
    );
    Some(
        result
            .map(|outcome| redact_output(&outcome.data))
            .map_err(|error| error.to_string()),
    )
}

pub(crate) fn edit_external_prompt(initial: &str, workspace: &Path) -> Result<String, String> {
    let paths = AppPaths::discover(workspace).map_err(|error| error.to_string())?;
    paths.ensure_dirs().map_err(|error| error.to_string())?;
    let editor = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .map_err(|_| "no VISUAL or EDITOR configured".to_owned())?;
    let argv = parse_direct_argv(&editor)
        .map_err(|_| "editor must be an explicit executable and argv".to_owned())?;
    let program = argv.first().ok_or("editor command is empty")?.clone();
    let path = paths
        .state_dir
        .join(format!("compose-{}.txt", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| error.to_string())?;
    file.write_all(initial.as_bytes())
        .map_err(|error| error.to_string())?;
    let result = agent_terminal::run(
        agent_terminal::CommandSpec {
            program,
            args: argv
                .into_iter()
                .skip(1)
                .chain(std::iter::once(path.display().to_string()))
                .collect(),
            cwd: workspace.to_path_buf(),
            timeout: Duration::from_secs(60),
            output_limit: 8 * 1024,
            spill_dir: paths.spill_dir,
            requires_pty: false,
            env_allowlist: vec![],
        },
        &CancellationToken::default(),
    )
    .map_err(|error| error.to_string());
    let text = result.and_then(|result| match result.status {
        agent_terminal::CommandStatus::Exited(0) => {
            fs::read_to_string(&path).map_err(|error| error.to_string())
        }
        status => Err(format!("editor failed: {status:?}")),
    });
    let _ = fs::remove_file(&path);
    text.map(|value| value.chars().take(64 * 1024).collect())
}

pub(crate) fn copy_last_text(target: &str, workspace: &Path) -> String {
    let text = if target == "diff" {
        ToolRegistry::new(workspace, PermissionMode::ReadOnly)
            .ok()
            .and_then(|registry| registry.git_diff_variant("worktree").ok())
            .map(|result| result.data)
            .unwrap_or_default()
    } else {
        "last response is available in the durable session export".into()
    };
    let program = ["wl-copy", "xclip", "xsel"]
        .into_iter()
        .find(|candidate| command_available(candidate));
    if let Some(program) = program {
        let args: Vec<String> = match program {
            "xclip" => vec!["-selection".into(), "clipboard".into()],
            "xsel" => vec!["--clipboard", "--input"]
                .into_iter()
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        };
        let mut child = match OsCommand::new(program)
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                return format!(
                    "clipboard unavailable; selectable fallback:\n{}",
                    text.chars().take(4096).collect::<String>()
                )
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().is_ok() {
            return "copied to clipboard".into();
        }
    }
    format!(
        "clipboard unavailable; selectable fallback:\n{}",
        text.chars().take(4096).collect::<String>()
    )
}

pub(crate) fn tui_config_command(args: &[String], path: &Path) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("show");
    let mut document: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    let current = document
        .get("tui")
        .cloned()
        .unwrap_or(toml::Value::Table(Default::default()));
    if action == "show" || action == "keys" {
        println!(
            "{}",
            if action == "keys" {
                toml::Value::String("q Esc Ctrl-C cancel; Up Down scroll".into())
            } else {
                current
            }
        );
        return Ok(());
    }
    if action != "set" {
        return Err(format!("unknown tui action: {action}"));
    }
    let tui = document
        .as_table_mut()
        .ok_or("config root must be a table")?
        .entry("tui")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("tui config must be a table")?;
    for (flag, key, min, max) in [
        ("--tick-ms", "tick_ms", 10, 1000),
        ("--channel-capacity", "channel_capacity", 1, 4096),
        ("--scroll-step", "scroll_step", 1, 100),
    ] {
        if let Some(value) = value_after(args, flag)? {
            let parsed: i64 = value
                .parse()
                .map_err(|_| format!("{flag} must be an integer"))?;
            if parsed < min || parsed > max {
                return Err(format!("{flag} outside safe bounds"));
            }
            tui.insert(key.into(), toml::Value::Integer(parsed));
        }
    }
    if let Some(color) = value_after(args, "--color")? {
        if !matches!(color.as_str(), "auto" | "always" | "never") {
            return Err("--color must be auto, always, or never".into());
        }
        tui.insert("color".into(), toml::Value::String(color));
    }
    if let Some(theme) = value_after(args, "--theme")? {
        if !matches!(theme.as_str(), "dark" | "light" | "system" | "monochrome") {
            return Err("--theme must be dark, light, system, or monochrome".into());
        }
        tui.insert("theme".into(), toml::Value::String(theme));
    }
    atomic_write(
        path,
        toml::to_string_pretty(&document)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
}

pub(crate) fn load_tui_settings(path: &Path) -> Result<agent_tui::TuiSettings, String> {
    let mut settings = agent_tui::TuiSettings::default();
    if !path.exists() {
        return Ok(settings);
    }
    let document: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let Some(table) = document.get("tui").and_then(toml::Value::as_table) else {
        return Ok(settings);
    };
    if let Some(value) = table.get("tick_ms").and_then(toml::Value::as_integer) {
        settings.tick_ms = value.clamp(10, 1000) as u64;
    }
    if let Some(value) = table
        .get("channel_capacity")
        .and_then(toml::Value::as_integer)
    {
        settings.channel_capacity = value.clamp(1, 4096) as usize;
    }
    if let Some(value) = table.get("scroll_step").and_then(toml::Value::as_integer) {
        settings.scroll_step = value.clamp(1, 100) as usize;
    }
    if let Some(value) = table.get("alternate_screen").and_then(toml::Value::as_bool) {
        settings.alternate_screen = value;
    }
    if let Some(value) = table.get("mouse").and_then(toml::Value::as_bool) {
        settings.mouse = value;
    }
    if let Some(value) = table.get("notify").and_then(toml::Value::as_bool) {
        settings.notify = value;
    }
    settings.color = match table.get("color").and_then(toml::Value::as_str) {
        Some("always") => agent_tui::TuiColor::Always,
        Some("never") => agent_tui::TuiColor::Never,
        _ => agent_tui::TuiColor::Auto,
    };
    settings.theme = match table.get("theme").and_then(toml::Value::as_str) {
        Some("dark") => agent_tui::TuiTheme::Dark,
        Some("light") => agent_tui::TuiTheme::Light,
        Some("monochrome") => agent_tui::TuiTheme::Monochrome,
        _ => agent_tui::TuiTheme::System,
    };
    Ok(settings)
}
