use agent_context::{
    load_project_instructions, CompactionRequest, Compactor, ContextSegment, LocalCompactor,
    Priority, SegmentKind, Stability,
};
use agent_core::{
    AgentLoop, EventSink, ForegroundTaskController, HookRunner, LifecycleEvent, MemoryRetriever,
    RetrievedMemory, SteeringChannel, ToolExecution, ToolExecutor, ToolFailure,
};
use agent_lsp::{load_config as load_lsp_config, LspClient, LspServerConfig, StdioTransport};
use agent_mcp::{
    load_config as load_mcp_config, normalize_host, HostAllowlist, McpClient, PermissionPolicy,
};
use agent_memory::{MemoryEntry, MemoryStore, SqliteMemoryStore};
use agent_paths::AppPaths;
use agent_plugins::{
    fetch_registry_manifest, install_from_registry, load_signed_registry, load_trust_store,
    uninstall_exact, verify_registry_entry, verify_signed_registry, TrustStore,
};
use agent_plugins::{HookEvent, PluginRegistry};
use agent_protocol::{Message, Role};
use agent_providers::{
    profile_for, AnthropicProvider, FakeProvider, GeminiProvider, ModelProvider,
    OpenAiCompatibleProvider, OpenAiProvider, RequestTuning,
};
use agent_sandbox::PermissionMode;
use agent_sandbox::{PermissionClass, PermissionEngine};
use agent_skills::SkillCatalog;
use agent_storage::{ContextRecord, Database, SessionSummary, UsageRecord};
use agent_subagents::{
    Budgets as SubagentBudgets, CancellationToken as SubagentCancellation, Scheduler, StopReason,
    TaskResult, TaskSpec, WorktreeManager,
};
use agent_terminal::CancellationToken;
use agent_tools::{
    ApprovalBroker, ApprovalDecision, SnapshotEvent, SnapshotHook, ToolRegistry, ToolStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command as OsCommand, ExitCode, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
        Arc, Mutex,
    },
    time::Duration,
};
use zeroize::Zeroizing;

thread_local! {
    static WORKSPACE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Process-wide copy of `--workspace`, published once by `main` after
/// parsing. The thread-local above is only visible on the parsing thread, so
/// turns and tools running on worker threads (interactive mode, jobs) would
/// otherwise silently fall back to the process working directory.
mod args;
mod jobs;
mod ops;
mod runtime_ext;
mod sessions_db;
use args::*;
use jobs::*;
use ops::*;
use runtime_ext::*;
use sessions_db::*;

static PROCESS_WORKSPACE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[tokio::main]
pub async fn main() -> ExitCode {
    install_broken_pipe_hook();
    let program_name = env::args()
        .next()
        .and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "helios".into());
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "__job-supervisor") {
        return match run_job_supervisor(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        };
    }
    let parsed = parse_args(args);
    if let Some(path) = WORKSPACE_OVERRIDE.with(|slot| slot.borrow().clone()) {
        let _ = PROCESS_WORKSPACE.set(path);
    }
    match parsed {
        Ok(Command::Version) => {
            println!("{} {}", program_name, env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            print_help();
            ExitCode::SUCCESS
        }
        Ok(Command::Interactive) => match interactive_command() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Resume(selector)) => match interactive_command_with_resume(selector) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Models { json }) => match models_command(json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Review { focus, json }) => match review_command(focus, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::ContextHelp(path)) => {
            print_command_help(&path);
            ExitCode::SUCCESS
        }
        Ok(Command::Run(options)) => {
            let machine = options.output_format != OutputFormat::Text;
            match run_command(options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    if machine {
                        println!("{}", error_envelope(&error));
                    }
                    fail(error)
                }
            }
        }
        Ok(Command::Sessions { command, db_path }) => match sessions_command(command, db_path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Search { db_path, query }) => match search_sessions(db_path, &query) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Database(command)) => match database_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Audit(command)) => match audit_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Jobs(command)) => match jobs_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Tasks(command)) => match tasks_command(command, None) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Agents(command)) => match tasks_command(command, Some("subagent")) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Eval(command)) => match eval_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Perf(command)) => match perf_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::SelfCommand(command)) => match self_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Trust(command)) => match trust_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Registry(command)) => match registry_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Context {
            db_path,
            session,
            json,
        }) => match context_command(db_path, session, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Usage {
            db_path,
            session,
            json,
        }) => match usage_command(db_path, session, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Memory(command)) => match memory_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Plugins(command)) => match plugins_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Verify { argv }) => match verify_command(argv) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Diff) => match diff_command() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Init { force }) => match init_command(force) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Config(command)) => match config_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Auth(command)) => match auth_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Doctor { json }) => match doctor_command(json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Completion(command)) => match completion_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Runtime(command)) => match runtime_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Err(error) => {
            eprintln!("{error}\n");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn install_broken_pipe_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or_default();
        if message.contains("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(panic);
    }));
}
fn fail(error: String) -> ExitCode {
    eprintln!("helios error: {error}");
    ExitCode::from(1)
}

fn output_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn error_envelope(error: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "sequence": 0,
        "timestamp": output_timestamp(),
        "session_id": null,
        "turn_id": null,
        "kind": "error",
        "payload": {"status": "failed", "error": redact_output(error)},
    })
    .to_string()
}

fn redact_output(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if [
        "api_key",
        "apikey",
        "authorization",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return "[REDACTED]".into();
    }
    const MAX_OUTPUT: usize = 8 * 1024;
    if value.len() > MAX_OUTPUT {
        let prefix = value.chars().take(MAX_OUTPUT).collect::<String>();
        format!("{prefix}…[truncated]")
    } else {
        value.into()
    }
}

struct RuntimeExecutor {
    builtin: ToolRegistry,
    plugins: PluginRegistry,
    mcp: Vec<McpRuntime>,
    workspace: PathBuf,
    config_path: Option<PathBuf>,
    permission_mode: PermissionMode,
    cancellation: CancellationToken,
    lsp: Vec<LspRuntime>,
    permissions: PermissionEngine,
}

struct LspRuntime {
    config: LspServerConfig,
    workspace: PathBuf,
    client: Option<LspClient<StdioTransport>>,
}

struct McpRuntime {
    name: String,
    enabled: bool,
    config: agent_mcp::McpServerConfig,
    allowed_hosts: Vec<String>,
    network_allowed: bool,
    oauth: Option<agent_mcp::OAuthConfig>,
    advertised: Vec<String>,
    discovered_tools: Vec<agent_mcp::McpTool>,
    client: McpClient<HostAllowlist<PermissionPolicy>>,
    discovered: bool,
}

/// Child hook confining a stdio MCP server to the workspace (Linux, Landlock).
/// Servers are untrusted, so this applies in every permission mode; network
/// stays closed unless the server's config sets `allow_network = true`.
fn mcp_child_hook(workspace: &Path, network_allowed: bool) -> Option<agent_terminal::ChildHook> {
    if env::var("HELIOS_OS_SANDBOX").is_ok_and(|value| value == "off") {
        return None;
    }
    let sandbox = agent_sandbox::landlock::workspace_sandbox(workspace, &[], network_allowed)
        .ok()
        .flatten()?;
    let sandbox = std::sync::Arc::new(sandbox);
    Some(std::sync::Arc::new(move || sandbox.restrict_current()))
}

impl RuntimeExecutor {
    fn new(
        builtin: ToolRegistry,
        servers: Vec<agent_mcp::LoadedMcpServer>,
        workspace: &std::path::Path,
        cancellation: CancellationToken,
        lsp: Vec<LspServerConfig>,
        config_path: Option<PathBuf>,
        permission_mode: PermissionMode,
    ) -> Result<Self, String> {
        let mut mcp = Vec::new();
        for server in servers {
            let engine = PermissionEngine::new(workspace, permission_mode)
                .map_err(|error| error.to_string())?;
            let advertised = server.config.tool_include.clone();
            let allowed_hosts = server.allowed_hosts.clone();
            let network_allowed = server.network_allowed;
            let oauth = server.oauth.clone();
            let hook = mcp_child_hook(workspace, network_allowed);
            let policy = HostAllowlist {
                inner: PermissionPolicy::new(engine),
                allowed_hosts: server.allowed_hosts,
            };
            mcp.push(McpRuntime {
                name: server.config.name.clone(),
                enabled: server.config.enabled,
                config: server.config.clone(),
                allowed_hosts,
                network_allowed,
                oauth: oauth.clone(),
                client: McpClient::new(server.config, policy, 64 * 1024)
                    .with_child_hook(hook)
                    .with_oauth(oauth),
                advertised,
                discovered_tools: Vec::new(),
                discovered: false,
            });
        }
        let lsp = lsp
            .into_iter()
            .map(|config| LspRuntime {
                config,
                workspace: workspace.to_path_buf(),
                client: None,
            })
            .collect();
        let permissions =
            PermissionEngine::new(workspace, permission_mode).map_err(|error| error.to_string())?;
        let mut plugins =
            PluginRegistry::discover(plugin_root(workspace)).map_err(|error| error.to_string())?;
        let enabled_plugins = config_path
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|content| content.parse::<toml::Value>().ok())
            .and_then(|value| {
                value
                    .get("plugins")
                    .and_then(|p| p.get("enabled"))
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
        for name in enabled_plugins {
            plugins.enable(&name).map_err(|error| error.to_string())?;
        }
        Ok(Self {
            builtin,
            plugins,
            mcp,
            workspace: workspace.to_path_buf(),
            config_path,
            permission_mode,
            cancellation,
            lsp,
            permissions,
        })
    }
}

impl RuntimeExecutor {
    fn definitions(&self) -> Vec<agent_protocol::ToolDefinition> {
        let mut definitions = self.builtin.definitions();
        for server in self.mcp.iter().filter(|server| server.enabled) {
            if server.discovered_tools.is_empty() {
                for name in &server.advertised {
                    definitions.push(agent_protocol::ToolDefinition {
                        name: format!("mcp.{}.{}", server.name, name),
                        description: format!("MCP tool {name} from {}", server.name),
                        input_schema: serde_json::json!({"type":"object"}),
                    });
                }
            } else {
                for tool in &server.discovered_tools {
                    definitions.push(agent_protocol::ToolDefinition {
                        name: format!("mcp.{}.{}", server.name, tool.name),
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                    });
                }
            }
        }
        for server in self.lsp.iter().filter(|server| server.config.enabled) {
            for tool in ["symbols", "definition", "references", "diagnostics"] {
                definitions.push(agent_protocol::ToolDefinition {
                    name: format!("lsp:{}:{tool}", server.config.name),
                    description: format!("LSP {tool} from {}", server.config.name),
                    input_schema: serde_json::json!({"type":"object","required":["path"]}),
                });
            }
        }
        for (plugin, tool) in self.plugins.declarative_tools() {
            definitions.push(agent_protocol::ToolDefinition {
                name: format!("plugin.{}.{}", plugin.manifest.name, tool.name),
                description: format!("{} (plugin {})", tool.description, plugin.manifest.name),
                input_schema: tool.input_schema.clone(),
            });
        }
        for (plugin, command) in self.plugins.declarative_commands() {
            definitions.push(agent_protocol::ToolDefinition {
                name: format!("plugin.{}.{}", plugin.manifest.name, command.name),
                description: format!(
                    "{} (plugin command {})",
                    command.description, plugin.manifest.name
                ),
                input_schema: serde_json::json!({"type":"object"}),
            });
        }
        definitions
    }

    fn execute(&mut self, call: &agent_protocol::ToolCall) -> Result<ToolExecution, String> {
        if call.name.starts_with("lsp:") {
            return self.execute_lsp(call);
        }
        if let Some(rest) = call.name.strip_prefix("plugin.") {
            let (plugin_name, tool_name) = rest
                .split_once('.')
                .ok_or_else(|| "invalid plugin tool name".to_owned())?;
            let is_tool = self.plugins.declarative_tools().any(|(plugin, tool)| {
                plugin.manifest.name == plugin_name && tool.name == tool_name
            });
            let output = if is_tool {
                self.plugins.execute_tool(
                    plugin_name,
                    tool_name,
                    &call.arguments,
                    &self.workspace,
                    &self.permissions,
                    &self.cancellation,
                    64 * 1024,
                )
            } else {
                self.plugins.execute_command(
                    plugin_name,
                    tool_name,
                    &self.workspace,
                    &self.permissions,
                    &self.cancellation,
                    64 * 1024,
                )
            }
            .map_err(|error| error.to_string())?;
            return Ok(ToolExecution {
                status: format!("{:?}", output.status),
                output: output.output,
            });
        }
        let (server_name, tool_name) = if let Some(rest) = call.name.strip_prefix("mcp.") {
            rest.split_once('.')
                .ok_or_else(|| "invalid MCP tool name".to_owned())?
        } else if let Some(rest) = call.name.strip_prefix("mcp:") {
            rest.split_once(':')
                .ok_or_else(|| "invalid MCP tool name".to_owned())?
        } else {
            return <ToolRegistry as ToolExecutor>::execute(&mut self.builtin, call);
        };
        let needs_discovery = !self
            .mcp
            .iter()
            .find(|server| server.name == server_name)
            .ok_or_else(|| "MCP server is not configured".to_owned())?
            .discovered;
        if needs_discovery {
            self.reload_mcp(server_name)?;
        }
        let server = self
            .mcp
            .iter_mut()
            .find(|server| server.name == server_name)
            .ok_or_else(|| "MCP server is not configured".to_owned())?;
        let output = server
            .client
            .call_tool(tool_name, call.arguments.clone(), &self.cancellation)
            .map_err(|error| error.to_string())?;
        Ok(ToolExecution {
            status: "ok".into(),
            output: output.to_string(),
        })
    }

    fn reload_mcp(&mut self, id: &str) -> Result<Vec<String>, String> {
        let replacement = self
            .config_path
            .as_deref()
            .map(|path| load_mcp_config(path, &self.workspace))
            .transpose()
            .map_err(|error| error.to_string())?
            .and_then(|servers| servers.into_iter().find(|server| server.config.name == id));
        let server = self
            .mcp
            .iter_mut()
            .find(|server| server.name == id)
            .ok_or_else(|| "MCP server is not configured".to_owned())?;
        if let Some(replacement) = replacement {
            server.name = replacement.config.name.clone();
            server.enabled = replacement.config.enabled;
            server.config = replacement.config;
            server.allowed_hosts = replacement.allowed_hosts;
            server.network_allowed = replacement.network_allowed;
            server.oauth = replacement.oauth.clone();
        }
        let policy = HostAllowlist {
            inner: PermissionPolicy::new(
                PermissionEngine::new(&self.workspace, self.permission_mode)
                    .map_err(|error| error.to_string())?,
            ),
            allowed_hosts: server.allowed_hosts.clone(),
        };
        server.client = McpClient::new(server.config.clone(), policy, 64 * 1024)
            .with_child_hook(mcp_child_hook(&self.workspace, server.network_allowed))
            .with_oauth(server.oauth.clone());
        server.discovered = false;
        server.advertised.clear();
        server.discovered_tools.clear();
        if !server.enabled {
            return Err("MCP server is disabled".into());
        }
        server
            .client
            .initialize(&self.cancellation)
            .map_err(|error| error.to_string())?;
        let tools = server
            .client
            .discover_tools(&self.cancellation)
            .map_err(|error| error.to_string())?;
        let names = tools.iter().map(|tool| tool.name.clone()).collect();
        server.advertised = tools.iter().map(|tool| tool.name.clone()).collect();
        server.discovered_tools = tools;
        server.discovered = true;
        Ok(names)
    }
}

impl RuntimeExecutor {
    fn execute_lsp(&mut self, call: &agent_protocol::ToolCall) -> Result<ToolExecution, String> {
        if self.cancellation.is_cancelled() {
            return Err("LSP request cancelled".into());
        }
        let rest = call.name.strip_prefix("lsp:").ok_or("invalid LSP tool")?;
        let (server_name, operation) = rest.split_once(':').ok_or("invalid LSP tool name")?;
        let server = self
            .lsp
            .iter_mut()
            .find(|server| server.config.name == server_name && server.config.enabled)
            .ok_or("LSP server is not enabled")?;
        let relative = call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or("LSP tool requires path")?;
        let path = server
            .workspace
            .join(relative)
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !path.starts_with(&server.workspace) {
            return Err("LSP path escapes workspace".into());
        }
        self.permissions
            .authorize(agent_sandbox::PermissionClass::ReadOnly)
            .map_err(|error| error.to_string())?;
        let path = self
            .permissions
            .resolve_path(&path)
            .map_err(|error| error.to_string())?;
        if server.client.is_none() {
            // Language servers are untrusted processes: confine them to the workspace
            // with network denied, unless the sandbox is explicitly turned off.
            let hook = mcp_child_hook(&server.workspace, false);
            let mut client = LspClient::new(
                StdioTransport::spawn_with_hook(&server.config, &server.workspace, hook)
                    .map_err(|error| error.to_string())?,
                std::time::Duration::from_millis(server.config.timeout_ms),
            );
            client
                .initialize_with_cancellation(&server.workspace, Some(&self.cancellation))
                .map_err(|error| error.to_string())?;
            server.client = Some(client);
        }
        let client = server.client.as_mut().ok_or("LSP client unavailable")?;
        let uri = format!("file://{}", path.display());
        let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        client
            .did_open_with_cancellation(
                &uri,
                &server.config.language,
                &text,
                Some(&self.cancellation),
            )
            .map_err(|error| error.to_string())?;
        let line = call
            .arguments
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let character = call
            .arguments
            .get("character")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let result = match operation {
            "symbols" => client.symbols_with_cancellation(&uri, Some(&self.cancellation)),
            "definition" => {
                client.definition_with_cancellation(&uri, line, character, Some(&self.cancellation))
            }
            "references" => {
                client.references_with_cancellation(&uri, line, character, Some(&self.cancellation))
            }
            "diagnostics" => client.diagnostics_with_cancellation(&uri, Some(&self.cancellation)),
            _ => return Err("unknown LSP operation".into()),
        }
        .map_err(|error| error.to_string())?;
        let output = result.to_string();
        Ok(ToolExecution {
            status: "ok".into(),
            output: output.chars().take(64 * 1024).collect(),
        })
    }
}

impl ToolExecutor for RuntimeExecutor {
    fn definitions(&self) -> Vec<agent_protocol::ToolDefinition> {
        RuntimeExecutor::definitions(self)
    }

    fn execute(&mut self, call: &agent_protocol::ToolCall) -> Result<ToolExecution, String> {
        RuntimeExecutor::execute(self, call)
    }

    fn execute_typed(
        &mut self,
        call: &agent_protocol::ToolCall,
    ) -> Result<ToolExecution, ToolFailure> {
        self.execute(call).map_err(|message| ToolFailure {
            kind: if message.contains("permission") || message.contains("network") {
                agent_core::ToolFailureKind::PermissionDenied
            } else {
                agent_core::ToolFailureKind::Recoverable
            },
            message,
        })
    }
}

struct RuntimeHooks {
    registry: PluginRegistry,
    workspace: PathBuf,
    permissions: PermissionEngine,
    cancellation: CancellationToken,
}

impl HookRunner for RuntimeHooks {
    fn run(&mut self, event: LifecycleEvent) -> Result<String, String> {
        let event = match event {
            LifecycleEvent::SessionStart => HookEvent::SessionStart,
            LifecycleEvent::SessionEnd => HookEvent::SessionEnd,
            LifecycleEvent::BeforePrompt => HookEvent::BeforePrompt,
            LifecycleEvent::PreToolUse => HookEvent::PreToolUse,
            LifecycleEvent::BeforeTool => HookEvent::BeforeTool,
            LifecycleEvent::PostToolUse => HookEvent::PostToolUse,
            LifecycleEvent::AfterTool => HookEvent::AfterTool,
            LifecycleEvent::PreEdit => HookEvent::PreEdit,
            LifecycleEvent::BeforeEdit => HookEvent::BeforeEdit,
            LifecycleEvent::PostEdit => HookEvent::PostEdit,
            LifecycleEvent::AfterEdit => HookEvent::AfterEdit,
            LifecycleEvent::PreCompact => HookEvent::PreCompact,
            LifecycleEvent::BeforeCompact => HookEvent::BeforeCompact,
            LifecycleEvent::AfterCompact => HookEvent::AfterCompact,
            LifecycleEvent::AgentFinished => HookEvent::AgentFinished,
            LifecycleEvent::Stop => HookEvent::Stop,
        };
        let outputs = self
            .registry
            .execute_event(
                event,
                &self.workspace,
                &self.permissions,
                &self.cancellation,
                8 * 1024,
            )
            .map_err(|error| error.to_string())?;
        Ok(outputs
            .iter()
            .map(|output| format!("{:?}: {}", output.status, output.output))
            .filter(|output| !output.ends_with(": "))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn models_command(json: bool) -> Result<(), String> {
    let paths = app_paths()?;
    let config = load_user_config(paths.effective_project_config()).unwrap_or_default();
    let known = ["fake", "openai", "anthropic", "gemini", "generic"];
    let models = known
        .iter()
        .map(|provider| {
            serde_json::json!({
                "provider": provider,
                "configured": config.provider.as_deref() == Some(*provider),
                "model": if config.provider.as_deref() == Some(*provider) { config.model.clone() } else { None::<String> },
                "capabilities": provider_capability_summary(provider),
            })
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&models).map_err(|error| error.to_string())?
        );
    } else {
        for model in models {
            println!(
                "{} configured={} model={} {}",
                model["provider"].as_str().unwrap_or("unknown"),
                model["configured"],
                model["model"].as_str().unwrap_or("(built-in/default)"),
                model["capabilities"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

fn review_command(focus: Option<String>, json: bool) -> Result<(), String> {
    let focus = focus.unwrap_or_else(|| "general".into());
    if !matches!(
        focus.as_str(),
        "general" | "security" | "performance" | "tests"
    ) {
        return Err("review focus must be security, performance, or tests".into());
    }
    let paths = app_paths()?;
    let registry = ToolRegistry::new(&paths.workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    let diff = registry
        .git_diff_variant("worktree")
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "focus": focus,
                "read_only": true,
                "status": format!("{:?}", diff.status),
                "summary": diff.summary,
                "diff": diff.data,
            })
        );
    } else {
        println!("review focus={focus} (read-only)\n{}", diff.data);
    }
    Ok(())
}

fn interactive_command() -> Result<(), String> {
    interactive_command_with_resume(None)
}

fn interactive_command_with_resume(selector: Option<String>) -> Result<(), String> {
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
                                effective_disabled_tool_names_for_session(&worker_config, worker_session).unwrap_or_default(),
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

fn checkpoint_hash(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn rollback_checkpoint(
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

fn interactive_tui_context(paths: &AppPaths) -> agent_tui::TuiContext {
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
    let disabled =
        effective_disabled_tool_names_for_session(config_path, active_session).unwrap_or_default();
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

fn help_category(category: &str) -> &'static str {
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

fn provider_capability_summary(provider: &str) -> String {
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

fn git_state_snapshot(workspace: &Path) -> (Option<String>, Option<usize>) {
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

fn parse_direct_argv(input: &str) -> Result<Vec<String>, String> {
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

fn path_target_output(input: &str) -> String {
    let target = input.trim_start_matches('@');
    match workspace_root() {
        Ok(workspace) => {
            let path = workspace.join(target);
            format!("path={} exists={}", path.display(), path.exists())
        }
        Err(error) => format!("error: {error}"),
    }
}

fn slash_command_output(command: &str) -> Option<String> {
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

fn mcp_interactive_command(command: &str, workspace: &Path, config_path: &Path) -> String {
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

fn mcp_discover_server(id: &str, workspace: &Path, config_path: &Path) -> Result<String, String> {
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

fn println_result(result: Result<String, String>) -> Result<(), String> {
    println!("{}", result?);
    Ok(())
}

fn mcp_auth_command(config_path: &Path) -> Result<(), String> {
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

fn custom_prompt_command(command: &str, config_path: &Path) -> Option<String> {
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

fn custom_exec_command(
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

fn edit_external_prompt(initial: &str, workspace: &Path) -> Result<String, String> {
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

fn copy_last_text(target: &str, workspace: &Path) -> String {
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

/// Reads and validates `--image` files. Each must resolve inside the
/// workspace (so the model cannot be shown files outside it), be a regular
/// file of at most 5 MiB, and start with a PNG, JPEG, GIF or WebP signature.
fn load_run_images(
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
fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
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

fn run_command(options: RunOptions) -> Result<(), String> {
    run_command_with_cancellation(options, CancellationToken::default())
}

/// Resolve the provider-facing runtime settings once for every entry point.
/// Keeping this ahead of the interactive and headless paths makes the
/// precedence contract identical: project/global config, environment, then
/// explicit CLI/session values, with fake as the offline default.
fn resolve_runtime_options(
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

fn run_command_with_cancellation(
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
    let disabled_tools =
        effective_disabled_tool_names_for_session(&runtime_config_path, Some(session_id))?;
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

fn emit_headless_output(options: &RunOptions, session_id: i64, outcome: &RunOutcome) {
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
fn run_selected_provider(
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
fn system_prompt(overlay: &str, root: &Path) -> String {
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
fn run_provider<P: ModelProvider, E: ToolExecutor>(
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

fn journal_observability(
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

fn event_record(event: &agent_protocol::Event) -> (&'static str, String) {
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

fn current_workspace() -> Result<PathBuf, String> {
    workspace_root()
}

fn secret_arg(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token=",
        "secret=",
        "password=",
        "api_key=",
        "--token",
        "--secret",
        "--password",
        "sk-",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn argv_digest(argv: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    argv.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn report_path(workspace: &Path, kind: &str, explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    let path = explicit.unwrap_or_else(|| {
        app_paths()
            .map(|paths| paths.reports_dir.join(format!("{kind}-{}.json", job_id())))
            .unwrap_or_else(|_| workspace.join(format!(".helios/reports/{kind}-{}.json", job_id())))
    });
    if path.exists() {
        return Err("report output already exists; choose another path".into());
    }
    Ok(path)
}

fn embedded_suite(name: &str) -> Result<agent_evals::BenchmarkSuite, String> {
    let source = match name {
        "smoke" => include_str!("../../evals/fixtures/tasks.json").to_owned(),
        "benchmark-100" => include_str!("../../evals/fixtures/benchmark-100.json").to_owned(),
        path => fs::read_to_string(path).map_err(|error| error.to_string())?,
    };
    serde_json::from_str(&source).map_err(|error| format!("invalid eval suite: {error}"))
}

fn write_embedded_fixtures(root: &Path) -> Result<(), String> {
    for (name, files) in [
        ("inspect", vec![("README.txt", "before\n")]),
        ("patch", vec![("README.txt", "before\n")]),
        ("escape", vec![("README.txt", "before\n")]),
        ("evidence", vec![("README.txt", "evidence seed\n")]),
        ("multi", vec![("one.txt", "one\n"), ("two.txt", "two\n")]),
    ] {
        let dir = root.join(name);
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        for (file, content) in files {
            fs::write(dir.join(file), content).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn expand_user_path(path: PathBuf) -> Result<PathBuf, String> {
    if path == Path::new("~") || path.starts_with("~/") {
        let home = env::var_os("HOME").ok_or("HOME is not set")?;
        return Ok(PathBuf::from(home).join(path.strip_prefix("~").unwrap_or(Path::new(""))));
    }
    Ok(path)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_ .".contains(&byte))
}

fn read_public_key_file(path: &Path) -> Result<String, String> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = raw.trim();
    if value.contains("BEGIN")
        || value.contains("PRIVATE")
        || value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("public key file must contain exactly 32-byte Ed25519 public key hex; private/PEM material is rejected".into());
    }
    Ok(value.to_ascii_lowercase())
}

fn parse_role(role: &str) -> Result<Role, String> {
    match role {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(format!("unknown stored role: {other}")),
    }
}
fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn print_help() {
    println!("helios [run|exec] [OPTIONS] [PROMPT]\n\nNo command opens the interactive TTY session; use exec for headless automation.\n\nCommands:");
    for definition in command_registry() {
        println!(
            "  {:<12} {:<12} {}",
            definition.name, definition.arguments, definition.description
        );
    }
    println!("\nGlobal options: --workspace PATH --provider NAME --model NAME --base-url URL --db-path PATH --resume ID|latest --tui --output-format text|json|stream-json (exec only)\nUse `helios COMMAND ACTION --help` for command-specific syntax. Secrets are never displayed.");
}

fn print_command_help(path: &[String]) {
    let key = path.join(" ");
    let (usage, body) = match key.as_str() {
        "run" => ("helios run [OPTIONS] PROMPT".into(), "Runs one bounded agent turn; --provider defaults to fake and CLI flags override workspace config."),
        "exec" => ("helios exec [OPTIONS] PROMPT".into(), "Headless execution; --output-format is text, json, or stream-json. No TUI or alternate screen is initialized."),
        "resume" => ("helios resume [latest|ID|TITLE]".into(), "Resumes an exact workspace-scoped session in the interactive TTY; no selector opens the session browser."),
        "models" => ("helios models [--json]".into(), "Lists only local configured/known provider metadata; it never calls a provider or network."),
        "review" => ("helios review [security|performance|tests] [--json]".into(), "Produces a bounded read-only review of the current worktree diff."),
        "auth" => ("helios auth status|login|logout PROVIDER [--token-stdin]".into(), "login uses hidden TTY input or explicit non-TTY --token-stdin; token values are never arguments or output."),
        "auth login" => ("helios auth login PROVIDER [--token-stdin]".into(), "Stores the token in the OS keyring; plaintext fallback is refused."),
        "self" => ("helios self version|install|update|uninstall".into(), "Self-management is user-local and never uses sudo."),
        "self install" => ("helios self install [--prefix PATH] [--name NAME] [--compat-agent-cli]".into(), "Default prefix is ~/.local; installation is atomic and creates an identity marker."),
        "self update" => ("helios self update --from PATH --sha256 HEX".into(), "Local update requires an exact SHA-256; release update additionally requires external cosign verification."),
        "mcp" => ("helios mcp list|add|remove|enable|disable|show|test".into(), "MCP configuration is typed; HTTP requires explicit host/network policy."),
        "mcp add" => ("helios mcp add ID (--command CMD | --url HTTPS_URL) [--arg ARG] [--auth-env ENV]".into(), "Only argv and secret environment-name references are accepted; literal secrets and shell strings are rejected."),
        "permissions" => ("helios permissions show|set-mode|add-network-host|remove-network-host".into(), "Permission changes are atomic and feed the runtime policy."),
        "permissions set-mode" => ("helios permissions set-mode read-only|workspace-write".into(), "Changes the effective permission mode atomically."),
        "permissions add-network-host" => ("helios permissions add-network-host HOST".into(), "Adds one normalized exact host to the network allowlist."),
        "permissions remove-network-host" => ("helios permissions remove-network-host HOST".into(), "Removes one exact host from the network allowlist."),
        "permissions env-allow" => ("helios permissions env-allow NAME".into(), "Environment inheritance is allowlisted; secret-like names are rejected."),
        "permissions env-deny" => ("helios permissions env-deny NAME".into(), "Adds one environment name to the deny list; secret-like names remain denied."),
        "providers set-default" => ("helios providers set-default PROVIDER".into(), "Sets the configured provider profile; credentials remain in keyring or explicit environment fallback."),
        "completion install" => ("helios completion install SHELL [--path ABSOLUTE_PATH]".into(), "Installs completion atomically at an explicit absolute path."),
        "subagents" => ("helios subagents config|show|run|test".into(), "Subagents inherit permissions and use bounded scheduler budgets."),
        "subagents run" => ("helios subagents run --objective TEXT [--provider fake|openai|anthropic|gemini|generic] [--scope PATH] [--session ID|latest] [--context-file PATH] [--memory-query TEXT] [--worktree --ref REF]".into(), "Selected context is bounded; worktree results are never auto-applied to the parent tree."),
        "jobs" => ("helios jobs start|list|status|logs|cancel|wait|reap".into(), "Jobs use explicit argv, bounded logs, process-group cleanup, and workspace ownership checks."),
        "jobs start" => ("helios jobs start -- COMMAND ARGS...".into(), "Shell interpolation is not used; secret-like arguments are rejected."),
        "tasks" | "agents" => ("helios tasks bg|list|show|status|cancel|adopt [ID]".into(), "Background tasks are bounded, workspace-scoped, fake/offline by default, and never auto-applied."),
        "tasks bg" | "agents bg" => ("helios tasks bg PROMPT".into(), "Starts a durable isolated task and returns immediately."),
        "agents run" => ("helios agents run --objective TEXT [--scope PATH] [--max-iterations N] [--max-tokens N]".into(), "Runs a real bounded subagent through the existing Scheduler and configured provider; results remain isolated until explicitly adopted."),
        "tasks show" | "tasks status" | "agents show" | "agents status" => ("helios tasks show|status ID".into(), "Shows bounded task state and budget metadata."),
        "tasks cancel" | "agents cancel" => ("helios tasks cancel ID".into(), "Cancels only the exact owned process group."),
        "tasks adopt" | "agents adopt" => ("helios tasks adopt ID".into(), "Explicitly reads a bounded completed result; it does not mutate the foreground session."),
        "tui" => ("helios tui show|set|keys".into(), "TUI requires a TTY; tick, channel, scroll, and color settings are bounded."),
        "perf run" => ("helios perf run [--runs N] [--suite smoke|benchmark-100|PATH] [--output PATH] [--baseline PATH]".into(), "Runs offline startup, RSS, and optional benchmark measurements."),
        "status" => ("helios status [--json]".into(), "Shows bounded aggregate state without secret values."),
        _ => (generated_help_usage(path), "Arguments are typed and bounded; values are validated before mutation, and secrets are never displayed."),
    };
    println!("Usage: {usage}\n\n{body}");
}

fn generated_help_usage(path: &[String]) -> String {
    let base = format!("helios {}", path.join(" "));
    let action = path.get(1).map(String::as_str);
    let suffix = match action {
        Some("list" | "show" | "status" | "version" | "path" | "validate" | "keys") => "",
        Some("get") => " KEY",
        Some("set" | "set-default") => " KEY VALUE",
        Some("unset") => " KEY",
        Some(
            "add" | "enable" | "disable" | "revoke" | "remove" | "archive" | "unarchive" | "delete"
            | "cancel" | "wait" | "logs",
        ) => " ID [OPTIONS]",
        Some("generate") => " SHELL",
        Some("install") => " [OPTIONS]",
        Some("login" | "logout") => " PROVIDER [OPTIONS]",
        Some("test") => " ID [OPTIONS]",
        Some(
            "run" | "start" | "update" | "restore" | "backup" | "export" | "import" | "verify"
            | "reset" | "compare" | "create",
        ) => " [OPTIONS]",
        _ => " [OPTIONS]",
    };
    format!("{base}{suffix}")
}

fn workspace_root() -> Result<PathBuf, String> {
    if let Some(path) = WORKSPACE_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Ok(path);
    }
    if let Some(path) = PROCESS_WORKSPACE.get() {
        return Ok(path.clone());
    }
    env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())
}

fn app_paths() -> Result<AppPaths, String> {
    AppPaths::discover(workspace_root()?).map_err(|error| error.to_string())
}

fn resolve_workspace_file(path: &Path, workspace: &Path) -> Result<PathBuf, String> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let parent = candidate
        .parent()
        .ok_or("invalid workspace path")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let resolved = parent.join(candidate.file_name().ok_or("invalid workspace path")?);
    if !resolved.starts_with(workspace) {
        return Err("path escapes workspace".into());
    }
    Ok(resolved)
}

fn effective_config_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    Ok(app_paths()?.effective_project_config().to_path_buf())
}

fn config_path() -> Result<PathBuf, String> {
    Ok(app_paths()?.project_config)
}

fn init_command(force: bool) -> Result<(), String> {
    app_paths()?.ensure_dirs().map_err(|e| e.to_string())?;
    let path = config_path()?;
    fs::create_dir_all(path.parent().ok_or("invalid config path")?).map_err(|e| e.to_string())?;
    if path.exists() && !force {
        return Err(".helios/config.toml already exists; use --force to overwrite".into());
    }
    atomic_write(&path, b"# Helios Agent workspace configuration\n")?;
    println!("{}", path.display());
    Ok(())
}

fn valid_config_key(key: &str) -> bool {
    matches!(key, "provider" | "model" | "base_url")
}

fn load_user_config(path: &Path) -> Result<UserConfig, String> {
    let mut config = UserConfig::default();
    let mut candidates = Vec::new();
    if let Ok(paths) = app_paths() {
        candidates.push(paths.global_config);
        candidates.push(paths.legacy_config);
    }
    candidates.push(path.to_path_buf());
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let next: UserConfig =
            toml::from_str(&fs::read_to_string(&candidate).map_err(|e| e.to_string())?)
                .map_err(|e| format!("invalid config: {e}"))?;
        if next.provider.is_some() {
            config.provider = next.provider;
        }
        if next.model.is_some() {
            config.model = next.model;
        }
        if next.base_url.is_some() {
            config.base_url = next.base_url;
        }
    }
    if let Some(value) = env::var_os("HELIOS_PROVIDER") {
        config.provider = Some(value.to_string_lossy().into_owned());
    }
    if let Some(value) = env::var_os("HELIOS_MODEL") {
        config.model = Some(value.to_string_lossy().into_owned());
    }
    if let Some(value) = env::var_os("HELIOS_BASE_URL") {
        config.base_url = Some(value.to_string_lossy().into_owned());
    }
    Ok(config)
}

fn config_command(command: ConfigCommand) -> Result<(), String> {
    let path = config_path()?;
    match command {
        ConfigCommand::Path => println!("{}", path.display()),
        ConfigCommand::Validate => {
            let _ = load_user_config(&path)?;
            println!("valid");
        }
        ConfigCommand::List => {
            let config = load_user_config(&path)?;
            println!(
                "{}",
                toml::to_string_pretty(&config).map_err(|e| e.to_string())?
            );
        }
        ConfigCommand::Sources => {
            let paths = app_paths()?;
            for key in ["provider", "model", "base_url"] {
                let source = config_source(key, &path, &paths);
                println!("{key}\t{source}");
            }
        }
        ConfigCommand::Get { key } => {
            if !valid_config_key(&key) {
                return Err(format!("unknown config key: {key}"));
            }
            let config = load_user_config(&path)?;
            let value = match key.as_str() {
                "provider" => config.provider,
                "model" => config.model,
                "base_url" => config.base_url,
                _ => None,
            };
            if let Some(value) = value {
                println!("{value}");
            }
        }
        ConfigCommand::Set { key, value } => {
            if !valid_config_key(&key) {
                return Err(format!("unknown config key: {key}"));
            }
            if key == "provider" && !valid_provider(&value) {
                return Err("unknown provider".into());
            }
            let mut config = load_user_config(&path)?;
            match key.as_str() {
                "provider" => config.provider = Some(value),
                "model" => config.model = Some(value),
                "base_url" => config.base_url = Some(value),
                _ => unreachable!(),
            }
            let bytes = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
            atomic_write(&path, bytes.as_bytes())?;
        }
        ConfigCommand::Unset { key } => {
            if !valid_config_key(&key) {
                return Err(format!("unknown config key: {key}"));
            }
            let mut config = load_user_config(&path)?;
            match key.as_str() {
                "provider" => config.provider = None,
                "model" => config.model = None,
                "base_url" => config.base_url = None,
                _ => unreachable!(),
            }
            let bytes = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
            atomic_write(&path, bytes.as_bytes())?;
        }
    }
    Ok(())
}

fn config_source(key: &str, project: &Path, paths: &AppPaths) -> &'static str {
    let env_name = match key {
        "provider" => "HELIOS_PROVIDER",
        "model" => "HELIOS_MODEL",
        "base_url" => "HELIOS_BASE_URL",
        _ => return "unknown",
    };
    if env::var_os(env_name).is_some() {
        return "environment";
    }
    let contains_key = |path: &Path| -> bool {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
            .and_then(|value| value.get(key).cloned())
            .is_some()
    };
    if contains_key(project) {
        return "project";
    }
    if contains_key(&paths.legacy_config) {
        return "legacy-project";
    }
    if contains_key(&paths.global_config) {
        return "global";
    }
    "default"
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("invalid target path")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        e.to_string()
    })
}

fn provider_env(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        _ => "GENERIC_API_KEY",
    }
}
fn valid_provider(provider: &str) -> bool {
    matches!(provider, "openai" | "anthropic" | "gemini" | "generic")
}

const KEYRING_SERVICE: &str = "dev.helios-agent";

trait SecretStore {
    fn get(&self, provider: &str) -> Result<Option<String>, String>;
    fn set(&self, provider: &str, token: &str) -> Result<(), String>;
    fn delete(&self, provider: &str) -> Result<(), String>;
}
#[cfg(test)]
struct UnavailableSecretStore;
#[cfg(test)]
impl SecretStore for UnavailableSecretStore {
    fn get(&self, _provider: &str) -> Result<Option<String>, String> {
        Err("Secret Service/keyring is unavailable".into())
    }
    fn set(&self, _provider: &str, _token: &str) -> Result<(), String> {
        Err("Secret Service/keyring is unavailable; refusing plaintext fallback".into())
    }
    fn delete(&self, _provider: &str) -> Result<(), String> {
        Err("Secret Service/keyring is unavailable".into())
    }
}

struct KeyringSecretStore;
impl KeyringSecretStore {
    fn entry(provider: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, provider)
            .map_err(|_| "keyring backend is unavailable".to_owned())
    }
}
impl SecretStore for KeyringSecretStore {
    fn get(&self, provider: &str) -> Result<Option<String>, String> {
        match Self::entry(provider)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(keyring::Error::NoStorageAccess(_)) => {
                Err("keyring access denied or unavailable".into())
            }
            Err(_) => Err("keyring read failed".into()),
        }
    }
    fn set(&self, provider: &str, token: &str) -> Result<(), String> {
        Self::entry(provider)?
            .set_password(token)
            .map_err(|_| "keyring write failed".into())
    }
    fn delete(&self, provider: &str) -> Result<(), String> {
        match Self::entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(keyring::Error::NoStorageAccess(_)) => {
                Err("keyring access denied or unavailable".into())
            }
            Err(_) => Err("keyring delete failed".into()),
        }
    }
}

fn credential_for(provider: &str) -> Result<String, String> {
    let store = KeyringSecretStore;
    credential_from_store(&store, provider)
}

fn credential_from_store(store: &dyn SecretStore, provider: &str) -> Result<String, String> {
    match store.get(provider) {
        Ok(Some(value)) => Ok(value),
        Ok(None) | Err(_) => env::var(provider_env(provider)).map_err(|_| {
            format!("no credential available for {provider} (keyring or environment)")
        }),
    }
}

fn auth_command(command: AuthCommand) -> Result<(), String> {
    let store = KeyringSecretStore;
    match command {
        AuthCommand::Status { provider } => {
            let env_name = provider_env(&provider);
            let source = match store.get(&provider) {
                Ok(Some(_)) => "keyring",
                Ok(None) if env::var_os(env_name).is_some() => "env",
                Ok(None) => "absent",
                Err(_) if env::var_os(env_name).is_some() => "env",
                Err(_) => "keyring-unavailable",
            };
            println!("provider={provider} credential_source={source}");
            Ok(())
        }
        AuthCommand::Login {
            provider,
            token_stdin,
        } => {
            let token = if token_stdin {
                if io::stdin().is_terminal() {
                    return Err("--token-stdin requires non-TTY stdin".into());
                }
                let mut input = String::new();
                io::stdin()
                    .read_to_string(&mut input)
                    .map_err(|e| e.to_string())?;
                if input.lines().count() != 1 {
                    return Err("--token-stdin expects exactly one line".into());
                }
                Zeroizing::new(input.trim_end_matches(['\r', '\n']).to_owned())
            } else {
                if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                    return Err(
                        "interactive login requires a TTY; use --token-stdin for automation".into(),
                    );
                }
                Zeroizing::new(
                    rpassword::prompt_password("Token: ")
                        .map_err(|_| "could not read token".to_owned())?,
                )
            };
            if token.trim().is_empty() {
                return Err("empty token".into());
            }
            store.set(&provider, token.trim())
        }
        AuthCommand::Logout { provider } => store.delete(&provider),
    }
}

fn config_value_count(path: &Path, key: &str) -> usize {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| value.get(key).and_then(toml::Value::as_array).map(Vec::len))
        .unwrap_or(0)
}

fn command_available(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn tui_config_command(args: &[String], path: &Path) -> Result<(), String> {
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

fn load_tui_settings(path: &Path) -> Result<agent_tui::TuiSettings, String> {
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

impl agent_subagents::Executor for RealSubagentExecutor {
    fn execute(
        &self,
        task: &TaskSpec,
        cancellation: &SubagentCancellation,
        _active: &std::sync::atomic::AtomicUsize,
    ) -> TaskResult {
        let context = task
            .context
            .iter()
            .map(|segment| segment.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let context = context
            .chars()
            .take(task.budgets.max_tokens.saturating_mul(4) as usize)
            .collect::<String>();
        let prompt = if context.is_empty() {
            task.objective.clone()
        } else {
            format!("Context:\n{}\n\nObjective:\n{}", context, task.objective)
        };
        match self.provider.as_str() {
            "fake" => execute_subagent_provider(FakeProvider, task, &prompt, cancellation),
            "openai" => match credential_for("openai") {
                Ok(key) => execute_subagent_provider(
                    OpenAiProvider::with_credentials(
                        self.model.clone(),
                        key,
                        self.base_url
                            .clone()
                            .or_else(|| env::var("OPENAI_BASE_URL").ok())
                            .unwrap_or_else(|| "https://api.openai.com/v1/responses".into()),
                    )
                    .with_tuning(request_tuning(None)),
                    task,
                    &prompt,
                    cancellation,
                ),
                Err(error) => failed_subagent(task, error),
            },
            "anthropic" => match credential_for("anthropic") {
                Ok(key) => execute_subagent_provider(
                    AnthropicProvider::with_credentials(
                        self.model.clone(),
                        key,
                        self.base_url
                            .clone()
                            .or_else(|| env::var("ANTHROPIC_BASE_URL").ok())
                            .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".into()),
                    )
                    .with_tuning(request_tuning(None)),
                    task,
                    &prompt,
                    cancellation,
                ),
                Err(error) => failed_subagent(task, error),
            },
            "gemini" => match credential_for("gemini") {
                Ok(key) => execute_subagent_provider(
                    GeminiProvider::with_credentials(
                        self.model.clone(),
                        key,
                        self.base_url
                            .clone()
                            .or_else(|| env::var("GEMINI_BASE_URL").ok())
                            .unwrap_or_else(|| {
                                "https://generativelanguage.googleapis.com/v1beta/models".into()
                            }),
                    )
                    .with_tuning(request_tuning(None)),
                    task,
                    &prompt,
                    cancellation,
                ),
                Err(error) => failed_subagent(task, error),
            },
            _ => match credential_for("generic") {
                Ok(key) => execute_subagent_provider(
                    OpenAiCompatibleProvider::with_credentials(
                        self.model.clone(),
                        key,
                        self.base_url
                            .clone()
                            .or_else(|| env::var("GENERIC_BASE_URL").ok())
                            .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".into()),
                    )
                    .with_tuning(request_tuning(None)),
                    task,
                    &prompt,
                    cancellation,
                ),
                Err(error) => failed_subagent(task, error),
            },
        }
    }
}

struct SubagentSink<'a> {
    events: Vec<agent_protocol::Event>,
    cancellation: &'a SubagentCancellation,
}
impl agent_core::EventSink for SubagentSink<'_> {
    fn emit(&mut self, event: agent_protocol::Event) {
        self.events.push(event);
    }
    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

fn failed_result(
    task: &TaskSpec,
    iterations: u32,
    tokens: u64,
    calls: u32,
    reason: StopReason,
) -> TaskResult {
    TaskResult {
        id: task.id.clone(),
        summary: String::new(),
        iterations,
        tokens,
        tool_calls: calls,
        stop_reason: reason,
    }
}
trait TaskResultSummary {
    fn with_summary(self, summary: String) -> TaskResult;
}
impl TaskResultSummary for TaskResult {
    fn with_summary(mut self, summary: String) -> TaskResult {
        self.summary = summary.chars().take(4096).collect();
        self
    }
}

fn configured_server_test(section: &str, args: &[String], path: &Path) -> Result<(), String> {
    let id = value_after(&args[1..], "--id")?
        .or_else(|| args.get(1).cloned())
        .ok_or("test requires exact --id")?;
    let timeout_ms = value_after(&args[1..], "--timeout")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "--timeout must be milliseconds".to_string())
        })
        .transpose()?
        .unwrap_or(10_000)
        .clamp(1, 60_000);
    let workspace = workspace_root()?;
    if section == "mcp" {
        let mut server = load_mcp_config(path, &workspace)
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|server| server.config.name == id)
            .ok_or("MCP server not found")?;
        server.config.startup_timeout = Duration::from_millis(timeout_ms);
        server.config.tool_timeout = Duration::from_millis(timeout_ms);
        let allow_network = args.iter().any(|arg| arg == "--allow-network");
        if let agent_mcp::McpTransport::StreamableHttp { ref url, .. } = server.config.transport {
            let host = reqwest::Url::parse(url)
                .map_err(|error| error.to_string())?
                .host_str()
                .ok_or("MCP URL has no host")?
                .to_string();
            let normalized_host = normalize_host(&host).map_err(|error| error.to_string())?;
            if !allow_network {
                return Err("HTTP MCP test requires explicit --allow-network".into());
            }
            if !server
                .allowed_hosts
                .iter()
                .any(|allowed| allowed == &normalized_host)
            {
                return Err("HTTP MCP host is not in configured allowed_hosts".into());
            }
        } else if allow_network {
            return Err("--allow-network is only valid for HTTP MCP".into());
        }
        let engine = PermissionEngine::new(&workspace, PermissionMode::ReadOnly)
            .map_err(|error| error.to_string())?;
        let policy = HostAllowlist {
            inner: PermissionPolicy::new(engine),
            allowed_hosts: if allow_network {
                server.allowed_hosts.clone()
            } else {
                Vec::new()
            },
        };
        let cancellation = CancellationToken::default();
        let hook = mcp_child_hook(&workspace, server.network_allowed);
        let mut client = McpClient::new(server.config, policy, 64 * 1024)
            .with_child_hook(hook)
            .with_oauth(server.oauth.clone());
        client
            .initialize(&cancellation)
            .map_err(|error| error.to_string())?;
        let tools = client
            .discover_tools(&cancellation)
            .map_err(|error| error.to_string())?;
        println!(
            "mcp={} tools={}",
            id,
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .take(128)
                .collect::<Vec<_>>()
                .join(",")
        );
        return Ok(());
    }
    let mut server = load_lsp_config(path, &workspace)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|server| server.name == id)
        .ok_or("LSP server not found")?;
    server.timeout_ms = timeout_ms;
    let engine = PermissionEngine::new(&workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    let mut client = LspClient::new(
        StdioTransport::spawn(&server, &workspace).map_err(|error| error.to_string())?,
        Duration::from_millis(timeout_ms),
    );
    client
        .initialize(&workspace)
        .map_err(|error| error.to_string())?;
    if let Some(file) = value_after(&args[1..], "--file")? {
        let path = engine
            .resolve_path(workspace.join(&file))
            .map_err(|error| error.to_string())?;
        let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        if text.len() > 1024 * 1024 {
            return Err("LSP test file exceeds 1 MiB bound".into());
        }
        let uri = format!("file://{}", path.display());
        client
            .did_open(&uri, &server.language, &text)
            .map_err(|error| error.to_string())?;
        let result = client.symbols(&uri).map_err(|error| error.to_string())?;
        println!(
            "lsp={} symbols={}",
            id,
            serde_json::to_string(&result).map_err(|error| error.to_string())?
        );
    } else {
        println!("lsp={} initialized", id);
    }
    client.shutdown().map_err(|error| error.to_string())
}

fn safe_control_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn safe_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn secret_env_name(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|marker| upper.contains(marker))
}

struct PrintingSink {
    events: Vec<agent_protocol::Event>,
    live_stdout: bool,
    live_events: Option<SyncSender<agent_protocol::Event>>,
    cancelled: Arc<AtomicBool>,
    cancellation: CancellationToken,
}
impl Default for PrintingSink {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            live_stdout: false,
            live_events: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            cancellation: CancellationToken::default(),
        }
    }
}
impl EventSink for PrintingSink {
    fn emit(&mut self, event: agent_protocol::Event) {
        if self.live_stdout {
            if let agent_protocol::Event::ModelDelta(text) = &event {
                print!("{text}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
        if let Some(sender) = &self.live_events {
            if sender.send(event.clone()).is_err() {
                self.cancelled.store(true, Ordering::Release);
            }
        }
        self.events.push(event);
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.cancellation.is_cancelled()
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
