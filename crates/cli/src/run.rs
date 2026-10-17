//! Headless run and exec: option resolution, provider dispatch, image loading and event journaling.

use super::*;

/// Reads and validates `--image` files. Each must resolve inside the
/// workspace (so the model cannot be shown files outside it), be a regular
/// file of at most 5 MiB, and start with a PNG, JPEG, GIF or WebP signature.
pub(crate) fn load_run_images(
    workspace: &Path,
    paths: &[String],
) -> Result<Vec<agent_protocol::ImageInput>, String> {
    const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
    const MAX_IMAGES: usize = 4;
    if paths.len() > MAX_IMAGES {
        return Err(format!("at most {MAX_IMAGES} images can be attached"));
    }
    let engine = PermissionEngine::new(workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    paths
        .iter()
        .map(|raw| {
            let path = engine
                .resolve_path(raw)
                .map_err(|error| format!("{raw}: {error}"))?;
            let metadata = fs::metadata(&path).map_err(|error| format!("{raw}: {error}"))?;
            if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
                return Err(format!("{raw}: image must be a file of at most 5 MiB"));
            }
            let bytes = fs::read(&path).map_err(|error| format!("{raw}: {error}"))?;
            let media_type = image_media_type(&bytes)
                .ok_or_else(|| format!("{raw}: not a PNG, JPEG, GIF or WebP image"))?;
            Ok(agent_protocol::ImageInput {
                media_type: media_type.into(),
                data: agent_protocol::base64_encode(&bytes),
            })
        })
        .collect()
}

/// Media type from the leading magic bytes; `None` for anything else.
pub(crate) fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn run_command(options: RunOptions) -> Result<(), String> {
    run_command_with_cancellation(options, CancellationToken::default())
}

/// Resolve the provider-facing runtime settings once for every entry point.
/// Keeping this ahead of the interactive and headless paths makes the
/// precedence contract identical: project/global config, environment, then
/// explicit CLI/session values, with fake as the offline default.
pub(crate) fn resolve_runtime_options(
    mut options: RunOptions,
) -> Result<(RunOptions, RuntimePolicy, PathBuf), String> {
    let workspace = workspace_root()?;
    let config_path = options
        .config
        .as_deref()
        .map(|path| resolve_workspace_file(path, &workspace))
        .transpose()?
        .unwrap_or_else(|| workspace.join(".helios/config.toml"));
    let configured = load_user_config(&config_path)?;
    let runtime_config_path = effective_config_path(&config_path)?;
    let runtime_policy = load_runtime_policy(&runtime_config_path)?;
    if !options.provider_explicit {
        if let Some(provider) = configured.provider {
            options.provider = provider;
        }
    }
    if !options.model_explicit {
        if let Some(model) = configured.model {
            options.model = model;
        }
    }
    if !options.base_url_explicit {
        options.base_url = configured.base_url;
    }
    Ok((options, runtime_policy, workspace))
}

pub(crate) fn run_command_with_cancellation(
    options: RunOptions,
    cancellation: CancellationToken,
) -> Result<(), String> {
    if options.tui && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal()) {
        return Err("--tui requires an interactive TTY".into());
    }
    let (mut options, runtime_policy, workspace) = resolve_runtime_options(options)?;
    let permission_mode = runtime_policy.mode;
    let config_path = options
        .config
        .as_deref()
        .map(|path| resolve_workspace_file(path, &workspace))
        .transpose()?
        .unwrap_or_else(|| workspace.join(".helios/config.toml"));
    let runtime_config_path = effective_config_path(&config_path)?;
    let mut database = open_database(&options.db_path)?;
    let workspace_id = workspace.display().to_string();
    let (session_id, objective, messages) = if let Some(selector) = options.resume.as_deref() {
        let record = if selector == "latest" {
            database
                .latest_session(&workspace_id)
                .map_err(|error| error.to_string())?
        } else {
            database
                .session(
                    selector
                        .parse::<i64>()
                        .map_err(|_| "--resume expects an integer id or latest")?,
                )
                .map_err(|error| error.to_string())?
        }
        .ok_or("session not found")?;
        if record.workspace != workspace_id {
            return Err("session belongs to another workspace".into());
        }
        let messages = database
            .session_context_messages(record.id)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|(role, content)| Ok(Message::new(parse_role(&role)?, content)))
            .collect::<Result<Vec<_>, String>>()?;
        (record.id, record.objective, messages)
    } else {
        (
            database
                .create_session_for_workspace(&options.prompt, &workspace_id)
                .map_err(|error| error.to_string())?,
            options.prompt.clone(),
            Vec::new(),
        )
    };
    if options.resume.is_some() {
        if let Some(override_config) = database
            .session_override(session_id, &workspace_id)
            .map_err(|error| error.to_string())?
        {
            if !options.provider_explicit {
                if let Some(provider) = override_config.provider {
                    options.provider = provider;
                }
            }
            if !options.model_explicit {
                if let Some(model) = override_config.model {
                    options.model = model;
                }
            }
            if options.effort.is_none() {
                options.effort = override_config.effort;
            }
        }
    }
    database
        .set_session_override(
            session_id,
            &workspace_id,
            Some(&options.provider),
            Some(&options.model),
            options.effort.as_deref(),
            None,
        )
        .map_err(|error| error.to_string())?;
    let config_path = runtime_config_path
        .is_file()
        .then_some(runtime_config_path.clone());
    let mut servers = config_path
        .as_ref()
        .map(|path| load_mcp_config(path, &workspace))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    for server in &mut servers {
        runtime_policy.filter_env(&mut server.config.env_allowlist);
        if !runtime_policy.allowed_network_hosts.is_empty() {
            server.allowed_hosts.retain(|host| {
                runtime_policy
                    .allowed_network_hosts
                    .iter()
                    .any(|allowed| allowed == host)
            });
        }
    }
    let mut lsp_servers = config_path
        .as_ref()
        .map(|path| load_lsp_config(path, &workspace))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    for server in &mut lsp_servers {
        runtime_policy.filter_env(&mut server.env_allowlist);
    }
    if options.exec {
        let signal_cancellation = cancellation.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal_cancellation.cancel();
                }
            });
        }
    }
    let disabled_tools = effective_disabled_tool_names_for_session(
        &runtime_config_path,
        Some(session_id),
        Some(&options.db_path),
    )?;
    let checkpoint_id = database
        .create_checkpoint(
            session_id,
            &workspace_id,
            "auto-before-turn",
            "automatic pre-mutation checkpoint",
        )
        .ok();
    let snapshot_hook = checkpoint_id.map(|checkpoint_id| {
        let db_path = options.db_path.clone();
        let workspace_id = workspace_id.clone();
        Arc::new(move |event: SnapshotEvent| -> Result<(), String> {
            let db = Database::open(&db_path).map_err(|error| error.to_string())?;
            match event {
                SnapshotEvent::Before {
                    path,
                    content,
                    existed,
                } => db
                    .record_checkpoint_file(checkpoint_id, &workspace_id, &path, existed, &content)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                SnapshotEvent::After { path, content, .. } => db
                    .update_checkpoint_file_after(checkpoint_id, &workspace_id, &path, &content)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
            }
        }) as SnapshotHook
    });
    let builtin = ToolRegistry::new(&workspace, permission_mode)
        .map_err(|error| error.to_string())?
        .with_disabled_tools(disabled_tools)
        .with_web_allowed_hosts(&runtime_policy.allowed_network_hosts)
        .with_cancellation(cancellation.clone());
    let builtin = if let Some(hook) = snapshot_hook {
        builtin.with_snapshot_hook(hook)
    } else {
        builtin
    };
    let builtin = if let Some(approval) = options.approval.clone() {
        builtin.with_approval_broker(approval)
    } else {
        builtin
    };
    let mut tools = RuntimeExecutor::new(
        builtin,
        servers,
        &workspace,
        cancellation.clone(),
        lsp_servers,
        config_path.clone(),
        permission_mode,
    )?;
    let hooks = load_runtime_hooks(
        &workspace,
        config_path.as_deref(),
        cancellation.clone(),
        permission_mode,
        &runtime_policy.env_allow,
        &runtime_policy.env_deny,
    )?;
    let mut startup_memory = load_startup_memory(&options.db_path, &workspace_id, &options.prompt)?;
    startup_memory.extend(load_startup_skills(
        &workspace,
        config_path.as_deref(),
        &options.prompt,
    )?);
    if options.tui {
        let tui_settings = config_path
            .as_deref()
            .map(load_tui_settings)
            .transpose()?
            .unwrap_or_default();
        let (sender, receiver) = agent_tui::bounded_event_channel(tui_settings.channel_capacity);
        return std::thread::scope(|scope| {
            let producer = scope.spawn(|| {
                run_selected_provider(
                    &options,
                    &mut tools,
                    &mut database,
                    session_id,
                    objective,
                    messages,
                    startup_memory.clone(),
                    hooks,
                    Some(sender),
                    false,
                    cancellation.clone(),
                )
            });
            agent_tui::run_channel_with_settings(receiver, cancellation.clone(), tui_settings)
                .map_err(|error| error.to_string())?;
            producer
                .join()
                .map_err(|_| "agent producer panicked".to_owned())?
                .map(|_| ())
        });
    }
    let outcome = run_selected_provider(
        &options,
        &mut tools,
        &mut database,
        session_id,
        objective,
        messages,
        startup_memory,
        hooks,
        options.event_sender.clone(),
        options.exec && options.output_format == OutputFormat::Text,
        cancellation,
    )?;
    if options.exec {
        emit_headless_output(&options, session_id, &outcome);
    } else if options.event_sender.is_none() {
        for event in outcome.events {
            if !matches!(event, agent_protocol::Event::ModelDelta(_)) {
                println!("{event:?}");
            }
        }
    }
    Ok(())
}

pub(crate) fn emit_headless_output(options: &RunOptions, session_id: i64, outcome: &RunOutcome) {
    let mut sequence = 0_u64;
    let mut emit = |kind: &str, payload: serde_json::Value| {
        let envelope = serde_json::json!({
            "schema_version": 1,
            "sequence": sequence,
            "timestamp": output_timestamp(),
            "session_id": session_id,
            "turn_id": outcome.turn_id,
            "kind": kind,
            "payload": payload,
        });
        if options.output_format == OutputFormat::StreamJson {
            println!("{}", envelope);
        }
        sequence = sequence.saturating_add(1);
    };
    let mut result = String::new();
    let mut tool_calls = 0_u64;
    let mut errors = Vec::new();
    let mut usage = serde_json::Map::new();
    for event in &outcome.events {
        match event {
            agent_protocol::Event::ModelDelta(text) => {
                result.push_str(&redact_output(text));
                if options.output_format == OutputFormat::StreamJson {
                    emit(
                        "model_delta",
                        serde_json::json!({"text": redact_output(text)}),
                    );
                }
            }
            agent_protocol::Event::ToolRequested(name) => {
                tool_calls = tool_calls.saturating_add(1);
                if options.output_format == OutputFormat::StreamJson {
                    emit("tool", serde_json::json!({"name": redact_output(name)}));
                }
            }
            agent_protocol::Event::ToolStarted(name)
            | agent_protocol::Event::ToolCompleted(name) => {
                if options.output_format == OutputFormat::StreamJson {
                    emit("tool", serde_json::json!({"name": redact_output(name)}));
                }
            }
            agent_protocol::Event::UsageUpdated(value) => {
                usage.insert(
                    "raw".into(),
                    serde_json::Value::String(redact_output(value)),
                );
                if options.output_format == OutputFormat::StreamJson {
                    emit("usage", serde_json::json!({"value": redact_output(value)}));
                }
            }
            agent_protocol::Event::Error(error) => {
                errors.push(redact_output(error));
                if options.output_format == OutputFormat::StreamJson {
                    emit("error", serde_json::json!({"error": redact_output(error)}));
                }
            }
            _ => {}
        }
    }
    let status = if errors.is_empty() {
        "complete"
    } else {
        "failed"
    };
    emit(
        "result",
        serde_json::json!({
            "status": status,
            "result": result,
            "usage": usage,
            "tool_calls": tool_calls,
            "errors": errors,
        }),
    );
    if options.output_format == OutputFormat::Json {
        let envelope = serde_json::json!({
            "schema_version": 1,
            "sequence": 0,
            "timestamp": output_timestamp(),
            "session_id": session_id,
            "turn_id": outcome.turn_id,
            "kind": "result",
            "payload": {
                "status": status,
                "result": result,
                "usage": usage,
                "tool_calls": tool_calls,
                "errors": errors,
            },
        });
        println!("{}", envelope);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_selected_provider(
    options: &RunOptions,
    tools: &mut impl ToolExecutor,
    database: &mut Database,
    session_id: i64,
    objective: String,
    messages: Vec<Message>,
    startup_memory: Vec<RetrievedMemory>,
    hooks: Option<Box<dyn HookRunner>>,
    live_events: Option<SyncSender<agent_protocol::Event>>,
    live_stdout: bool,
    cancellation: CancellationToken,
) -> Result<RunOutcome, String> {
    let images = load_run_images(&workspace_root()?, &options.images)?;
    let context_budget = profile_for(&options.provider)
        .and_then(|profile| profile.capabilities.context_window)
        .unwrap_or(16_000);
    match options.provider.as_str() {
        "fake" => run_provider(
            FakeProvider,
            tools,
            database,
            session_id,
            objective,
            messages,
            startup_memory,
            hooks,
            &options.prompt,
            live_stdout,
            live_events,
            cancellation.clone(),
            context_budget,
            options.steering.clone(),
            images.clone(),
        ),
        "openai" => run_provider(
            OpenAiProvider::with_credentials(
                options.model.clone(),
                credential_for("openai")?,
                options
                    .base_url
                    .clone()
                    .or_else(|| env::var("OPENAI_BASE_URL").ok())
                    .unwrap_or_else(|| "https://api.openai.com/v1/responses".into()),
            )
            .with_tuning(request_tuning(options.effort.as_deref())),
            tools,
            database,
            session_id,
            objective,
            messages,
            startup_memory,
            hooks,
            &options.prompt,
            live_stdout,
            live_events,
            cancellation.clone(),
            context_budget,
            options.steering.clone(),
            images.clone(),
        ),
        "anthropic" => run_provider(
            AnthropicProvider::with_credentials(
                options.model.clone(),
                credential_for("anthropic")?,
                options
                    .base_url
                    .clone()
                    .or_else(|| env::var("ANTHROPIC_BASE_URL").ok())
                    .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".into()),
            )
            .with_tuning(request_tuning(options.effort.as_deref())),
            tools,
            database,
            session_id,
            objective,
            messages,
            startup_memory,
            hooks,
            &options.prompt,
            live_stdout,
            live_events,
            cancellation.clone(),
            context_budget,
            options.steering.clone(),
            images.clone(),
        ),
        "gemini" => run_provider(
            GeminiProvider::with_credentials(
                options.model.clone(),
                credential_for("gemini")?,
                options
                    .base_url
                    .clone()
                    .or_else(|| env::var("GEMINI_BASE_URL").ok())
                    .unwrap_or_else(|| {
                        "https://generativelanguage.googleapis.com/v1beta/models".into()
                    }),
            )
            .with_tuning(request_tuning(options.effort.as_deref())),
            tools,
            database,
            session_id,
            objective,
            messages,
            startup_memory,
            hooks,
            &options.prompt,
            live_stdout,
            live_events,
            cancellation.clone(),
            context_budget,
            options.steering.clone(),
            images.clone(),
        ),
        "generic" | "openai-compatible" => run_provider(
            OpenAiCompatibleProvider::with_credentials(
                options.model.clone(),
                credential_for("generic")?,
                options
                    .base_url
                    .clone()
                    .or_else(|| env::var("GENERIC_BASE_URL").ok())
                    .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".into()),
            )
            .with_tuning(request_tuning(options.effort.as_deref())),
            tools,
            database,
            session_id,
            objective,
            messages,
            startup_memory,
            hooks,
            &options.prompt,
            live_stdout,
            live_events,
            cancellation,
            context_budget,
            options.steering.clone(),
            images.clone(),
        ),
        other => Err(format!("unknown provider: {other}")),
    }
}

/// Stable system prompt: base rules, a role overlay, then bounded project
/// instructions (`AGENTS.md`, `.helios/AGENTS.md`). Built once per run so the
/// provider prompt-cache prefix stays byte-identical across turns.
pub(crate) fn system_prompt(overlay: &str, root: &Path) -> String {
    let mut sections = vec![
        include_str!("../../../prompts/SYSTEM.md").trim().to_owned(),
        overlay.trim().to_owned(),
    ];
    if let Some(instructions) = load_project_instructions(root) {
        sections.push(format!(
            "Repository instructions follow. Apply them unless they conflict with the rules above or with workspace, network and approval restrictions.\n\n{instructions}"
        ));
    }
    sections.join("\n\n")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider<P: ModelProvider, E: ToolExecutor>(
    provider: P,
    tools: &mut E,
    database: &mut Database,
    session_id: i64,
    objective: String,
    messages: Vec<Message>,
    startup_memory: Vec<RetrievedMemory>,
    hooks: Option<Box<dyn HookRunner>>,
    prompt: &str,
    live_stdout: bool,
    live_events: Option<SyncSender<agent_protocol::Event>>,
    cancellation: CancellationToken,
    context_budget: u64,
    steering: Option<SteeringChannel>,
    images: Vec<agent_protocol::ImageInput>,
) -> Result<RunOutcome, String> {
    let previous = messages.len();
    let memory_scope = workspace_root()?.display().to_string();
    let mut agent = AgentLoop::new(provider)
        .with_images(images)
        .with_context_budget(context_budget)
        .with_memory_scope(memory_scope)
        .with_memory_retriever(Box::new(StaticMemory(startup_memory)))
        .with_compactor(Box::new(LocalCompactor))
        .with_system_prompt(system_prompt(
            include_str!("../../../prompts/CODING_AGENT.md"),
            &workspace_root()?,
        ));
    if let Some(steering) = steering {
        agent = agent.with_steering(steering);
    }
    if let Some(hooks) = hooks {
        agent = agent.with_hooks(hooks);
    }
    agent.restore_session(objective, messages);
    let turn_index = database
        .session_turn_count(session_id)
        .map_err(|error| error.to_string())?;
    let turn_id = database
        .transactional(|transaction| {
            transaction.execute(
                "INSERT INTO turns(session_id, turn_index, created_at, status) VALUES (?1, ?2, strftime('%s','now'), 'running')",
                rusqlite::params![session_id, turn_index],
            )?;
            let turn_id = transaction.last_insert_rowid();
            transaction.execute(
                "INSERT INTO messages(session_id, turn_id, role, content, created_at) VALUES (?1, ?2, 'user', ?3, strftime('%s','now'))",
                rusqlite::params![session_id, turn_id, prompt],
            )?;
            Ok(turn_id)
        })
        .map_err(|error| error.to_string())?;
    let mut sink = PrintingSink {
        events: Vec::new(),
        live_stdout,
        live_events,
        cancelled: Arc::new(AtomicBool::new(false)),
        cancellation: cancellation.clone(),
    };
    let run_result = agent.run_coding_loop(prompt, tools, 8, &mut sink);
    match run_result {
        Ok(_) => database.transactional(|transaction| {
            transaction.execute("UPDATE turns SET status = 'complete', error = NULL WHERE id = ?1", rusqlite::params![turn_id])?;
            for message in &agent.session.messages[previous + 1..] {
                transaction.execute("INSERT INTO messages(session_id, turn_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))", rusqlite::params![session_id, turn_id, role_name(&message.role), message.plain_text()])?;
            }
            journal_observability(transaction, session_id, turn_id, &sink.events, agent.session.context_plan.as_ref())?;
            Ok(())
        }),
        Err(error) => {
            let message = error.to_string();
            database.transactional(|transaction| {
                transaction.execute("UPDATE turns SET status = 'failed', error = ?1 WHERE id = ?2", rusqlite::params![message, turn_id])?;
                transaction.execute("INSERT INTO messages(session_id, turn_id, role, content, created_at) VALUES (?1, ?2, 'tool', ?3, strftime('%s','now'))", rusqlite::params![session_id, turn_id, format!("agent error: {message}")])?;
                journal_observability(transaction, session_id, turn_id, &sink.events, agent.session.context_plan.as_ref())?;
                Ok(())
            }).map_err(|journal_error| journal_error.to_string())?;
            return Err(message);
        }
    }.map_err(|error| error.to_string())?;
    Ok(RunOutcome {
        turn_id,
        events: sink.events,
    })
}

pub(crate) fn journal_observability(
    transaction: &rusqlite::Transaction<'_>,
    session_id: i64,
    turn_id: i64,
    events: &[agent_protocol::Event],
    context_plan: Option<&agent_context::ContextPlan>,
) -> rusqlite::Result<()> {
    let mut usage = UsageRecord::default();
    for (sequence, event) in events.iter().enumerate() {
        let (kind, payload) = event_record(event);
        agent_storage::Database::insert_event(
            transaction,
            session_id,
            turn_id,
            sequence as i64,
            kind,
            &payload,
        )?;
        if let agent_protocol::Event::UsageUpdated(value) = event {
            add_usage(&mut usage, value);
        }
    }
    if usage.total_tokens > 0 || usage.input_tokens > 0 || usage.output_tokens > 0 {
        agent_storage::Database::insert_usage(transaction, turn_id, &usage)?;
    }
    if let Some(plan) = context_plan {
        agent_storage::Database::insert_context(
            transaction,
            turn_id,
            &ContextRecord {
                budget: plan.budget,
                estimated_tokens: plan.estimated_tokens,
                fingerprint: plan.stable_prefix_fingerprint.clone(),
                selected_count: plan.segments.len(),
                dropped_count: plan.dropped.len(),
                truncated_count: plan.segments.iter().filter(|item| item.truncated).count(),
            },
        )?;
    }
    Ok(())
}

pub(crate) fn event_record(event: &agent_protocol::Event) -> (&'static str, String) {
    use agent_protocol::Event;
    match event {
        Event::ModelDelta(_) | Event::ReasoningSummaryDelta(_) => ("model_delta", "delta".into()),
        Event::ToolOutputDelta(_) | Event::McpToolResult(_) => {
            ("tool_output", "output omitted or redacted".into())
        }
        Event::UsageUpdated(value) => ("usage", value.clone()),
        Event::Error(value) => ("error", value.clone()),
        Event::ToolRequested(value) => ("tool_requested", value.clone()),
        Event::ToolStarted(value) => ("tool_started", value.clone()),
        Event::ToolCompleted(value) => ("tool_completed", value.clone()),
        Event::HookStarted(value) => ("hook_started", value.clone()),
        Event::HookResult(value) => ("hook_result", value.clone()),
        Event::McpToolDiscovered(value) => ("mcp_tool_discovered", value.clone()),
        Event::PatchProduced(value) => ("patch", value.clone()),
        Event::VerificationStarted(value) => ("verification_started", value.clone()),
        Event::VerificationResult(value) => ("verification_result", value.clone()),
        Event::PermissionRequested(value) => ("permission_requested", value.clone()),
        Event::ContextCompacted(value) => ("context_compacted", value.clone()),
        Event::SessionReset(value) => ("session_reset", value.clone()),
        Event::PermissionModeChanged(value) => ("permission_mode", value.clone()),
        Event::TurnStarted => ("turn_started", String::new()),
        Event::TurnCompleted => ("turn_completed", String::new()),
    }
}
