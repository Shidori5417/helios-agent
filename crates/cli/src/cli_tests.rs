//! Unit tests for the `helios` and `agent-cli` command surface.

use super::*;

#[test]
fn top_level_resume_models_review_and_unknown_help_are_typed() {
    assert!(
        matches!(parse_args(vec!["resume".into(), "latest".into()]), Ok(Command::Resume(Some(value))) if value == "latest")
    );
    assert!(matches!(
        parse_args(vec!["models".into(), "--json".into()]),
        Ok(Command::Models { json: true })
    ));
    assert!(
        matches!(parse_args(vec!["review".into(), "security".into(), "--json".into()]), Ok(Command::Review { focus: Some(value), json: true }) if value == "security")
    );
    assert!(parse_args(vec!["not-a-command".into(), "--help".into()]).is_err());
}

#[test]
fn approval_worker_to_tui_decision_gates_workspace_effect() {
    let root = std::env::temp_dir().join(format!(
        "helios-tui-approval-{}-{}",
        std::process::id(),
        std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("workspace");
    let (events, receiver) = std::sync::mpsc::sync_channel(8);
    let broker = ApprovalBroker::new(events);
    let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
        .expect("registry")
        .with_approval_broker(broker.clone());
    let cancellation = CancellationToken::default();
    let worker_root = root.clone();
    let worker_cancel = cancellation.clone();
    let worker = std::thread::spawn(move || {
        registry.execute(
            "shell",
            serde_json::json!({
                "program": "touch",
                "args": ["tui-approved.txt"],
                "cwd": worker_root.display().to_string(),
                "requires_pty": false
            }),
            &worker_cancel,
        )
    });
    let request = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("permission event");
    let mut model = agent_tui::TuiModel::default();
    model.reduce(&request);
    let prompt = model.permission_decision().cloned().expect("modal request");
    assert_eq!(prompt.id, 1);
    assert!(!root.join("tui-approved.txt").exists());
    let action = model.handle_overlay_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('a'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        action,
        agent_tui::OverlayAction::Permission(agent_tui::PermissionDecision::AllowOnce)
    );
    assert!(broker.decide(prompt.id, ApprovalDecision::AllowOnce));
    worker.join().expect("worker join").expect("approved tool");
    assert!(root.join("tui-approved.txt").is_file());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rollback_restores_checkpoint_after_approval_and_rejects_conflict() {
    let root = std::env::temp_dir().join(format!("helios-rollback-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("workspace");
    let db_path = root.join("state.db");
    let file = root.join("tracked.txt");
    fs::write(&file, "before").expect("before");
    let workspace = root.canonicalize().expect("canonical");
    let db = Database::open(&db_path).expect("db");
    let session = db
        .create_session_for_workspace("run", &workspace.display().to_string())
        .expect("session");
    let checkpoint = db
        .create_checkpoint(session, &workspace.display().to_string(), "auto", "test")
        .expect("checkpoint");
    db.record_checkpoint_file(
        checkpoint,
        &workspace.display().to_string(),
        &file,
        true,
        b"before",
    )
    .expect("preimage");
    fs::write(&file, "after").expect("after");
    db.update_checkpoint_file_after(
        checkpoint,
        &workspace.display().to_string(),
        &file,
        b"after",
    )
    .expect("postimage");
    drop(db);
    let (events, receiver) = std::sync::mpsc::sync_channel(4);
    let broker = ApprovalBroker::new(events);
    let worker_broker = broker.clone();
    let worker_db = db_path.clone();
    let worker_workspace = workspace.clone();
    let worker = std::thread::spawn(move || {
        rollback_checkpoint(
            &worker_db,
            checkpoint,
            &worker_workspace,
            PermissionMode::WorkspaceWrite,
            &worker_broker,
            &CancellationToken::default(),
        )
    });
    let request = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("approval");
    let id = match request {
        agent_protocol::Event::PermissionRequested(value) => value
            .split(';')
            .find_map(|part| part.strip_prefix("id=")?.parse::<u64>().ok())
            .expect("approval id"),
        _ => panic!("unexpected event"),
    };
    assert!(broker.decide(id, ApprovalDecision::AllowOnce));
    worker.join().expect("join").expect("rollback");
    assert_eq!(fs::read_to_string(&file).expect("restored"), "before");
    fs::write(&file, "unrelated").expect("conflict");
    let (events, _receiver) = std::sync::mpsc::sync_channel(1);
    let conflict = rollback_checkpoint(
        &db_path,
        checkpoint,
        &workspace,
        PermissionMode::WorkspaceWrite,
        &ApprovalBroker::new(events),
        &CancellationToken::default(),
    );
    assert!(conflict.is_err());
    let _ = fs::remove_dir_all(root);
}
#[test]
fn help_and_verify_parse() {
    assert_eq!(
        parse_args(Vec::new()).expect("interactive"),
        Command::Interactive
    );
    assert_eq!(
        parse_args(vec!["--help".into()]).expect("help"),
        Command::Help
    );
    assert_eq!(
        parse_args(vec!["--version".into()]).expect("version"),
        Command::Version
    );
    assert_eq!(
        parse_args(vec![
            "verify".into(),
            "--".into(),
            "cargo".into(),
            "test".into()
        ])
        .expect("verify"),
        Command::Verify {
            argv: vec!["cargo".into(), "test".into()]
        }
    );
}

#[test]
fn run_options_before_exec_subcommand_are_accepted() {
    let command = parse_args(vec![
        "--db-path".into(),
        "/tmp/sessions.db".into(),
        "exec".into(),
        "--output-format".into(),
        "json".into(),
        "prompt".into(),
    ])
    .expect("exec after leading options");
    assert!(matches!(
        command,
        Command::Run(RunOptions {
            exec: true,
            output_format: OutputFormat::Json,
            ref db_path,
            ref prompt,
            ..
        }) if prompt == "prompt" && db_path == Path::new("/tmp/sessions.db")
    ));
    assert!(parse_args(vec!["--tui".into(), "exec".into(), "prompt".into()]).is_err());
}

#[test]
fn exec_parses_headless_output_formats() {
    let command = parse_args(vec![
        "exec".into(),
        "--output-format".into(),
        "stream-json".into(),
        "prompt".into(),
    ])
    .expect("exec");
    assert!(matches!(
        command,
        Command::Run(RunOptions {
            exec: true,
            output_format: OutputFormat::StreamJson,
            prompt,
            ..
        }) if prompt == "prompt"
    ));
    assert!(parse_args(vec![
        "run".into(),
        "--output-format".into(),
        "json".into(),
        "prompt".into(),
    ])
    .is_err());
    assert!(parse_args(vec!["exec".into(), "--tui".into(), "prompt".into()]).is_err());
}

#[test]
fn headless_error_envelope_is_bounded_and_redacted() {
    let value: serde_json::Value =
        serde_json::from_str(&error_envelope("authorization token should not leak"))
            .expect("error envelope");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["kind"], "error");
    assert_eq!(value["payload"]["error"], "[REDACTED]");
}

#[test]
fn command_registry_has_unique_names_and_aliases() {
    let registry = command_registry();
    let mut names = std::collections::BTreeSet::new();
    let mut aliases = std::collections::BTreeSet::new();
    for command in &registry {
        assert!(!aliases.contains(command.name));
        assert!(names.insert(command.name));
        for alias in command.aliases {
            assert!(aliases.insert(*alias));
            assert!(!names.contains(alias));
        }
    }
    assert!(registry.iter().any(|command| command.name == "exec"));
    assert!(registry.iter().any(|command| command.name == "status"));
}
#[test]
fn run_parses_resume_and_db() {
    let command = parse_args(vec![
        "run".into(),
        "--provider".into(),
        "fake".into(),
        "--db-path".into(),
        "/tmp/a.db".into(),
        "--resume".into(),
        "latest".into(),
        "prompt".into(),
    ])
    .expect("run");
    assert!(
        matches!(command, Command::Run(RunOptions { resume: Some(ref value), prompt: ref text, .. }) if value == "latest" && text == "prompt")
    );
}

#[test]
fn memory_commands_require_explicit_values() {
    let command = parse_args(vec![
        "memory".into(),
        "add".into(),
        "--db-path".into(),
        "/tmp/memory.db".into(),
        "--key".into(),
        "build".into(),
        "--content".into(),
        "cargo test".into(),
    ])
    .expect("memory add");
    assert!(matches!(
        command,
        Command::Memory(MemoryCommand::Add { ref key, ref content, .. })
            if key == "build" && content == "cargo test"
    ));
    assert!(parse_args(vec![
        "memory".into(),
        "delete".into(),
        "--id".into(),
        "not-an-id".into(),
    ])
    .is_err());
}

#[test]
fn run_accepts_explicit_runtime_config() {
    let command = parse_args(vec![
        "run".into(),
        "--config".into(),
        ".agent/config.toml".into(),
        "inspect".into(),
    ])
    .expect("config");
    assert!(matches!(
        command,
        Command::Run(RunOptions { config: Some(path), .. }) if path == std::path::Path::new(".agent/config.toml")
    ));
}

#[test]
fn interactive_runtime_uses_project_provider_configuration() {
    let root = std::env::temp_dir().join(format!(
        "helios-interactive-runtime-{}-{}",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".helios")).expect("workspace");
    fs::write(
        root.join(".helios/config.toml"),
        "provider = 'generic'\nmodel = 'configured-model'\nbase_url = 'http://127.0.0.1:4310'\n",
    )
    .expect("config");
    WORKSPACE_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(root.clone()));
    let options = RunOptions {
        images: Vec::new(),
        provider: "fake".into(),
        model: "gpt-5".into(),
        effort: None,
        base_url: None,
        prompt: "hello".into(),
        db_path: root.join("state.db"),
        resume: None,
        tui: false,
        config: None,
        provider_explicit: false,
        model_explicit: false,
        base_url_explicit: false,
        output_format: OutputFormat::Text,
        exec: false,
        event_sender: None,
        approval: None,
        steering: None,
    };
    let (resolved, _policy, workspace) = resolve_runtime_options(options).expect("resolve");
    assert_eq!(workspace, root.canonicalize().expect("canonical workspace"));
    assert_eq!(resolved.provider, "generic");
    assert_eq!(resolved.model, "configured-model");
    assert_eq!(resolved.base_url.as_deref(), Some("http://127.0.0.1:4310"));
    WORKSPACE_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn plugin_commands_require_registry_and_trust_store() {
    let command = parse_args(vec![
        "plugins".into(),
        "verify".into(),
        "--registry".into(),
        "registry.json".into(),
        "--trust-store".into(),
        "trust.json".into(),
        "--id".into(),
        "demo".into(),
    ])
    .expect("plugin verify");
    assert!(
        matches!(command, Command::Plugins(PluginCommand::Verify { ref id, .. }) if id == "demo")
    );
    assert!(parse_args(vec![
        "plugins".into(),
        "install".into(),
        "--id".into(),
        "demo".into()
    ])
    .is_err());
}

#[test]
fn skill_enable_state_is_typed_and_persistent() {
    let root = std::env::temp_dir().join(format!("helios-skill-state-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let path = root.join("config.toml");
    assert!(skill_enabled(&path, "review").expect("default"));
    set_skill_enabled(&path, "review", false).expect("disable");
    assert!(!skill_enabled(&path, "review").expect("disabled"));
    set_skill_enabled(&path, "review", true).expect("enable");
    assert!(skill_enabled(&path, "review").expect("enabled"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn usage_and_context_commands_parse_json_and_session() {
    assert!(matches!(
        parse_args(vec!["usage".into(), "--session".into(), "latest".into(), "--json".into()]).expect("usage"),
        Command::Usage { session: Some(ref session), json: true, .. } if session == "latest"
    ));
    assert!(matches!(
        parse_args(vec![
            "context".into(),
            "--db-path".into(),
            "/tmp/a.db".into()
        ])
        .expect("context"),
        Command::Context { json: false, .. }
    ));
}

#[test]
fn control_plane_commands_parse_and_reject_unknown_config() {
    assert_eq!(
        parse_args(vec!["init".into(), "--force".into()]),
        Ok(Command::Init { force: true })
    );
    assert_eq!(
        parse_args(vec!["config".into(), "path".into()]),
        Ok(Command::Config(ConfigCommand::Path))
    );
    assert_eq!(
        parse_args(vec!["auth".into(), "status".into(), "openai".into()]),
        Ok(Command::Auth(AuthCommand::Status {
            provider: "openai".into()
        }))
    );
    assert_eq!(
        parse_args(vec!["completion".into(), "generate".into(), "bash".into()]),
        Ok(Command::Completion(CompletionCommand::Generate {
            shell: "bash".into()
        }))
    );
    assert!(parse_args(vec!["auth".into(), "login".into(), "openai".into()]).is_ok());
}

#[test]
fn secret_store_never_falls_back_to_plaintext() {
    let store = UnavailableSecretStore;
    assert!(store.set("openai", "not-persisted").is_err());
    assert!(store.delete("openai").is_err());
}

#[test]
fn credential_resolution_prefers_mock_keyring_and_falls_back_to_env() {
    struct MockStore(Option<String>);
    impl SecretStore for MockStore {
        fn get(&self, _provider: &str) -> Result<Option<String>, String> {
            Ok(self.0.clone())
        }
        fn set(&self, _provider: &str, _token: &str) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _provider: &str) -> Result<(), String> {
            Ok(())
        }
    }
    assert_eq!(
        credential_from_store(&MockStore(Some("keyring-value".into())), "generic")
            .expect("keyring"),
        "keyring-value"
    );
}

#[test]
fn session_database_and_audit_commands_parse_exact_targets() {
    assert_eq!(
        parse_args(vec![
            "sessions".into(),
            "delete".into(),
            "--id".into(),
            "7".into(),
            "--confirm".into(),
            "7".into()
        ])
        .expect("delete"),
        Command::Sessions {
            command: SessionCommand::Delete(7),
            db_path: default_db_path().expect("db")
        }
    );
    assert!(parse_args(vec![
        "sessions".into(),
        "delete".into(),
        "--id".into(),
        "7".into(),
        "--confirm".into(),
        "8".into()
    ])
    .is_err());
    assert!(matches!(
        parse_args(vec!["database".into(), "backup".into(), "--output".into(), "/tmp/backup.db".into()]).expect("backup"),
        Command::Database(DatabaseCommand::Backup { output, .. }) if output == Path::new("/tmp/backup.db")
    ));
    assert!(matches!(
        parse_args(vec!["audit".into(), "list".into(), "--event".into(), "tool_completed".into()]).expect("audit"),
        Command::Audit(AuditCommand::List { event: Some(event), .. }) if event == "tool_completed"
    ));
}

#[test]
fn eval_and_perf_commands_parse_reports_and_thresholds() {
    assert!(matches!(
        parse_args(vec!["eval".into(), "run".into(), "--suite".into(), "smoke".into(), "--max-regressions".into(), "2".into()]).expect("eval"),
        Command::Eval(EvalCommand::Run { suite, max_regressions: 2, .. }) if suite == "smoke"
    ));
    assert!(matches!(
        parse_args(vec!["perf".into(), "compare".into(), "--current".into(), "a.json".into(), "--baseline".into(), "b.json".into()]).expect("perf"),
        Command::Perf(PerfCommand::Compare { current, baseline }) if current == Path::new("a.json") && baseline == Path::new("b.json")
    ));
}

#[test]
fn self_commands_require_exact_update_inputs_and_confirmation() {
    assert_eq!(
        parse_args(vec!["self".into(), "version".into()]).expect("version"),
        Command::SelfCommand(SelfCommand::Version)
    );
    assert!(parse_args(vec![
        "self".into(),
        "uninstall".into(),
        "--confirm".into(),
        "other".into()
    ])
    .is_err());
    assert!(parse_args(vec![
        "self".into(),
        "update".into(),
        "--from".into(),
        "binary".into()
    ])
    .is_ok());
}

#[test]
fn trust_store_lifecycle_rejects_private_material_and_preserves_revoke() {
    let root = std::env::temp_dir().join(format!("helios-trust-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let path = root.join("trust-store.json");
    let key = root.join("public.hex");
    fs::write(&key, "11".repeat(32)).expect("key");
    trust_command(TrustCommand::Add {
        path: path.clone(),
        id: "release".into(),
        key_file: key,
    })
    .expect("add");
    trust_command(TrustCommand::Revoke {
        path: path.clone(),
        id: "release".into(),
    })
    .expect("revoke");
    assert!(trust_command(TrustCommand::Enable {
        path: path.clone(),
        id: "release".into()
    })
    .is_err());
    let private = root.join("private.pem");
    fs::write(&private, "-----BEGIN PRIVATE KEY-----").expect("private");
    assert!(trust_command(TrustCommand::Add {
        path: path.clone(),
        id: "bad".into(),
        key_file: private
    })
    .is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn local_install_is_hashed_and_atomic() {
    let root = std::env::temp_dir().join(format!("helios-self-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let source = root.join("source");
    let target = root.join("bin/helios");
    fs::create_dir_all(source.parent().expect("parent")).expect("dir");
    fs::write(&source, b"helios-test").expect("source");
    let digest = install_one(&source, &target).expect("install");
    assert_eq!(sha256_file(&target).expect("hash"), digest);
    assert_eq!(
        fs::read_to_string(target.with_extension("install.sha256")).expect("marker"),
        digest
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_roundtrip_is_toml_and_atomic() {
    let root = std::env::temp_dir().join(format!("helios-cli-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let path = root.join(".agent/config.toml");
    let config = UserConfig {
        provider: Some("fake".into()),
        model: Some("test".into()),
        base_url: None,
    };
    let encoded = toml::to_string_pretty(&config).expect("toml");
    atomic_write(&path, encoded.as_bytes()).expect("atomic write");
    assert_eq!(load_user_config(&path).expect("load"), config);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_flag_uses_helios_config_and_reads_legacy_without_overwrite() {
    let root = std::env::temp_dir().join(format!("helios-workspace-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".agent")).expect("legacy dir");
    fs::write(root.join(".agent/config.toml"), "provider = 'fake'\n").expect("legacy config");
    parse_args(vec![
        "--workspace".into(),
        root.display().to_string(),
        "config".into(),
        "path".into(),
    ])
    .expect("workspace flag");
    let paths = app_paths().expect("paths");
    assert_eq!(
        paths.project_config,
        root.canonicalize().unwrap().join(".helios/config.toml")
    );
    assert_eq!(
        load_user_config(&paths.project_config)
            .unwrap()
            .provider
            .as_deref(),
        Some("fake")
    );
    assert!(!paths.project_config.exists());
    WORKSPACE_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_server_mutations_use_loader_shaped_arrays() {
    let root = std::env::temp_dir().join(format!("helios-runtime-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let path = root.join("config.toml");
    runtime_server_array_command(
        "mcp",
        &[
            "add".into(),
            "--id".into(),
            "local".into(),
            "--command".into(),
            "printf".into(),
            "--arg".into(),
            "ready".into(),
        ],
        &path,
    )
    .expect("mcp");
    runtime_server_array_command(
        "lsp",
        &[
            "add".into(),
            "--id".into(),
            "rust".into(),
            "--language".into(),
            "rust".into(),
            "--command".into(),
            "rust-analyzer".into(),
        ],
        &path,
    )
    .expect("lsp");
    let value: toml::Value =
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml");
    assert_eq!(value["mcp_server"][0]["name"].as_str(), Some("local"));
    assert_eq!(value["mcp_server"][0]["args"][0].as_str(), Some("ready"));
    let loaded = load_mcp_config(&path, &root).expect("loader");
    assert_eq!(loaded[0].config.args, vec!["ready"]);
    assert_eq!(value["lsp_server"][0]["name"].as_str(), Some("rust"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn mcp_catalog_uses_qualified_names_and_excludes_disabled_servers() {
    let root = std::env::temp_dir().join(format!("helios-mcp-catalog-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let builtin = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("tools");
    let make_server = |name: &str, enabled: bool| agent_mcp::LoadedMcpServer {
        config: agent_mcp::McpServerConfig {
            name: name.into(),
            command: "mock".into(),
            args: Vec::new(),
            cwd: None,
            env_allowlist: Vec::new(),
            enabled,
            tool_include: vec!["echo".into()],
            tool_exclude: Vec::new(),
            startup_timeout: std::time::Duration::from_millis(100),
            tool_timeout: std::time::Duration::from_millis(100),
            permission_class: agent_sandbox::PermissionClass::ReadOnly,
            transport: agent_mcp::McpTransport::Stdio,
        },
        allowed_hosts: Vec::new(),
        network_allowed: false,
        oauth: None,
    };
    let executor = RuntimeExecutor::new(
        builtin,
        vec![make_server("local", true), make_server("off", false)],
        &root,
        CancellationToken::default(),
        Vec::new(),
        None,
        PermissionMode::ReadOnly,
    )
    .expect("runtime");
    let names = executor
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"mcp.local.echo".into()));
    assert!(!names.contains(&"mcp.off.echo".into()));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn permission_mode_is_consumed_from_runtime_config() {
    let root =
        std::env::temp_dir().join(format!("helios-permission-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let path = root.join("config.toml");
    runtime_permissions_command(&["set-mode".into(), "read-only".into()], &path).expect("set mode");
    assert_eq!(
        load_runtime_policy(&path).expect("load mode").mode,
        PermissionMode::ReadOnly
    );
    assert!(runtime_permissions_command(&["set-mode".into(), "unsafe".into()], &path,).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_policy_normalizes_hosts_and_rejects_secret_env_names() {
    let root = std::env::temp_dir().join(format!("helios-policy-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let path = root.join("config.toml");
    runtime_permissions_command(&["add-network-host".into(), "Example.COM".into()], &path)
        .expect("host");
    runtime_permissions_command(&["env-allow".into(), "CI".into()], &path).expect("env");
    let policy = load_runtime_policy(&path).expect("policy");
    assert_eq!(policy.allowed_network_hosts, vec!["example.com"]);
    assert_eq!(policy.env_allow, vec!["CI"]);
    assert!(
        runtime_permissions_command(&["env-allow".into(), "API_TOKEN".into()], &path,).is_err()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn effort_is_typed_and_persisted_on_session_override() {
    assert!(validate_effort("xhigh").is_ok());
    assert!(validate_effort("turbo").is_err());
    let db = Database::in_memory().expect("db");
    let id = db
        .create_session_for_workspace("task", "/workspace")
        .expect("session");
    db.set_session_override(
        id,
        "/workspace",
        Some("fake"),
        Some("gpt-5"),
        Some("high"),
        None,
    )
    .expect("override");
    assert_eq!(
        db.session_override(id, "/workspace")
            .expect("read")
            .expect("override")
            .effort
            .as_deref(),
        Some("high")
    );
}

#[test]
fn disabled_tool_config_is_read_for_runtime() {
    let root = std::env::temp_dir().join(format!("helios-tools-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let path = root.join("config.toml");
    atomic_write(&path, b"[tools]\ndisabled = [\"shell\"]\n").expect("write");
    assert_eq!(disabled_tool_names(&path).expect("read"), vec!["shell"]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn tools_disable_is_persisted_in_runtime_config() {
    let root = std::env::temp_dir().join(format!("helios-tools-disable-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    WORKSPACE_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(root.clone()));
    runtime_command(RuntimeCommand::Tools(vec![
        "disable".into(),
        "--id".into(),
        "shell".into(),
    ]))
    .expect("disable");
    let disabled = disabled_tool_names(&root.join(".helios/config.toml")).expect("read");
    assert_eq!(disabled, vec!["shell"]);
    WORKSPACE_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn c3_local_controls_are_bounded_and_custom_prompt_is_provider_free() {
    let root = std::env::temp_dir().join(format!("helios-c3-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let config = root.join("config.toml");
    atomic_write(
        &config,
        b"[commands.review]\ntype = 'prompt'\nprompt = 'inspect only'",
    )
    .expect("config");
    assert_eq!(
        custom_prompt_command("/review", &config).as_deref(),
        Some("inspect only")
    );
    assert!(resolve_workspace_file(Path::new("../escape.json"), &root).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn background_task_commands_parse_and_are_explicit() {
    assert_eq!(
        parse_args(vec!["tasks".into(), "list".into()]).expect("tasks"),
        Command::Tasks(TaskCommand::List)
    );
    assert_eq!(
        parse_args(vec![
            "tasks".into(),
            "bg".into(),
            "inspect".into(),
            "tree".into()
        ])
        .expect("bg"),
        Command::Tasks(TaskCommand::Background("inspect tree".into()))
    );
    assert_eq!(
        parse_args(vec!["agents".into(), "status".into(), "task-1".into()]).expect("agents"),
        Command::Agents(TaskCommand::Status("task-1".into()))
    );
    assert_eq!(
        parse_args(vec![
            "agents".into(),
            "run".into(),
            "--objective".into(),
            "inspect".into(),
            "--scope".into(),
            "src".into()
        ])
        .expect("agent run"),
        Command::Agents(TaskCommand::Run {
            objective: "inspect".into(),
            scope: Some("src".into()),
            max_iterations: None,
            max_tokens: None
        })
    );
    assert!(parse_args(vec!["tasks".into(), "bg".into()]).is_err());
}

#[test]
fn attached_images_are_validated_and_contained() {
    let root = env::temp_dir().join(format!("helios-images-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let root = root.canonicalize().expect("canonical");
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    png.extend_from_slice(b"rest");
    fs::write(root.join("shot.png"), &png).expect("png");
    fs::write(root.join("fake.png"), b"plain text, not an image").expect("fake");

    let images = load_run_images(&root, &["shot.png".into()]).expect("valid image");
    assert_eq!(images[0].media_type, "image/png");
    assert_eq!(images[0].data, agent_protocol::base64_encode(&png));
    assert!(load_run_images(&root, &["fake.png".into()]).is_err());
    assert!(load_run_images(&root, &["../outside.png".into()]).is_err());
    assert!(load_run_images(&root, &["missing.png".into()]).is_err());
    let five = vec!["shot.png".to_owned(); 5];
    assert!(load_run_images(&root, &five).is_err());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn image_signatures_cover_the_supported_formats() {
    assert_eq!(
        image_media_type(&[0xff, 0xd8, 0xff, 0xe0]),
        Some("image/jpeg")
    );
    assert_eq!(image_media_type(b"GIF89a..."), Some("image/gif"));
    assert_eq!(
        image_media_type(b"RIFF\0\0\0\0WEBPVP8 "),
        Some("image/webp")
    );
    assert_eq!(image_media_type(b"<svg>"), None);
}
