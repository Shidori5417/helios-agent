//! Runtime extension commands: plugins, skills, hooks, tools, subagents and runtime policy.

use super::*;

pub(crate) fn plugins_command(command: PluginCommand) -> Result<(), String> {
    let workspace = workspace_root()?;
    let requested_enable = matches!(&command, PluginCommand::Enable { .. });
    match command {
        PluginCommand::List => {
            let mut registry = PluginRegistry::discover(plugin_root(&workspace))
                .map_err(|error| error.to_string())?;
            for id in enabled_plugin_names(&config_path()?)? {
                let _ = registry.enable(&id);
            }
            for plugin in registry.plugins() {
                println!(
                    "{}\t{}\t{}",
                    plugin.manifest.name, plugin.manifest.version, plugin.enabled
                );
            }
        }
        PluginCommand::Info { id } => {
            let mut registry = PluginRegistry::discover(plugin_root(&workspace))
                .map_err(|error| error.to_string())?;
            for enabled in enabled_plugin_names(&config_path()?)? {
                let _ = registry.enable(&enabled);
            }
            let plugin = registry
                .plugins()
                .iter()
                .find(|plugin| plugin.manifest.name == id)
                .ok_or("plugin not found")?;
            println!(
                "{} version={} enabled={} capabilities={} hooks={}",
                plugin.manifest.name,
                plugin.manifest.version,
                plugin.enabled,
                plugin.manifest.capabilities.join(","),
                plugin.manifest.hooks.len()
            );
        }
        PluginCommand::Enable { id } | PluginCommand::Disable { id } => {
            let registry = PluginRegistry::discover(plugin_root(&workspace))
                .map_err(|error| error.to_string())?;
            if !registry
                .plugins()
                .iter()
                .any(|plugin| plugin.manifest.name == id)
            {
                return Err("plugin not found".into());
            }
            let path = config_path()?;
            set_enabled_plugin(&path, &id, requested_enable)?;
            println!(
                "plugin {} {}",
                id,
                if requested_enable {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
        PluginCommand::Reload => {
            let registry = PluginRegistry::discover(plugin_root(&workspace))
                .map_err(|error| error.to_string())?;
            println!("reloaded {} plugin(s)", registry.plugins().len());
        }
        PluginCommand::Verify {
            registry,
            trust_store,
            id,
        } => {
            let trust = load_trust_store(&trust_store).map_err(|error| error.to_string())?;
            let state = app_paths()?.registry_state;
            let index = load_signed_registry(&registry, &trust, state)
                .map_err(|error| error.to_string())?;
            let entry = index
                .payload
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .ok_or("plugin id not found")?;
            let manifest =
                fetch_registry_manifest(&registry, entry).map_err(|error| error.to_string())?;
            verify_registry_entry(entry, &manifest, &trust).map_err(|error| error.to_string())?;
            println!(
                "verified {} {} signer={} sequence={} expires_at={}",
                entry.id, entry.version, index.key_id, index.sequence, index.expires_at
            );
        }
        PluginCommand::Install {
            registry,
            trust_store,
            id,
        }
        | PluginCommand::Update {
            registry,
            trust_store,
            id,
        } => {
            let trust = load_trust_store(trust_store).map_err(|error| error.to_string())?;
            let installed = install_from_registry(&registry, &trust, &workspace, &id)
                .map_err(|error| error.to_string())?;
            println!("{}", installed.display());
        }
        PluginCommand::Uninstall { id } => {
            let moved = uninstall_exact(&workspace, &id).map_err(|error| error.to_string())?;
            println!("{}", moved.display());
        }
    }
    Ok(())
}

pub(crate) fn load_startup_skills(
    workspace: &Path,
    config_path: Option<&Path>,
    prompt: &str,
) -> Result<Vec<RetrievedMemory>, String> {
    let mut roots = vec![
        workspace.join("skills"),
        workspace.join(".helios/skills"),
        workspace.join(".agent/skills"),
    ];
    if let Ok(paths) = AppPaths::discover(workspace) {
        roots.push(
            paths
                .global_config
                .parent()
                .unwrap_or(workspace)
                .join("skills"),
        );
    }
    let mut loaded = Vec::new();
    let mut names = std::collections::HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let root_permissions = PermissionEngine::new(&root, PermissionMode::ReadOnly)
            .map_err(|error| error.to_string())?;
        let catalog =
            SkillCatalog::discover(&root, root_permissions).map_err(|error| error.to_string())?;
        for metadata in catalog.metadata() {
            if !names.insert(metadata.name.clone())
                || !skill_enabled(config_path.unwrap_or(Path::new("")), &metadata.name)?
                || !(metadata.auto_invoke || prompt.contains(&metadata.name))
            {
                continue;
            }
            let body = catalog
                .load_body(&metadata.name)
                .map_err(|error| error.to_string())?;
            let body = body.chars().take(32 * 1024).collect::<String>();
            loaded.push(RetrievedMemory {
                id: format!("skill:{}", metadata.name),
                key: format!("skill:{}", metadata.name),
                content: format!(
                    "[skill source={} risk={}]\n{}",
                    root.display(),
                    metadata.risk,
                    body
                ),
            });
            if loaded.len() >= 8 {
                return Ok(loaded);
            }
        }
    }
    Ok(loaded)
}

pub(crate) fn load_runtime_hooks(
    workspace: &std::path::Path,
    config_path: Option<&std::path::Path>,
    cancellation: CancellationToken,
    permission_mode: PermissionMode,
    env_allow: &[String],
    env_deny: &[String],
) -> Result<Option<Box<dyn HookRunner>>, String> {
    let plugins_root = plugin_root(workspace);
    if !plugins_root.is_dir() {
        return Ok(None);
    }
    let mut registry =
        PluginRegistry::discover(&plugins_root).map_err(|error| error.to_string())?;
    let enabled = config_path
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|value| {
            value
                .get("plugins")
                .and_then(|plugins| plugins.get("enabled"))
                .and_then(toml::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
        })
        .unwrap_or_default();
    for name in enabled {
        registry.enable(&name).map_err(|error| error.to_string())?;
    }
    registry.restrict_environment(env_allow, env_deny);
    if !registry.plugins().iter().any(|plugin| plugin.enabled) {
        return Ok(None);
    }
    let permissions =
        PermissionEngine::new(workspace, permission_mode).map_err(|error| error.to_string())?;
    Ok(Some(Box::new(RuntimeHooks {
        registry,
        workspace: workspace.to_path_buf(),
        permissions,
        cancellation,
    })))
}

pub(crate) fn plugin_root(workspace: &Path) -> PathBuf {
    AppPaths::discover(workspace)
        .map(|paths| {
            let primary = agent_plugins::default_plugin_root(&paths).to_path_buf();
            if primary.exists() {
                primary
            } else if paths.legacy_project_dir.join("plugins").exists() {
                paths.legacy_project_dir.join("plugins")
            } else {
                paths.plugins_dir
            }
        })
        .unwrap_or_else(|_| workspace.join(".helios/plugins"))
}

pub(crate) fn load_runtime_policy(path: &Path) -> Result<RuntimePolicy, String> {
    if !path.exists() {
        return Ok(RuntimePolicy::default());
    }
    let document: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid config: {error}"))?;
    let permissions = document.get("permissions");
    let mode = permissions
        .and_then(|value| value.get("mode"))
        .and_then(toml::Value::as_str)
        .unwrap_or("workspace-write");
    let mode = match mode {
        "read-only" | "readonly" => Ok(PermissionMode::ReadOnly),
        "workspace-write" | "workspace_write" => Ok(PermissionMode::WorkspaceWrite),
        other => Err(format!("invalid permissions.mode: {other}")),
    }?;
    let strings = |key: &str| -> Result<Vec<String>, String> {
        permissions
            .and_then(|value| value.get(key))
            .and_then(toml::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| format!("permissions.{key} must contain strings"))
                    .map(str::to_owned)
            })
            .collect()
    };
    let mut allowed_network_hosts = strings("allowed_network_hosts")?;
    for host in &mut allowed_network_hosts {
        *host = normalize_host(host).map_err(|error| error.to_string())?;
    }
    let env_allow = strings("env_allow")?;
    let env_deny = strings("env_deny")?;
    for name in env_allow.iter().chain(env_deny.iter()) {
        if !safe_env_name(name) || secret_env_name(name) {
            return Err(format!(
                "secret-like or invalid environment name is not allowed: {name}"
            ));
        }
    }
    Ok(RuntimePolicy {
        mode,
        allowed_network_hosts,
        env_allow,
        env_deny,
    })
}

pub(crate) fn disabled_tool_names(path: &Path) -> Result<Vec<String>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let document: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid config: {e}"))?;
    Ok(document
        .get("tools")
        .and_then(|value| value.get("disabled"))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

pub(crate) fn effective_disabled_tool_names_for_session(
    project: &Path,
    session_id: Option<i64>,
    db_path: Option<&Path>,
) -> Result<Vec<String>, String> {
    let paths = app_paths()?;
    let mut disabled = disabled_tool_names(&paths.global_config)?;
    let project_disabled = disabled_tool_names(project)?;
    let project_enabled = tool_name_list(project, "enabled")?;
    disabled.retain(|name| !project_enabled.iter().any(|enabled| enabled == name));
    for name in project_disabled {
        if !disabled.iter().any(|current| current == &name) {
            disabled.push(name);
        }
    }
    if let Some(session_id) = session_id {
        let db_path = db_path.unwrap_or(&paths.db_path);
        let db = Database::open(db_path).map_err(|error| error.to_string())?;
        if let Some(override_) = db
            .session_tool_override(session_id, &paths.workspace.display().to_string())
            .map_err(|error| error.to_string())?
        {
            disabled.retain(|name| !override_.enabled.iter().any(|enabled| enabled == name));
            for name in override_.disabled {
                if !disabled.iter().any(|current| current == &name) {
                    disabled.push(name);
                }
            }
        }
    }
    disabled.sort();
    Ok(disabled)
}

pub(crate) fn tool_name_list(path: &Path, key: &str) -> Result<Vec<String>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let document: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid config: {e}"))?;
    Ok(document
        .get("tools")
        .and_then(|value| value.get(key))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// Reports whether command and MCP processes are confined by the kernel.
/// Says why not when they are not, so a user can tell a policy gap from a
/// deliberate opt-out.
pub(crate) fn os_sandbox_status() -> String {
    if cfg!(not(target_os = "linux")) {
        return "unsupported: OS sandbox is Linux-only".into();
    }
    if env::var("HELIOS_OS_SANDBOX").is_ok_and(|value| value == "off") {
        return "disabled: HELIOS_OS_SANDBOX=off".into();
    }
    match agent_sandbox::landlock::status() {
        agent_sandbox::landlock::Status::Enforced { abi } => format!("landlock abi={abi}"),
        agent_sandbox::landlock::Status::Unavailable => {
            "unavailable: kernel has no Landlock; commands run unrestricted".into()
        }
    }
}

pub(crate) fn runtime_command(command: RuntimeCommand) -> Result<(), String> {
    if let RuntimeCommand::Status(args) = &command {
        let workspace = workspace_root()?;
        let config = config_path()?;
        let document = load_user_config(&config).ok();
        let config_value = config
            .exists()
            .then(|| fs::read_to_string(&config).ok())
            .flatten()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok());
        let paths = AppPaths::discover(&workspace).map_err(|e| e.to_string())?;
        let db_path = paths.db_path.clone();
        let sessions = if db_path.is_file() {
            agent_storage::Database::open(&db_path)
                .ok()
                .and_then(|db| db.list_sessions(&workspace.display().to_string()).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let count_dirs = |path: &Path| {
            fs::read_dir(path)
                .map(|entries| entries.filter_map(Result::ok).count())
                .unwrap_or(0)
        };
        let mcp_count = config_value
            .as_ref()
            .and_then(|value| value.get("mcp_server"))
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len);
        let lsp_count = config_value
            .as_ref()
            .and_then(|value| value.get("lsp_server"))
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len);
        let enabled_plugins = config_value
            .as_ref()
            .and_then(|value| value.get("plugins"))
            .and_then(|value| value.get("enabled"))
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len);
        let provider = document.as_ref().and_then(|value| value.provider.clone());
        let provider_env = provider
            .as_deref()
            .and_then(|name| match name {
                "openai" => Some("OPENAI_API_KEY"),
                "anthropic" => Some("ANTHROPIC_API_KEY"),
                "gemini" => Some("GEMINI_API_KEY"),
                "generic" => Some("OPENAI_API_KEY"),
                _ => None,
            })
            .map(|name| std::env::var_os(name).is_some())
            .unwrap_or(false);
        let reports = paths.reports_dir.clone();
        let latest_report = fs::read_dir(&reports)
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_file())
                    .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())
            })
            .map(|entry| entry.path().display().to_string());
        let payload = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "workspace": workspace.display().to_string(),
            "config": config.display().to_string(),
            "config_valid": load_user_config(&config).is_ok(),
            "db_present": db_path.is_file(),
            "provider_default": provider,
            "provider_model": config_value.as_ref().and_then(|value| value.get("model")).and_then(toml::Value::as_str),
            "auth_source": if provider_env { "env" } else { "absent" },
            "permissions": config_value.as_ref().and_then(|value| value.get("permissions")).and_then(|value| value.get("mode")).and_then(toml::Value::as_str).unwrap_or("read-only"),
            "network_hosts": config_value.as_ref().and_then(|value| value.get("permissions")).and_then(|value| value.get("allowed_network_hosts")).and_then(toml::Value::as_array).map_or(0, Vec::len),
            "secret_values_included": false,
            "sessions": {"active": sessions.iter().filter(|session| !session.archived && session.deleted_at.is_none()).count(), "archived": sessions.iter().filter(|session| session.archived).count(), "total": sessions.len()},
            "jobs": {"entries": count_dirs(&paths.jobs_dir)},
            "mcp": {"configured": mcp_count, "enabled": mcp_count},
            "lsp": {"configured": lsp_count, "enabled": lsp_count},
            "plugins": {"installed": count_dirs(&plugin_root(&workspace)), "enabled": enabled_plugins},
            "skills": {"workspace": count_dirs(&workspace.join("skills")) + count_dirs(&workspace.join(".agent/skills"))},
            "trust": {"store_present": paths.trust_store.is_file(), "registry_state_present": paths.registry_state.is_file()},
            "latest_report": latest_report,
        });
        if args.iter().any(|arg| arg == "--json") {
            println!("{}", payload);
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?
            );
        }
        return Ok(());
    }
    let (section, args) = match command {
        RuntimeCommand::Providers(args) => ("providers", args),
        RuntimeCommand::Permissions(args) => ("permissions", args),
        RuntimeCommand::Mcp(args) => ("mcp", args),
        RuntimeCommand::Lsp(args) => ("lsp", args),
        RuntimeCommand::Hooks(args) => ("hooks", args),
        RuntimeCommand::Skills(args) => ("skills", args),
        RuntimeCommand::Subagents(args) => ("subagents", args),
        RuntimeCommand::Tui(args) => ("tui", args),
        RuntimeCommand::Tools(args) => ("tools", args),
        RuntimeCommand::Status(_) => unreachable!(),
    };
    let action = args.first().map(String::as_str).unwrap_or("list");
    let path = config_path()?;
    if section == "mcp" || section == "lsp" {
        if action == "test" {
            return configured_server_test(section, &args, &path);
        }
        if section == "mcp" && matches!(action, "warm" | "discover" | "reload") {
            let id = value_after(&args[1..], "--id")?
                .or_else(|| args.get(1).cloned())
                .ok_or("mcp reload/warm requires exact --id")?;
            let result = mcp_discover_server(&id, &workspace_root()?, &path)?;
            return println_result(if action == "reload" {
                Ok(format!("{result} reloaded"))
            } else {
                Ok(result)
            });
        }
        if section == "mcp" && action == "auth" {
            return mcp_auth_command(&path);
        }
        if section == "mcp" && matches!(action, "login" | "logout") {
            let id = value_after(&args[1..], "--id")?
                .or_else(|| args.get(1).cloned())
                .ok_or("mcp login/logout requires an exact server id")?;
            if !safe_control_id(&id) {
                return Err("invalid server id".into());
            }
            return println_result(if action == "login" {
                mcp_login_command(&id, &workspace_root()?, &path)
            } else {
                mcp_logout_command(&id)
            });
        }
        return runtime_server_array_command(section, &args, &path);
    }
    if section == "permissions" {
        return runtime_permissions_command(&args, &path);
    }
    if section == "tui" {
        return tui_config_command(&args, &path);
    }
    if section == "tools" {
        return tools_config_command(&args, &path);
    }
    if matches!(section, "hooks" | "skills" | "subagents") {
        return runtime_extensions_command(section, &args, &path);
    }
    let mut document: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(&path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid config: {e}"))?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = document
        .as_table_mut()
        .ok_or("config root must be a table")?;
    if matches!(action, "list" | "show") {
        println!(
            "{}",
            table
                .get(section)
                .cloned()
                .unwrap_or(toml::Value::Table(Default::default()))
        );
        return Ok(());
    }
    if matches!(section, "skills" | "hooks") && action == "validate" {
        return Ok(());
    }
    let id = value_after(&args[1..], "--id")?.or_else(|| args.get(1).cloned());
    if matches!(action, "remove" | "enable" | "disable" | "show") && id.is_none() {
        return Err(format!("{section} {action} requires exact --id"));
    }
    if matches!(action, "add" | "set-default") && id.is_none() {
        return Err(format!("{section} {action} requires exact --id"));
    }
    if args
        .iter()
        .any(|arg| arg.contains("secret=") || arg.contains("token=") || arg.starts_with("sk-"))
    {
        return Err("secret literals are not accepted; use an environment-name reference".into());
    }
    let id = id.unwrap_or_else(|| "default".into());
    if !safe_control_id(&id) {
        return Err("invalid control-plane id".into());
    }
    let section_value = table
        .entry(section)
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let section_table = section_value
        .as_table_mut()
        .ok_or("config section must be a table")?;
    match action {
        "remove" => {
            section_table.remove(&id);
        }
        "enable" | "disable" => {
            section_table
                .entry(id)
                .or_insert_with(|| toml::Value::Table(Default::default()))
                .as_table_mut()
                .ok_or("entry must be table")?
                .insert("enabled".into(), toml::Value::Boolean(action == "enable"));
        }
        "set-default" => {
            if section != "providers" {
                return Err("set-default is only valid for providers".into());
            }
            table.insert("provider".into(), toml::Value::String(id));
        }
        "add" => {
            let mut entry = toml::map::Map::new();
            if section == "mcp" {
                let url = value_after(&args[1..], "--url")?;
                let command = value_after(&args[1..], "--command")?;
                if url.is_some() == command.is_some() {
                    return Err("mcp add requires exactly one of --url or --command".into());
                }
                if let Some(url) = url {
                    if !url.starts_with("https://") {
                        return Err("MCP HTTP requires https://".into());
                    }
                    entry.insert("url".into(), toml::Value::String(url));
                }
                if let Some(command) = command {
                    entry.insert("command".into(), toml::Value::String(command));
                    let args = args[1..]
                        .windows(2)
                        .filter(|pair| pair[0] == "--arg")
                        .map(|pair| toml::Value::String(pair[1].clone()))
                        .collect::<Vec<_>>();
                    if !args.is_empty() {
                        entry.insert("args".into(), toml::Value::Array(args));
                    }
                }
                if let Some(env_name) = value_after(&args[1..], "--auth-env")? {
                    if !env_name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
                    {
                        return Err("--auth-env must be an environment variable name".into());
                    }
                    entry.insert("auth_env".into(), toml::Value::String(env_name));
                }
            } else if section == "lsp" {
                for (flag, key) in [
                    ("--language", "language"),
                    ("--command", "command"),
                    ("--cwd", "cwd"),
                ] {
                    if let Some(value) = value_after(&args[1..], flag)? {
                        entry.insert(key.into(), toml::Value::String(value));
                    }
                }
                if !entry.contains_key("language") || !entry.contains_key("command") {
                    return Err("lsp add requires --language and --command".into());
                }
            } else {
                entry.insert("enabled".into(), toml::Value::Boolean(true));
            }
            section_table.insert(id, toml::Value::Table(entry));
        }
        _ => return Err(format!("unsupported {section} action: {action}")),
    }
    let bytes = toml::to_string_pretty(&document).map_err(|e| e.to_string())?;
    atomic_write(&path, bytes.as_bytes())
}

pub(crate) fn tools_config_command(args: &[String], path: &Path) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let workspace = workspace_root()?;
    let policy = load_runtime_policy(path).unwrap_or_default();
    let registry = ToolRegistry::new(&workspace, policy.mode).map_err(|e| e.to_string())?;
    let scope = value_after(&args[1..], "--scope")?.unwrap_or_else(|| "project".into());
    let session_id = value_after(&args[1..], "--session")?
        .map(|value| value.parse::<i64>())
        .transpose()
        .map_err(|_| "--session expects an integer id")?;
    if scope == "session" && session_id.is_none() {
        return Err("session tool scope requires --session ID".into());
    }
    if !matches!(scope.as_str(), "global" | "project" | "session") {
        return Err("tool scope must be global, project, or session".into());
    }
    let mutation_path = if scope == "global" {
        app_paths()?.global_config
    } else {
        path.to_path_buf()
    };
    let mut document: toml::Value = if mutation_path.exists() {
        toml::from_str(&fs::read_to_string(&mutation_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    if action == "list" || action == "info" {
        let disabled = effective_disabled_tool_names_for_session(path, session_id, None)?;
        let requested = args.get(1).cloned();
        for definition in registry
            .definitions()
            .into_iter()
            .filter(|tool| requested.as_deref().is_none_or(|name| name == tool.name))
        {
            println!(
                "{}\tenabled={}\t{}",
                definition.name,
                !disabled.iter().any(|name| name == &definition.name),
                definition.description
            );
        }
        return Ok(());
    }
    let id = value_after(&args[1..], "--id")?
        .or_else(|| args.get(1).cloned())
        .ok_or("tools action requires exact tool id")?;
    if registry.metadata(&id).is_none() {
        return Err("unknown tool".into());
    }
    if scope == "session" {
        let paths = app_paths()?;
        let db = Database::open(&paths.db_path).map_err(|error| error.to_string())?;
        let mut override_ = db
            .session_tool_override(
                session_id.expect("validated"),
                &workspace.display().to_string(),
            )
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        match action {
            "enable" => {
                override_.disabled.retain(|name| name != &id);
                if !override_.enabled.iter().any(|name| name == &id) {
                    override_.enabled.push(id);
                }
            }
            "disable" => {
                override_.enabled.retain(|name| name != &id);
                if !override_.disabled.iter().any(|name| name == &id) {
                    override_.disabled.push(id);
                }
            }
            _ => return Err(format!("unsupported tools action: {action}")),
        }
        override_.enabled.sort();
        override_.disabled.sort();
        if !db
            .set_session_tool_override(
                session_id.expect("validated"),
                &workspace.display().to_string(),
                &override_,
            )
            .map_err(|error| error.to_string())?
        {
            return Err("session not found in workspace".into());
        }
        return Ok(());
    }
    let table = document
        .as_table_mut()
        .ok_or("config root must be a table")?
        .entry("tools")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("tools config must be a table")?;
    let disabled = table
        .entry("disabled")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("tools.disabled must be an array")?;
    match action {
        "enable" => disabled.retain(|value| value.as_str() != Some(id.as_str())),
        "disable" => {
            if !disabled
                .iter()
                .any(|value| value.as_str() == Some(id.as_str()))
            {
                disabled.push(toml::Value::String(id.clone()));
            }
        }
        _ => return Err(format!("unsupported tools action: {action}")),
    }
    let enabled = table
        .entry("enabled")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("tools.enabled must be an array")?;
    match action {
        "enable" => {
            if !enabled
                .iter()
                .any(|value| value.as_str() == Some(id.as_str()))
            {
                enabled.push(toml::Value::String(id));
            }
        }
        "disable" => enabled.retain(|value| value.as_str() != Some(id.as_str())),
        _ => unreachable!(),
    }
    atomic_write(
        &mutation_path,
        toml::to_string_pretty(&document)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
}

pub(crate) fn enabled_plugin_names(path: &Path) -> Result<Vec<String>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let document: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid config: {e}"))?;
    Ok(document
        .get("plugins")
        .and_then(|value| value.get("enabled"))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

pub(crate) fn set_enabled_plugin(path: &Path, id: &str, enabled: bool) -> Result<(), String> {
    let mut document: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = document
        .as_table_mut()
        .ok_or("config root must be a table")?;
    let plugins = table
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("plugins config must be a table")?;
    let enabled_list = plugins
        .entry("enabled")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("plugins.enabled must be an array")?;
    enabled_list.retain(|value| value.as_str() != Some(id));
    if enabled {
        enabled_list.push(toml::Value::String(id.to_owned()));
    }
    atomic_write(
        path,
        toml::to_string_pretty(&document)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
}

pub(crate) fn parse_hook_event(name: &str) -> Result<HookEvent, String> {
    match name.to_ascii_lowercase().as_str() {
        "sessionstart" | "session_start" => Ok(HookEvent::SessionStart),
        "sessionend" | "session_end" => Ok(HookEvent::SessionEnd),
        "beforeprompt" | "before_prompt" => Ok(HookEvent::BeforePrompt),
        "userpromptsubmit" | "user_prompt_submit" => Ok(HookEvent::UserPromptSubmit),
        "beforetool" | "before_tool" => Ok(HookEvent::BeforeTool),
        "pretooluse" | "pre_tool_use" => Ok(HookEvent::PreToolUse),
        "aftertool" | "after_tool" => Ok(HookEvent::AfterTool),
        "posttooluse" | "post_tool_use" => Ok(HookEvent::PostToolUse),
        "preedit" | "pre_edit" => Ok(HookEvent::PreEdit),
        "beforeedit" | "before_edit" => Ok(HookEvent::BeforeEdit),
        "postedit" | "post_edit" => Ok(HookEvent::PostEdit),
        "afteredit" | "after_edit" => Ok(HookEvent::AfterEdit),
        "permissionrequest" | "permission_request" => Ok(HookEvent::PermissionRequest),
        "precompact" | "pre_compact" => Ok(HookEvent::PreCompact),
        "beforecompact" | "before_compact" => Ok(HookEvent::BeforeCompact),
        "aftercompact" | "after_compact" => Ok(HookEvent::AfterCompact),
        "agentfinished" | "agent_finished" => Ok(HookEvent::AgentFinished),
        "stop" => Ok(HookEvent::Stop),
        _ => Err(format!("unknown hook event: {name}")),
    }
}

pub(crate) fn skill_enabled(path: &Path, name: &str) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(true);
    }
    let document: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid config: {e}"))?;
    let skills = document.get("skills");
    if skills
        .and_then(|value| value.get("disabled"))
        .and_then(toml::Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(name)))
    {
        return Ok(false);
    }
    if let Some(enabled) = skills
        .and_then(|value| value.get("enabled"))
        .and_then(toml::Value::as_array)
    {
        return Ok(enabled.iter().any(|value| value.as_str() == Some(name)));
    }
    Ok(true)
}

pub(crate) fn set_skill_enabled(path: &Path, name: &str, enabled: bool) -> Result<(), String> {
    let mut document: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid config: {e}"))?
    } else {
        toml::Value::Table(Default::default())
    };
    let skills = document
        .as_table_mut()
        .ok_or("config root must be a table")?
        .entry("skills")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("skills config must be a table")?;
    for (key, present) in [("enabled", enabled), ("disabled", !enabled)] {
        let values = skills
            .entry(key)
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or("skills state must be an array")?;
        values.retain(|value| value.as_str() != Some(name));
        if present {
            values.push(toml::Value::String(name.to_owned()));
        }
    }
    atomic_write(
        path,
        toml::to_string_pretty(&document)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
}

pub(crate) fn skill_body(workspace: &Path, name: &str) -> Result<String, String> {
    let permissions = PermissionEngine::new(workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    for root in [
        workspace.join("skills"),
        workspace.join(".helios/skills"),
        workspace.join(".agent/skills"),
    ] {
        if !root.is_dir() {
            continue;
        }
        let catalog =
            SkillCatalog::discover(root, permissions.clone()).map_err(|error| error.to_string())?;
        if catalog.metadata().iter().any(|skill| skill.name == name) {
            let body = catalog.load_body(name).map_err(|error| error.to_string())?;
            if body.len() > 128 * 1024 {
                return Err("skill body exceeds 128 KiB".into());
            }
            return Ok(body);
        }
    }
    Err("skill not found".into())
}

pub(crate) fn runtime_extensions_command(
    section: &str,
    args: &[String],
    path: &Path,
) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let workspace = workspace_root()?;
    match section {
        "hooks" => {
            let mut registry =
                PluginRegistry::discover(plugin_root(&workspace)).map_err(|e| e.to_string())?;
            let enabled = enabled_plugin_names(path)?;
            for id in &enabled {
                registry.enable(id).map_err(|e| e.to_string())?;
            }
            match action {
                "list" => {
                    for plugin in registry.plugins() {
                        let events = plugin
                            .manifest
                            .hooks
                            .iter()
                            .map(|hook| format!("{:?}", hook.event))
                            .collect::<Vec<_>>()
                            .join(",");
                        println!(
                            "{}\tenabled={}\tevents={}",
                            plugin.manifest.name, plugin.enabled, events
                        );
                    }
                    Ok(())
                }
                "enable" | "disable" => {
                    let id = args
                        .get(1)
                        .or_else(|| {
                            args.iter()
                                .position(|arg| arg == "--id")
                                .and_then(|index| args.get(index + 1))
                        })
                        .ok_or("hooks command requires exact plugin id")?;
                    if !registry
                        .plugins()
                        .iter()
                        .any(|plugin| plugin.manifest.name == *id)
                    {
                        return Err("plugin not found".into());
                    }
                    set_enabled_plugin(path, id, action == "enable")
                }
                "test" => {
                    let id = args
                        .get(1)
                        .or_else(|| {
                            args.iter()
                                .position(|arg| arg == "--id")
                                .and_then(|index| args.get(index + 1))
                        })
                        .ok_or("hooks test requires plugin id")?;
                    let event_name =
                        value_after(args, "--event")?.ok_or("hooks test requires --event EVENT")?;
                    let event = parse_hook_event(&event_name)?;
                    if !registry
                        .plugins()
                        .iter()
                        .any(|plugin| plugin.manifest.name == *id)
                    {
                        return Err("plugin not found".into());
                    }
                    if !enabled.iter().any(|enabled_id| enabled_id == id) {
                        return Err("plugin is disabled; enable it before testing".into());
                    }
                    let policy = load_runtime_policy(path)?;
                    let permissions = PermissionEngine::new(&workspace, policy.mode)
                        .map_err(|e| e.to_string())?;
                    let cancellation = CancellationToken::default();
                    let output = registry
                        .execute_hook(id, event, &workspace, &permissions, &cancellation, 8 * 1024)
                        .map_err(|e| e.to_string())?;
                    println!(
                        "status={:?} truncated={} output={}",
                        output.status, output.truncated, output.output
                    );
                    Ok(())
                }
                "reload" => {
                    println!("reloaded {} hook plugin(s)", registry.plugins().len());
                    Ok(())
                }
                _ => Err(format!("unsupported hooks action: {action}")),
            }
        }
        "skills" => {
            let mut roots = vec![
                workspace.join("skills"),
                workspace.join(".helios/skills"),
                workspace.join(".agent/skills"),
            ];
            if let Ok(paths) = AppPaths::discover(&workspace) {
                roots.push(
                    paths
                        .global_config
                        .parent()
                        .unwrap_or(&workspace)
                        .join("skills"),
                );
            }
            if let Some(root) = value_after(args, "--root")? {
                roots = vec![workspace.join(root)];
            }
            let name = args.get(1).filter(|value| !value.starts_with('-')).cloned();
            let mut found = Vec::new();
            for root in roots {
                if root.exists() {
                    let permissions = PermissionEngine::new(&root, PermissionMode::ReadOnly)
                        .map_err(|e| e.to_string())?;
                    found.push(
                        SkillCatalog::discover(root, permissions.clone())
                            .map_err(|e| e.to_string())?,
                    );
                }
            }
            match action {
                "list" => {
                    let mut seen = std::collections::HashSet::new();
                    for catalog in &found {
                        for skill in catalog.metadata() {
                            if !seen.insert(skill.name.clone()) {
                                continue;
                            }
                            println!(
                                "{}\t{}\tenabled={}\tauto_invoke={}\trisk={}",
                                skill.name,
                                skill.description,
                                skill_enabled(path, &skill.name)?,
                                skill.auto_invoke,
                                skill.risk
                            );
                        }
                    }
                    Ok(())
                }
                "show" | "info" => {
                    let name = name.ok_or("skills show requires NAME")?;
                    for catalog in &found {
                        if let Ok(body) = catalog.load_body(&name) {
                            if body.len() > 128 * 1024 {
                                return Err("skill body exceeds 128 KiB".into());
                            }
                            println!("{body}");
                            return Ok(());
                        }
                    }
                    Err("skill not found".into())
                }
                "enable" | "disable" => {
                    let name = name.ok_or("skills enable/disable requires NAME")?;
                    if !found
                        .iter()
                        .any(|catalog| catalog.metadata().iter().any(|skill| skill.name == name))
                    {
                        return Err("skill not found".into());
                    }
                    set_skill_enabled(path, &name, action == "enable")
                }
                "reload" => {
                    println!("reloaded {} skill catalog(s)", found.len());
                    Ok(())
                }
                "validate" => {
                    let target = name;
                    if let Some(target) = target {
                        if !found.iter().any(|catalog| {
                            catalog.metadata().iter().any(|skill| skill.name == target)
                        }) {
                            return Err("skill not found".into());
                        }
                    }
                    println!("validated {} skill catalog(s)", found.len());
                    Ok(())
                }
                _ => Err(format!("unsupported skills action: {action}")),
            }
        }
        "subagents" => subagents_command(args, path, &workspace),
        _ => Err("unsupported extension section".into()),
    }
}

pub(crate) fn subagent_budgets(path: &Path) -> Result<SubagentBudgets, String> {
    let value: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = value.get("subagents");
    Ok(SubagentBudgets {
        max_iterations: table
            .and_then(|v| v.get("max_iterations"))
            .and_then(toml::Value::as_integer)
            .unwrap_or(8)
            .max(1) as u32,
        max_tokens: table
            .and_then(|v| v.get("max_tokens"))
            .and_then(toml::Value::as_integer)
            .unwrap_or(16_000)
            .max(1) as u64,
        max_tool_calls: table
            .and_then(|v| v.get("max_tool_calls"))
            .and_then(toml::Value::as_integer)
            .unwrap_or(32)
            .max(1) as u32,
        wall_clock: Duration::from_secs(
            table
                .and_then(|v| v.get("wall_clock_seconds"))
                .and_then(toml::Value::as_integer)
                .unwrap_or(60)
                .max(1) as u64,
        ),
    })
}

pub(crate) fn subagent_concurrency(path: &Path) -> Result<usize, String> {
    let value: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    Ok(value
        .get("subagents")
        .and_then(|v| v.get("concurrency"))
        .and_then(toml::Value::as_integer)
        .unwrap_or(1)
        .clamp(1, 16) as usize)
}

pub(crate) fn subagents_command(
    args: &[String],
    path: &Path,
    workspace: &Path,
) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("show");
    match action {
        "show" => println!("concurrency={} max_iterations={} max_tokens={} max_tool_calls={} wall_clock_seconds={}", subagent_concurrency(path)?, subagent_budgets(path)?.max_iterations, subagent_budgets(path)?.max_tokens, subagent_budgets(path)?.max_tool_calls, subagent_budgets(path)?.wall_clock.as_secs()),
        "config" => {
            let mut document: toml::Value = if path.exists() { toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())? } else { toml::Value::Table(Default::default()) };
            let table = document.as_table_mut().ok_or("config root must be a table")?.entry("subagents").or_insert_with(|| toml::Value::Table(Default::default())).as_table_mut().ok_or("subagents config must be a table")?;
            for (flag, key) in [("--concurrency", "concurrency"), ("--max-iterations", "max_iterations"), ("--max-tokens", "max_tokens"), ("--max-tool-calls", "max_tool_calls"), ("--wall-clock", "wall_clock_seconds")] { if let Some(value) = value_after(args, flag)? { table.insert(key.into(), toml::Value::Integer(value.parse().map_err(|_| format!("{flag} must be an integer"))?)); } }
            atomic_write(path, toml::to_string_pretty(&document).map_err(|e| e.to_string())?.as_bytes())?;
        }
        "run" | "test" => {
            let objective = if action == "test" { "complete offline subagent smoke".to_owned() } else { value_after(args, "--objective")?.ok_or("subagents run requires --objective TEXT")? };
            let configured = load_user_config(path)?;
            let explicit_provider = value_after(args, "--provider")?;
            let provider = explicit_provider.clone().or(configured.provider).unwrap_or_else(|| "fake".into());
            if provider != "fake" && !valid_provider(&provider) { return Err(format!("unknown provider: {provider}")); }
            let model = value_after(args, "--model")?.or(configured.model).unwrap_or_else(|| "offline-subagent".into());
            let base_url = value_after(args, "--base-url")?.or(configured.base_url);
            let scope = value_after(args, "--scope")?.map(|value| workspace.join(value)).unwrap_or_else(|| workspace.to_path_buf());
            let policy = load_runtime_policy(path)?;
            let mut context = Vec::new();
            if let Some(file) = value_after(args, "--context-file")? {
                let context_path = workspace.join(file).canonicalize().map_err(|e| e.to_string())?;
                if !context_path.starts_with(workspace) { return Err("context file escapes workspace".into()); }
                let content = fs::read_to_string(&context_path).map_err(|e| e.to_string())?;
                if content.len() > 64 * 1024 { return Err("context file exceeds 64 KiB".into()); }
                context.push(ContextSegment::new(SegmentKind::Source, Priority::Normal, Stability::Static, content));
            }
            if let Some(selector) = value_after(args, "--session")? {
                let db = open_database(&default_db_path()?)?;
                let id = if selector == "latest" { db.latest_session(&workspace.display().to_string()).map_err(|e| e.to_string())?.ok_or("no latest session")?.id } else { selector.parse::<i64>().map_err(|_| "--session expects an integer id or latest")? };
                for (role, content) in db.session_messages(id).map_err(|e| e.to_string())?.into_iter().take(32) {
                    context.push(ContextSegment::new(SegmentKind::Conversation, Priority::Normal, Stability::SessionStable, format!("{role}: {content}")));
                }
            }
            if let Some(query) = value_after(args, "--memory-query")? {
                let store = SqliteMemoryStore::open(default_db_path()?, 128, 16 * 1024).map_err(|e| e.to_string())?;
                for memory in store.search_scoped(&workspace.display().to_string(), &query, 8).map_err(|e| e.to_string())? {
                    context.push(ContextSegment::new(SegmentKind::Memory, Priority::Normal, Stability::Static, format!("[{}] {}", memory.entry.key, memory.entry.content)));
                }
            }
            let mut budgets = subagent_budgets(path)?;
            if let Some(value) = value_after(args, "--max-iterations")?.and_then(|v| v.parse().ok()) { budgets.max_iterations = value; }
            if let Some(value) = value_after(args, "--max-tokens")?.and_then(|v| v.parse().ok()) { budgets.max_tokens = value; }
            if let Some(value) = value_after(args, "--max-tool-calls")?.and_then(|v| v.parse().ok()) { budgets.max_tool_calls = value; }
            let mut task = TaskSpec::new("cli-subagent", objective).with_scope(vec![scope]).with_context(context).with_permission(policy.mode).with_budgets(budgets);
            let cancellation = SubagentCancellation::default();
            let keep_worktree = args.iter().any(|arg| arg == "--keep-worktree");
            let mut manager = None;
            let worktree = if args.iter().any(|arg| arg == "--worktree") {
                let temp_root = std::env::temp_dir().join("helios-agent-worktrees");
                let mut owned = WorktreeManager::new(workspace, &temp_root).map_err(|e| e.to_string())?;
                let handle = owned.create(value_after(args, "--ref")?.as_deref()).map_err(|e| e.to_string())?;
                task = owned.reroot_task(&task, &handle).map_err(|e| e.to_string())?;
                manager = Some((owned, handle.clone()));
                Some(handle.path)
            } else { None };
            let permission = PermissionEngine::new(worktree.as_deref().unwrap_or(workspace), policy.mode).map_err(|e| e.to_string())?;
            let executor = RealSubagentExecutor { provider, model, base_url };
            let results = Scheduler::new(subagent_concurrency(path)?).run(&permission, policy.mode, vec![task], &executor, cancellation).map_err(|e| e.to_string())?;
            let result = results.first().ok_or("subagent returned no result")?;
            println!("id={} reason={:?} iterations={} tokens={} tool_calls={} summary={}", result.id, result.stop_reason, result.iterations, result.tokens, result.tool_calls, result.summary);
            if let Some((mut manager, handle)) = manager {
                if keep_worktree { println!("worktree={}", handle.path.display()); } else { manager.cleanup(handle).map_err(|e| e.to_string())?; }
            }
        }
        _ => return Err(format!("unsupported subagents action: {action}")),
    }
    Ok(())
}

pub(crate) struct RealSubagentExecutor {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) base_url: Option<String>,
}

pub(crate) fn execute_subagent_provider<P: ModelProvider>(
    provider: P,
    task: &TaskSpec,
    prompt: &str,
    cancellation: &SubagentCancellation,
) -> TaskResult {
    let started = std::time::Instant::now();
    if cancellation.is_cancelled() {
        return failed_result(task, 0, 0, 0, StopReason::Cancelled);
    }
    let root = task
        .scope
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    let terminal_cancel = CancellationToken::default();
    let builtin = match ToolRegistry::new(&root, task.permission_mode) {
        Ok(registry) => registry.with_cancellation(terminal_cancel.clone()),
        Err(error) => return failed_subagent(task, error.to_string()),
    };
    let mut tools = match RuntimeExecutor::new(
        builtin,
        Vec::new(),
        &root,
        terminal_cancel,
        Vec::new(),
        None,
        task.permission_mode,
    ) {
        Ok(tools) => tools,
        Err(error) => return failed_subagent(task, error),
    };
    let mut agent = AgentLoop::new(provider)
        .with_context_budget(task.budgets.max_tokens)
        .with_system_prompt(system_prompt(
            include_str!("../../../prompts/SUBAGENT.md"),
            &root,
        ));
    let mut sink = SubagentSink {
        events: Vec::new(),
        cancellation,
    };
    let result = agent.run_coding_loop(
        prompt.to_owned(),
        &mut tools,
        task.budgets
            .max_iterations
            .min(task.budgets.max_tool_calls.max(1)) as usize,
        &mut sink,
    );
    let tokens = result
        .as_ref()
        .map(|text| text.len() as u64 / 4)
        .unwrap_or(0);
    let reason = if cancellation.is_cancelled() {
        StopReason::Cancelled
    } else if started.elapsed() > task.budgets.wall_clock {
        StopReason::WallClock
    } else if tokens > task.budgets.max_tokens {
        StopReason::TokenBudget
    } else if result.is_ok() {
        StopReason::Completed
    } else {
        StopReason::PermissionDenied
    };
    let tool_calls = sink
        .events
        .iter()
        .filter(|event| matches!(event, agent_protocol::Event::ToolRequested(_)))
        .count() as u32;
    failed_result(
        task,
        1,
        tokens.min(task.budgets.max_tokens),
        tool_calls,
        reason,
    )
    .with_summary(result.unwrap_or_else(|error| error.to_string()))
}

pub(crate) fn failed_subagent(task: &TaskSpec, error: String) -> TaskResult {
    failed_result(task, 0, 0, 0, StopReason::PermissionDenied).with_summary(error)
}

pub(crate) fn runtime_permissions_command(args: &[String], path: &Path) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("show");
    let mut document: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid config: {error}"))?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = document
        .as_table_mut()
        .ok_or("config root must be a table")?;
    if matches!(action, "show" | "list") {
        println!(
            "{}",
            table
                .get("permissions")
                .cloned()
                .unwrap_or(toml::Value::Table(Default::default()))
        );
        return Ok(());
    }
    if action == "set-mode" {
        let mode = value_after(&args[1..], "--mode")?
            .or_else(|| args.get(1).cloned())
            .ok_or("permissions set-mode requires read-only or workspace-write")?;
        if !matches!(
            mode.as_str(),
            "read-only" | "readonly" | "workspace-write" | "workspace_write"
        ) {
            return Err("permissions mode must be read-only or workspace-write".into());
        }
        let permissions = table
            .entry("permissions")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or("permissions config must be a table")?;
        permissions.insert("mode".into(), toml::Value::String(mode));
        let bytes = toml::to_string_pretty(&document).map_err(|error| error.to_string())?;
        return atomic_write(path, bytes.as_bytes());
    }
    let (key, remove) = match action {
        "add-network-host" => ("allowed_network_hosts", false),
        "remove-network-host" => ("allowed_network_hosts", true),
        "env-allow" => ("env_allow", false),
        "env-deny" => ("env_deny", false),
        _ => return Err(format!("unsupported permissions action: {action}")),
    };
    let value = value_after(&args[1..], "--value")?
        .or_else(|| args.get(1).cloned())
        .ok_or("permission mutation requires a value")?;
    let value = if key == "allowed_network_hosts" {
        normalize_host(&value).map_err(|error| error.to_string())?
    } else {
        if !safe_env_name(&value) || secret_env_name(&value) {
            return Err("invalid or secret-like environment name".into());
        }
        value
    };
    let permissions = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or("permissions config must be a table")?;
    let values = permissions
        .entry(key)
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("permission list must be an array")?;
    if remove {
        values.retain(|item| item.as_str() != Some(&value));
    } else if !values.iter().any(|item| item.as_str() == Some(&value)) {
        values.push(toml::Value::String(value));
    }
    let bytes = toml::to_string_pretty(&document).map_err(|error| error.to_string())?;
    atomic_write(path, bytes.as_bytes())
}

pub(crate) fn runtime_server_array_command(
    section: &str,
    args: &[String],
    path: &Path,
) -> Result<(), String> {
    let mut document: toml::Value = if path.exists() {
        toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = document
        .as_table_mut()
        .ok_or("config root must be a table")?;
    let key = if section == "mcp" {
        "mcp_server"
    } else {
        "lsp_server"
    };
    let servers = table
        .entry(key)
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("server config must be an array")?;
    let action = args.first().map(String::as_str).unwrap_or("list");
    let id = value_after(&args[1..], "--id")?.or_else(|| args.get(1).cloned());
    if matches!(action, "list" | "show") {
        println!("{}", toml::Value::Array(servers.clone()));
        return Ok(());
    }
    if action == "test" {
        return Err("external MCP/LSP tests are disabled in offline mode".into());
    }
    let id = id.ok_or("server command requires exact --id")?;
    if !safe_control_id(&id) {
        return Err("invalid server id".into());
    }
    let find =
        |value: &toml::Value| value.get("name").and_then(toml::Value::as_str) == Some(id.as_str());
    match action {
        "remove" => {
            if let Some(index) = servers.iter().position(find) {
                servers.remove(index);
            }
        }
        "enable" | "disable" => {
            let server = servers
                .iter_mut()
                .find(|value| find(value))
                .ok_or("server not found")?;
            server
                .as_table_mut()
                .ok_or("server must be table")?
                .insert("enabled".into(), toml::Value::Boolean(action == "enable"));
        }
        "add" => {
            if servers.iter().any(find) {
                return Err("server id already exists".into());
            }
            let mut entry = toml::map::Map::new();
            entry.insert("name".into(), toml::Value::String(id));
            entry.insert("enabled".into(), toml::Value::Boolean(true));
            if section == "mcp" {
                let url = value_after(&args[1..], "--url")?;
                let command = value_after(&args[1..], "--command")?;
                if url.is_some() == command.is_some() {
                    return Err("mcp add requires exactly one of --url or --command".into());
                }
                if let Some(url) = url {
                    if !url.starts_with("https://") {
                        return Err("MCP HTTP requires https://".into());
                    }
                    entry.insert("transport".into(), toml::Value::String("http".into()));
                    entry.insert("url".into(), toml::Value::String(url));
                }
                if let Some(command) = command {
                    entry.insert("command".into(), toml::Value::String(command));
                    let command_args = args[1..]
                        .windows(2)
                        .filter(|pair| pair[0] == "--arg")
                        .map(|pair| toml::Value::String(pair[1].clone()))
                        .collect::<Vec<_>>();
                    if !command_args.is_empty() {
                        entry.insert("args".into(), toml::Value::Array(command_args));
                    }
                }
                if let Some(auth) = value_after(&args[1..], "--auth-env")? {
                    if !safe_env_name(&auth) {
                        return Err("invalid auth env name".into());
                    }
                    entry.insert("auth_env".into(), toml::Value::String(auth));
                }
            } else {
                for (flag, key) in [
                    ("--language", "language"),
                    ("--command", "command"),
                    ("--cwd", "cwd"),
                ] {
                    if let Some(value) = value_after(&args[1..], flag)? {
                        entry.insert(key.into(), toml::Value::String(value));
                    }
                }
                if !entry.contains_key("language") || !entry.contains_key("command") {
                    return Err("lsp add requires --language and --command".into());
                }
            }
            servers.push(toml::Value::Table(entry));
        }
        _ => return Err(format!("unsupported {section} action: {action}")),
    }
    atomic_write(
        path,
        toml::to_string_pretty(&document)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
}
