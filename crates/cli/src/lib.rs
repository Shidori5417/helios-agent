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
mod helpers;
mod interactive;
mod jobs;
mod ops;
mod run;
mod runtime_ext;
mod sessions_db;
use args::*;
use helpers::*;
use interactive::*;
use jobs::*;
use ops::*;
use run::*;
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

fn current_workspace() -> Result<PathBuf, String> {
    workspace_root()
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
