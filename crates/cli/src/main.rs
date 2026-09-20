use agent_context::{
    CompactionRequest, Compactor, ContextSegment, LocalCompactor, Priority, SegmentKind, Stability,
};
use agent_core::{
    AgentLoop, EventSink, HookRunner, LifecycleEvent, MemoryRetriever, RetrievedMemory,
    ToolExecution, ToolExecutor, ToolFailure,
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
    anthropic_profile, gemini_profile, generic_profile, openai_profile, AnthropicProvider,
    FakeProvider, GeminiProvider, ModelProvider, OpenAiCompatibleProvider, OpenAiProvider,
};
use agent_sandbox::PermissionEngine;
use agent_sandbox::PermissionMode;
use agent_skills::SkillCatalog;
use agent_storage::{ContextRecord, Database, SessionSummary, UsageRecord};
use agent_subagents::{
    Budgets as SubagentBudgets, CancellationToken as SubagentCancellation, Scheduler, StopReason,
    TaskResult, TaskSpec, WorktreeManager,
};
use agent_terminal::CancellationToken;
use agent_tools::{ApprovalBroker, ApprovalDecision, ToolRegistry, ToolStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, IsTerminal, Read},
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

#[tokio::main]
async fn main() -> ExitCode {
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
    match parse_args(args) {
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
        Ok(Command::Doctor) => match doctor_command() {
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
    mcp: Vec<McpRuntime>,
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
    advertised: Vec<String>,
    client: McpClient<HostAllowlist<PermissionPolicy>>,
    discovered: bool,
}

impl RuntimeExecutor {
    fn new(
        builtin: ToolRegistry,
        servers: Vec<agent_mcp::LoadedMcpServer>,
        workspace: &std::path::Path,
        cancellation: CancellationToken,
        lsp: Vec<LspServerConfig>,
        permission_mode: PermissionMode,
    ) -> Result<Self, String> {
        let mut mcp = Vec::new();
        for server in servers {
            let engine = PermissionEngine::new(workspace, permission_mode)
                .map_err(|error| error.to_string())?;
            let advertised = server.config.tool_include.clone();
            let policy = HostAllowlist {
                inner: PermissionPolicy::new(engine),
                allowed_hosts: server.allowed_hosts,
            };
            mcp.push(McpRuntime {
                name: server.config.name.clone(),
                client: McpClient::new(server.config, policy, 64 * 1024),
                advertised,
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
        Ok(Self {
            builtin,
            mcp,
            cancellation,
            lsp,
            permissions,
        })
    }
}

impl RuntimeExecutor {
    fn definitions(&self) -> Vec<agent_protocol::ToolDefinition> {
        let mut definitions = self.builtin.definitions();
        for server in &self.mcp {
            for name in &server.advertised {
                definitions.push(agent_protocol::ToolDefinition {
                    name: format!("mcp:{}:{name}", server.name),
                    description: format!("MCP tool {name} from {}", server.name),
                    input_schema: serde_json::json!({"type":"object"}),
                });
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
        definitions
    }

    fn execute(&mut self, call: &agent_protocol::ToolCall) -> Result<ToolExecution, String> {
        if call.name.starts_with("lsp:") {
            return self.execute_lsp(call);
        }
        let Some(rest) = call.name.strip_prefix("mcp:") else {
            return <ToolRegistry as ToolExecutor>::execute(&mut self.builtin, call);
        };
        let (server_name, tool_name) = rest
            .split_once(':')
            .ok_or_else(|| "invalid MCP tool name".to_owned())?;
        let server = self
            .mcp
            .iter_mut()
            .find(|server| server.name == server_name)
            .ok_or_else(|| "MCP server is not configured".to_owned())?;
        if !server.discovered {
            server
                .client
                .initialize(&self.cancellation)
                .map_err(|error| error.to_string())?;
            server
                .client
                .discover_tools(&self.cancellation)
                .map_err(|error| error.to_string())?;
            server.discovered = true;
        }
        let output = server
            .client
            .call_tool(tool_name, call.arguments.clone(), &self.cancellation)
            .map_err(|error| error.to_string())?;
        Ok(ToolExecution {
            status: "ok".into(),
            output: output.to_string(),
        })
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
            let mut client = LspClient::new(
                StdioTransport::spawn(&server.config, &server.workspace)
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
            LifecycleEvent::PreToolUse => HookEvent::PreToolUse,
            LifecycleEvent::PostToolUse => HookEvent::PostToolUse,
            LifecycleEvent::PreEdit => HookEvent::PreEdit,
            LifecycleEvent::PostEdit => HookEvent::PostEdit,
            LifecycleEvent::PreCompact => HookEvent::PreCompact,
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

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Version,
    Help,
    Interactive,
    ContextHelp(Vec<String>),
    Run(RunOptions),
    Sessions {
        command: SessionCommand,
        db_path: PathBuf,
    },
    Database(DatabaseCommand),
    Audit(AuditCommand),
    Jobs(JobCommand),
    Eval(EvalCommand),
    Perf(PerfCommand),
    SelfCommand(SelfCommand),
    Trust(TrustCommand),
    Registry(RegistryCommand),
    Search {
        db_path: PathBuf,
        query: String,
    },
    Context {
        db_path: PathBuf,
        session: Option<String>,
        json: bool,
    },
    Usage {
        db_path: PathBuf,
        session: Option<String>,
        json: bool,
    },
    Memory(MemoryCommand),
    Plugins(PluginCommand),
    Verify {
        argv: Vec<String>,
    },
    Diff,
    Init {
        force: bool,
    },
    Config(ConfigCommand),
    Auth(AuthCommand),
    Doctor,
    Completion(CompletionCommand),
    Runtime(RuntimeCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandDefinition {
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    category: &'static str,
    arguments: &'static str,
    destructive: bool,
    requires_agent: bool,
    local: bool,
    handler_id: &'static str,
    source: &'static str,
}

fn command_registry() -> Vec<CommandDefinition> {
    vec![
        CommandDefinition {
            name: "init",
            aliases: &[],
            description: "initialize a workspace",
            category: "workspace",
            arguments: "[--force]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "init",
            source: "builtin",
        },
        CommandDefinition {
            name: "config",
            aliases: &[],
            description: "inspect or mutate typed configuration",
            category: "control",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "config",
            source: "builtin",
        },
        CommandDefinition {
            name: "auth",
            aliases: &[],
            description: "manage provider credentials",
            category: "control",
            arguments: "ACTION PROVIDER",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "auth",
            source: "builtin",
        },
        CommandDefinition {
            name: "doctor",
            aliases: &[],
            description: "inspect local runtime health",
            category: "diagnostics",
            arguments: "",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "doctor",
            source: "builtin",
        },
        CommandDefinition {
            name: "completion",
            aliases: &[],
            description: "generate or install shell completion",
            category: "control",
            arguments: "ACTION SHELL",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "completion",
            source: "builtin",
        },
        CommandDefinition {
            name: "providers",
            aliases: &[],
            description: "configure model providers",
            category: "runtime",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "providers",
            source: "builtin",
        },
        CommandDefinition {
            name: "permissions",
            aliases: &[],
            description: "configure execution permissions",
            category: "security",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "permissions",
            source: "builtin",
        },
        CommandDefinition {
            name: "mcp",
            aliases: &[],
            description: "configure MCP servers",
            category: "runtime",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "mcp",
            source: "builtin",
        },
        CommandDefinition {
            name: "lsp",
            aliases: &[],
            description: "configure language servers",
            category: "runtime",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "lsp",
            source: "builtin",
        },
        CommandDefinition {
            name: "hooks",
            aliases: &[],
            description: "inspect and test hooks",
            category: "runtime",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "hooks",
            source: "builtin",
        },
        CommandDefinition {
            name: "skills",
            aliases: &[],
            description: "discover and validate skills",
            category: "runtime",
            arguments: "ACTION",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "skills",
            source: "builtin",
        },
        CommandDefinition {
            name: "subagents",
            aliases: &[],
            description: "run isolated subagents",
            category: "agent",
            arguments: "ACTION",
            destructive: true,
            requires_agent: true,
            local: false,
            handler_id: "subagents",
            source: "builtin",
        },
        CommandDefinition {
            name: "tui",
            aliases: &[],
            description: "configure the terminal UI",
            category: "control",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "tui",
            source: "builtin",
        },
        CommandDefinition {
            name: "status",
            aliases: &[],
            description: "show consolidated runtime status",
            category: "diagnostics",
            arguments: "[--json]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "status",
            source: "builtin",
        },
        CommandDefinition {
            name: "run",
            aliases: &["r"],
            description: "run one agent turn",
            category: "agent",
            arguments: "PROMPT",
            destructive: true,
            requires_agent: true,
            local: false,
            handler_id: "run",
            source: "builtin",
        },
        CommandDefinition {
            name: "exec",
            aliases: &["x"],
            description: "run one headless agent turn",
            category: "agent",
            arguments: "PROMPT",
            destructive: true,
            requires_agent: true,
            local: false,
            handler_id: "exec",
            source: "builtin",
        },
        CommandDefinition {
            name: "sessions",
            aliases: &[],
            description: "inspect persisted sessions",
            category: "storage",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "sessions",
            source: "builtin",
        },
        CommandDefinition {
            name: "database",
            aliases: &[],
            description: "backup or restore the database",
            category: "storage",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "database",
            source: "builtin",
        },
        CommandDefinition {
            name: "audit",
            aliases: &[],
            description: "inspect bounded audit events",
            category: "storage",
            arguments: "ACTION",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "audit",
            source: "builtin",
        },
        CommandDefinition {
            name: "jobs",
            aliases: &[],
            description: "manage background jobs",
            category: "runtime",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "jobs",
            source: "builtin",
        },
        CommandDefinition {
            name: "eval",
            aliases: &[],
            description: "run offline evaluations",
            category: "diagnostics",
            arguments: "ACTION",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "eval",
            source: "builtin",
        },
        CommandDefinition {
            name: "perf",
            aliases: &[],
            description: "measure offline performance",
            category: "diagnostics",
            arguments: "ACTION",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "perf",
            source: "builtin",
        },
        CommandDefinition {
            name: "self",
            aliases: &[],
            description: "manage the installed binary",
            category: "control",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "self",
            source: "builtin",
        },
        CommandDefinition {
            name: "trust",
            aliases: &[],
            description: "manage public trust keys",
            category: "security",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "trust",
            source: "builtin",
        },
        CommandDefinition {
            name: "registry",
            aliases: &[],
            description: "inspect signed registries",
            category: "security",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "registry",
            source: "builtin",
        },
        CommandDefinition {
            name: "plugins",
            aliases: &[],
            description: "manage signed plugins",
            category: "runtime",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "plugins",
            source: "builtin",
        },
        CommandDefinition {
            name: "search",
            aliases: &[],
            description: "search persisted sessions",
            category: "storage",
            arguments: "--query TEXT",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "search",
            source: "builtin",
        },
        CommandDefinition {
            name: "context",
            aliases: &[],
            description: "inspect selected context",
            category: "diagnostics",
            arguments: "[--session ID]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "context",
            source: "builtin",
        },
        CommandDefinition {
            name: "usage",
            aliases: &[],
            description: "inspect normalized usage",
            category: "diagnostics",
            arguments: "[--session ID]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "usage",
            source: "builtin",
        },
        CommandDefinition {
            name: "memory",
            aliases: &[],
            description: "manage bounded project memory",
            category: "storage",
            arguments: "ACTION",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "memory",
            source: "builtin",
        },
        CommandDefinition {
            name: "verify",
            aliases: &[],
            description: "run explicit verification argv",
            category: "agent",
            arguments: "-- COMMAND ARGS...",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "verify",
            source: "builtin",
        },
        CommandDefinition {
            name: "diff",
            aliases: &[],
            description: "show a bounded read-only git diff",
            category: "diagnostics",
            arguments: "",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "diff",
            source: "builtin",
        },
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug)]
struct RunOutcome {
    turn_id: i64,
    events: Vec<agent_protocol::Event>,
}
#[derive(Debug, PartialEq, Eq)]
enum SessionCommand {
    List,
    Show(i64),
    Archive(i64, bool),
    Delete(i64),
    Export(i64, PathBuf),
    Import(PathBuf),
}
#[derive(Debug, PartialEq, Eq)]
enum DatabaseCommand {
    Backup { input: PathBuf, output: PathBuf },
    Restore { input: PathBuf, workspace: PathBuf },
}
#[derive(Debug, PartialEq, Eq)]
enum AuditCommand {
    List {
        db_path: PathBuf,
        session: Option<i64>,
        event: Option<String>,
        limit: usize,
    },
    Show {
        db_path: PathBuf,
        id: i64,
    },
    Export {
        db_path: PathBuf,
        output: PathBuf,
        session: Option<i64>,
        limit: usize,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum JobCommand {
    Start {
        argv: Vec<String>,
    },
    List,
    Status(String),
    Logs {
        id: String,
        stderr: bool,
        tail: usize,
    },
    Cancel(String),
    Wait {
        id: String,
        timeout: Duration,
    },
    Reap {
        id: Option<String>,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum EvalCommand {
    Run {
        suite: String,
        report: Option<PathBuf>,
        baseline: Option<PathBuf>,
        max_regressions: usize,
    },
    Show(PathBuf),
    Baseline {
        input: PathBuf,
        output: PathBuf,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum PerfCommand {
    Run {
        runs: usize,
        suite: Option<PathBuf>,
        fixtures: Option<PathBuf>,
        output: Option<PathBuf>,
        baseline: Option<PathBuf>,
        max_cold_start_ms: Option<u128>,
        max_peak_rss_bytes: Option<u64>,
        max_regressions: usize,
    },
    Show(PathBuf),
    Compare {
        current: PathBuf,
        baseline: PathBuf,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum SelfCommand {
    Version,
    Install {
        prefix: PathBuf,
        name: String,
        compat: bool,
    },
    Uninstall {
        name: String,
        confirm: String,
    },
    Update {
        source: Option<PathBuf>,
        sha256: Option<String>,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum TrustCommand {
    List {
        path: PathBuf,
    },
    Show {
        path: PathBuf,
        id: String,
    },
    Add {
        path: PathBuf,
        id: String,
        key_file: PathBuf,
    },
    Disable {
        path: PathBuf,
        id: String,
    },
    Enable {
        path: PathBuf,
        id: String,
    },
    Revoke {
        path: PathBuf,
        id: String,
    },
    Remove {
        path: PathBuf,
        id: String,
        confirm: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum RegistryCommand {
    Status {
        path: PathBuf,
        id: Option<String>,
    },
    Reset {
        path: PathBuf,
        id: String,
        confirm: String,
    },
    Verify {
        source: String,
        trust: PathBuf,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum RuntimeCommand {
    Providers(Vec<String>),
    Permissions(Vec<String>),
    Mcp(Vec<String>),
    Lsp(Vec<String>),
    Hooks(Vec<String>),
    Skills(Vec<String>),
    Subagents(Vec<String>),
    Tui(Vec<String>),
    Status(Vec<String>),
}
#[derive(Debug, PartialEq, Eq)]
enum ConfigCommand {
    List,
    Sources,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
    Path,
    Validate,
}
#[derive(Debug, PartialEq, Eq)]
enum AuthCommand {
    Status { provider: String },
    Login { provider: String, token_stdin: bool },
    Logout { provider: String },
}
#[derive(Debug, PartialEq, Eq)]
enum CompletionCommand {
    Generate {
        shell: String,
    },
    Install {
        shell: String,
        path: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct UserConfig {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimePolicy {
    mode: PermissionMode,
    allowed_network_hosts: Vec<String>,
    env_allow: Vec<String>,
    env_deny: Vec<String>,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            mode: PermissionMode::WorkspaceWrite,
            allowed_network_hosts: Vec::new(),
            env_allow: Vec::new(),
            env_deny: Vec::new(),
        }
    }
}

impl RuntimePolicy {
    fn filter_env(&self, names: &mut Vec<String>) {
        names.retain(|name| {
            !self.env_deny.iter().any(|denied| denied == name)
                && (self.env_allow.is_empty()
                    || self.env_allow.iter().any(|allowed| allowed == name))
        });
    }
}
#[derive(Debug, PartialEq, Eq)]
enum MemoryCommand {
    List {
        db_path: PathBuf,
        limit: usize,
    },
    Search {
        db_path: PathBuf,
        query: String,
        limit: usize,
    },
    Add {
        db_path: PathBuf,
        key: String,
        content: String,
        kind: String,
        importance: i64,
    },
    Supersede {
        db_path: PathBuf,
        id: i64,
    },
    Delete {
        db_path: PathBuf,
        id: i64,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum PluginCommand {
    List,
    Verify {
        registry: String,
        trust_store: PathBuf,
        id: String,
    },
    Install {
        registry: String,
        trust_store: PathBuf,
        id: String,
    },
    Update {
        registry: String,
        trust_store: PathBuf,
        id: String,
    },
    Uninstall {
        id: String,
    },
}
#[derive(Debug)]
struct RunOptions {
    provider: String,
    model: String,
    base_url: Option<String>,
    prompt: String,
    db_path: PathBuf,
    resume: Option<String>,
    tui: bool,
    config: Option<PathBuf>,
    provider_explicit: bool,
    model_explicit: bool,
    base_url_explicit: bool,
    output_format: OutputFormat,
    exec: bool,
    event_sender: Option<SyncSender<agent_protocol::Event>>,
    approval: Option<ApprovalBroker>,
}

impl PartialEq for RunOptions {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.base_url == other.base_url
            && self.prompt == other.prompt
            && self.db_path == other.db_path
            && self.resume == other.resume
            && self.tui == other.tui
            && self.config == other.config
            && self.provider_explicit == other.provider_explicit
            && self.model_explicit == other.model_explicit
            && self.base_url_explicit == other.base_url_explicit
            && self.output_format == other.output_format
            && self.exec == other.exec
    }
}

impl Eq for RunOptions {}

fn default_run_options(
    prompt: String,
    db_path: PathBuf,
    resume: Option<String>,
    event_sender: Option<SyncSender<agent_protocol::Event>>,
) -> RunOptions {
    RunOptions {
        provider: "fake".into(),
        model: "gpt-5".into(),
        base_url: None,
        prompt,
        db_path,
        resume,
        tui: false,
        config: None,
        provider_explicit: false,
        model_explicit: false,
        base_url_explicit: false,
        output_format: OutputFormat::Text,
        exec: false,
        event_sender,
        approval: None,
    }
}

fn parse_args(args: Vec<String>) -> Result<Command, String> {
    let args = extract_workspace_flag(args)?;
    if args.len() == 1 && args[0] == "--version" {
        return Ok(Command::Version);
    }
    if args.is_empty() {
        return Ok(Command::Interactive);
    }
    if matches!(args[0].as_str(), "run" | "exec" | "r" | "x") {
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok(if args.is_empty() {
                Command::Help
            } else {
                Command::ContextHelp(vec![match args[0].as_str() {
                    "r" => "run",
                    "x" => "exec",
                    other => other,
                }
                .into()])
            });
        }
        return parse_run(
            if args
                .first()
                .is_some_and(|arg| matches!(arg.as_str(), "run" | "exec" | "r" | "x"))
            {
                &args[1..]
            } else {
                &args
            },
            args.first().is_some_and(|arg| arg == "exec" || arg == "x"),
        );
    }
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        let path = args
            .iter()
            .take_while(|arg| *arg != "--help" && *arg != "-h")
            .cloned()
            .collect::<Vec<_>>();
        return Ok(Command::ContextHelp(path));
    }
    match args[0].as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "sessions" => parse_sessions_command(&args[1..]),
        "database" => parse_database_command(&args[1..]),
        "audit" => parse_audit_command(&args[1..]),
        "jobs" => parse_jobs_command(&args[1..]),
        "eval" => parse_eval_command(&args[1..]),
        "perf" => parse_perf_command(&args[1..]),
        "self" => parse_self_command(&args[1..]),
        "trust" => parse_trust_command(&args[1..]),
        "registry" => parse_registry_command(&args[1..]),
        "search" => {
            let db_path = path_option(&args[1..], "--db-path")?;
            let query = value_after(&args[1..], "--query")?.ok_or("search requires --query")?;
            Ok(Command::Search { db_path, query })
        }
        "context" => parse_observability_command(&args[1..], true),
        "usage" => parse_observability_command(&args[1..], false),
        "memory" => parse_memory(&args[1..]),
        "plugins" => parse_plugins(&args[1..]),
        "verify" => {
            let separator = args
                .iter()
                .position(|arg| arg == "--")
                .ok_or("verify requires argv after --")?;
            let argv = args[separator + 1..].to_vec();
            if argv.is_empty() {
                return Err("verify requires a non-empty argv after --".into());
            }
            Ok(Command::Verify { argv })
        }
        "diff" => Ok(Command::Diff),
        "init" => Ok(Command::Init {
            force: args.iter().any(|arg| arg == "--force"),
        }),
        "config" => parse_config_command(&args[1..]),
        "auth" => parse_auth_command(&args[1..]),
        "doctor" => Ok(Command::Doctor),
        "completion" => parse_completion_command(&args[1..]),
        "providers" => Ok(Command::Runtime(RuntimeCommand::Providers(
            args[1..].to_vec(),
        ))),
        "permissions" => Ok(Command::Runtime(RuntimeCommand::Permissions(
            args[1..].to_vec(),
        ))),
        "mcp" => Ok(Command::Runtime(RuntimeCommand::Mcp(args[1..].to_vec()))),
        "lsp" => Ok(Command::Runtime(RuntimeCommand::Lsp(args[1..].to_vec()))),
        "hooks" => Ok(Command::Runtime(RuntimeCommand::Hooks(args[1..].to_vec()))),
        "skills" => Ok(Command::Runtime(RuntimeCommand::Skills(args[1..].to_vec()))),
        "subagents" => Ok(Command::Runtime(RuntimeCommand::Subagents(
            args[1..].to_vec(),
        ))),
        "tui" => Ok(Command::Runtime(RuntimeCommand::Tui(args[1..].to_vec()))),
        "status" => Ok(Command::Runtime(RuntimeCommand::Status(args[1..].to_vec()))),
        _ => parse_run(&args, false),
    }
}

fn extract_workspace_flag(args: Vec<String>) -> Result<Vec<String>, String> {
    let mut filtered = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--workspace" {
            let value = args.get(index + 1).ok_or("--workspace requires a path")?;
            if value.starts_with('-') {
                return Err("--workspace requires a path".into());
            }
            let path = PathBuf::from(value)
                .canonicalize()
                .map_err(|error| format!("invalid --workspace: {error}"))?;
            if !path.is_dir() {
                return Err("--workspace must point to a directory".into());
            }
            WORKSPACE_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(path));
            index += 2;
            continue;
        }
        filtered.push(args[index].clone());
        index += 1;
    }
    Ok(filtered)
}

fn parse_config_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let command = match action {
        "list" => ConfigCommand::List,
        "sources" => ConfigCommand::Sources,
        "get" => ConfigCommand::Get {
            key: value_after(&args[1..], "--key")?
                .or_else(|| args.get(1).cloned())
                .ok_or("config get requires key")?,
        },
        "set" => ConfigCommand::Set {
            key: value_after(&args[1..], "--key")?
                .or_else(|| args.get(1).cloned())
                .ok_or("config set requires key")?,
            value: value_after(&args[1..], "--value")?
                .or_else(|| args.get(2).cloned())
                .ok_or("config set requires value")?,
        },
        "unset" => ConfigCommand::Unset {
            key: value_after(&args[1..], "--key")?
                .or_else(|| args.get(1).cloned())
                .ok_or("config unset requires key")?,
        },
        "path" => ConfigCommand::Path,
        "validate" => ConfigCommand::Validate,
        other => return Err(format!("unknown config command: {other}")),
    };
    Ok(Command::Config(command))
}

fn parse_auth_command(args: &[String]) -> Result<Command, String> {
    let action = args
        .first()
        .map(String::as_str)
        .ok_or("auth requires status, login, or logout")?;
    let provider = args.get(1).cloned().ok_or("auth requires provider")?;
    if !valid_provider(&provider) {
        return Err("unknown provider".into());
    }
    let command = match action {
        "status" => AuthCommand::Status { provider },
        "login" => AuthCommand::Login {
            provider,
            token_stdin: args.iter().any(|arg| arg == "--token-stdin"),
        },
        "logout" => AuthCommand::Logout { provider },
        other => return Err(format!("unknown auth command: {other}")),
    };
    Ok(Command::Auth(command))
}

fn exact_id(args: &[String], action: &str) -> Result<i64, String> {
    args.get(1)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .or(value_after(args, "--id")?)
        .ok_or_else(|| format!("sessions {action} requires exact id"))?
        .parse()
        .map_err(|_| "id must be an integer".into())
}

fn parse_sessions_command(args: &[String]) -> Result<Command, String> {
    let db_path = path_option(args, "--db-path")?;
    let action = args.first().map(String::as_str).unwrap_or("list");
    let command = match action {
        "list" => SessionCommand::List,
        "show" => SessionCommand::Show(exact_id(args, action)?),
        "archive" => SessionCommand::Archive(exact_id(args, action)?, true),
        "unarchive" => SessionCommand::Archive(exact_id(args, action)?, false),
        "delete" => {
            let id = exact_id(args, action)?;
            let confirm = value_after(args, "--confirm")?.ok_or("delete requires --confirm ID")?;
            if confirm.parse::<i64>().ok() != Some(id) {
                return Err("--confirm must exactly match session id".into());
            }
            SessionCommand::Delete(id)
        }
        "export" => SessionCommand::Export(
            exact_id(args, action)?,
            value_after(args, "--output")?
                .map(PathBuf::from)
                .ok_or("export requires --output PATH")?,
        ),
        "import" => SessionCommand::Import(
            value_after(args, "--input")?
                .map(PathBuf::from)
                .ok_or("import requires --input PATH")?,
        ),
        other => return Err(format!("unknown sessions command: {other}")),
    };
    Ok(Command::Sessions { command, db_path })
}

fn parse_database_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().ok_or("database requires backup or restore")?;
    match action.as_str() {
        "backup" => Ok(Command::Database(DatabaseCommand::Backup {
            input: path_option(args, "--db-path")?,
            output: value_after(&args[1..], "--output")?
                .map(PathBuf::from)
                .ok_or("backup requires --output PATH")?,
        })),
        "restore" => Ok(Command::Database(DatabaseCommand::Restore {
            input: value_after(&args[1..], "--input")?
                .map(PathBuf::from)
                .ok_or("restore requires --input PATH")?,
            workspace: value_after(&args[1..], "--confirm-workspace")?
                .map(PathBuf::from)
                .ok_or("restore requires --confirm-workspace PATH")?,
        })),
        other => Err(format!("unknown database command: {other}")),
    }
}

fn parse_audit_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().ok_or("audit requires list, show, or export")?;
    let db_path = path_option(&args[1..], "--db-path")?;
    match action.as_str() {
        "list" => Ok(Command::Audit(AuditCommand::List {
            db_path,
            session: value_after(&args[1..], "--session")?.and_then(|v| v.parse().ok()),
            event: value_after(&args[1..], "--event")?,
            limit: value_after(&args[1..], "--limit")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
        })),
        "show" => Ok(Command::Audit(AuditCommand::Show {
            db_path,
            id: value_after(&args[1..], "--id")?
                .ok_or("audit show requires --id")?
                .parse()
                .map_err(|_| "invalid audit id")?,
        })),
        "export" => Ok(Command::Audit(AuditCommand::Export {
            db_path,
            output: value_after(&args[1..], "--output")?
                .map(PathBuf::from)
                .ok_or("audit export requires --output PATH")?,
            session: value_after(&args[1..], "--session")?.and_then(|v| v.parse().ok()),
            limit: value_after(&args[1..], "--limit")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000),
        })),
        other => Err(format!("unknown audit command: {other}")),
    }
}

fn parse_jobs_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let command = match action {
        "start" => {
            let separator = args
                .iter()
                .position(|arg| arg == "--")
                .ok_or("jobs start requires argv after --")?;
            let argv = args[separator + 1..].to_vec();
            if argv.is_empty() {
                return Err("jobs start requires a non-empty argv".into());
            }
            if argv.iter().any(|arg| secret_arg(arg)) {
                return Err("secret-like job arguments are rejected".into());
            }
            JobCommand::Start { argv }
        }
        "list" => JobCommand::List,
        "status" => JobCommand::Status(args.get(1).cloned().ok_or("jobs status requires ID")?),
        "logs" => JobCommand::Logs {
            id: args.get(1).cloned().ok_or("jobs logs requires ID")?,
            stderr: args.iter().any(|arg| arg == "--stderr"),
            tail: value_after(args, "--tail")?
                .and_then(|value| value.parse().ok())
                .unwrap_or(100),
        },
        "cancel" => JobCommand::Cancel(args.get(1).cloned().ok_or("jobs cancel requires ID")?),
        "wait" => JobCommand::Wait {
            id: args.get(1).cloned().ok_or("jobs wait requires ID")?,
            timeout: Duration::from_secs(
                value_after(args, "--timeout")?
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(30),
            ),
        },
        "reap" => JobCommand::Reap {
            id: args
                .get(1)
                .cloned()
                .or_else(|| value_after(args, "--id").ok().flatten()),
        },
        other => return Err(format!("unknown jobs command: {other}")),
    };
    Ok(Command::Jobs(command))
}

fn parse_eval_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("run");
    match action {
        "run" => Ok(Command::Eval(EvalCommand::Run {
            suite: value_after(&args[1..], "--suite")?.unwrap_or_else(|| "smoke".into()),
            report: value_after(&args[1..], "--report")?.map(PathBuf::from),
            baseline: value_after(&args[1..], "--baseline")?.map(PathBuf::from),
            max_regressions: value_after(&args[1..], "--max-regressions")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        })),
        "show" => Ok(Command::Eval(EvalCommand::Show(
            value_after(&args[1..], "--report")?
                .map(PathBuf::from)
                .or_else(|| args.get(1).cloned().map(PathBuf::from))
                .ok_or("eval show requires REPORT")?,
        ))),
        "baseline" => {
            if args.get(1).map(String::as_str) != Some("create") {
                return Err("eval baseline requires create".into());
            }
            Ok(Command::Eval(EvalCommand::Baseline {
                input: value_after(&args[2..], "--from")?
                    .map(PathBuf::from)
                    .ok_or("baseline requires --from REPORT")?,
                output: value_after(&args[2..], "--output")?
                    .map(PathBuf::from)
                    .ok_or("baseline requires --output PATH")?,
            }))
        }
        other => Err(format!("unknown eval command: {other}")),
    }
}

fn parse_perf_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("run");
    match action {
        "run" => Ok(Command::Perf(PerfCommand::Run {
            runs: value_after(&args[1..], "--runs")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            suite: value_after(&args[1..], "--suite")?.map(|value| {
                if value == "smoke" {
                    PathBuf::from("crates/evals/fixtures/tasks.json")
                } else if value == "benchmark-100" {
                    PathBuf::from("crates/evals/fixtures/benchmark-100.json")
                } else {
                    PathBuf::from(value)
                }
            }),
            fixtures: value_after(&args[1..], "--fixtures")?.map(PathBuf::from),
            output: value_after(&args[1..], "--output")?.map(PathBuf::from),
            baseline: value_after(&args[1..], "--baseline")?.map(PathBuf::from),
            max_cold_start_ms: value_after(&args[1..], "--max-cold-start-ms")?
                .and_then(|v| v.parse().ok()),
            max_peak_rss_bytes: value_after(&args[1..], "--max-peak-rss-bytes")?
                .and_then(|v| v.parse().ok()),
            max_regressions: value_after(&args[1..], "--max-regressions")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        })),
        "show" => Ok(Command::Perf(PerfCommand::Show(
            value_after(&args[1..], "--report")?
                .map(PathBuf::from)
                .or_else(|| args.get(1).cloned().map(PathBuf::from))
                .ok_or("perf show requires REPORT")?,
        ))),
        "compare" => Ok(Command::Perf(PerfCommand::Compare {
            current: value_after(&args[1..], "--current")?
                .map(PathBuf::from)
                .or_else(|| args.get(1).cloned().map(PathBuf::from))
                .ok_or("perf compare requires current report")?,
            baseline: value_after(&args[1..], "--baseline")?
                .map(PathBuf::from)
                .or_else(|| args.get(2).cloned().map(PathBuf::from))
                .ok_or("perf compare requires baseline report")?,
        })),
        other => Err(format!("unknown perf command: {other}")),
    }
}

fn parse_self_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("version");
    match action {
        "version" => Ok(Command::SelfCommand(SelfCommand::Version)),
        "install" => Ok(Command::SelfCommand(SelfCommand::Install {
            prefix: value_after(&args[1..], "--prefix")?
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~/.local")),
            name: value_after(&args[1..], "--name")?.unwrap_or_else(|| "helios".into()),
            compat: args.iter().any(|arg| arg == "--compat-agent-cli"),
        })),
        "uninstall" => {
            let name = value_after(&args[1..], "--name")?.unwrap_or_else(|| "helios".into());
            let confirm = value_after(&args[1..], "--confirm")?
                .ok_or("self uninstall requires --confirm NAME")?;
            if confirm != name {
                return Err("self uninstall confirmation must exactly match NAME".into());
            }
            Ok(Command::SelfCommand(SelfCommand::Uninstall {
                name,
                confirm,
            }))
        }
        "update" => Ok(Command::SelfCommand(SelfCommand::Update {
            source: value_after(&args[1..], "--from")?.map(PathBuf::from),
            sha256: value_after(&args[1..], "--sha256")?,
        })),
        other => Err(format!("unknown self command: {other}")),
    }
}

fn default_trust_path() -> Result<PathBuf, String> {
    Ok(app_paths()?.trust_store)
}

fn trust_path(args: &[String]) -> Result<PathBuf, String> {
    Ok(value_after(args, "--trust-store")?
        .map(PathBuf::from)
        .unwrap_or(default_trust_path()?))
}

fn parse_trust_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let path = trust_path(&args[1..])?;
    let id = || {
        value_after(&args[1..], "--id")?.ok_or_else(|| "trust command requires --id".to_string())
    };
    let command = match action {
        "list" => TrustCommand::List { path },
        "show" => TrustCommand::Show { path, id: id()? },
        "add" => TrustCommand::Add {
            path,
            id: id()?,
            key_file: value_after(&args[1..], "--public-key-file")?
                .map(PathBuf::from)
                .ok_or("trust add requires --public-key-file PATH")?,
        },
        "disable" => TrustCommand::Disable { path, id: id()? },
        "enable" => TrustCommand::Enable { path, id: id()? },
        "revoke" => TrustCommand::Revoke { path, id: id()? },
        "remove" => {
            let id = id()?;
            let confirm = value_after(&args[1..], "--confirm")?
                .ok_or("trust remove requires --confirm ID")?;
            if id != confirm {
                return Err("trust remove confirmation must match ID".into());
            }
            TrustCommand::Remove { path, id, confirm }
        }
        other => return Err(format!("unknown trust command: {other}")),
    };
    Ok(Command::Trust(command))
}

fn parse_registry_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let state = app_paths()?.registry_state;
    match action {
        "status" => Ok(Command::Registry(RegistryCommand::Status {
            path: state,
            id: value_after(&args[1..], "--registry-id")?,
        })),
        "reset" => {
            let id = value_after(&args[1..], "--registry-id")?
                .ok_or("registry reset requires --registry-id ID")?;
            let confirm = value_after(&args[1..], "--confirm")?
                .ok_or("registry reset requires --confirm ID")?;
            if id != confirm {
                return Err("registry reset confirmation must match ID".into());
            }
            Ok(Command::Registry(RegistryCommand::Reset {
                path: state,
                id,
                confirm,
            }))
        }
        "verify" => Ok(Command::Registry(RegistryCommand::Verify {
            source: value_after(&args[1..], "--source")?
                .ok_or("registry verify requires --source PATH|URL")?,
            trust: trust_path(&args[1..])?,
        })),
        other => Err(format!("unknown registry command: {other}")),
    }
}

fn parse_completion_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("generate");
    let shell = args.get(1).cloned().ok_or("completion requires shell")?;
    if !matches!(shell.as_str(), "bash" | "zsh" | "fish") {
        return Err("shell must be bash, zsh, or fish".into());
    }
    match action {
        "generate" => Ok(Command::Completion(CompletionCommand::Generate { shell })),
        "install" => Ok(Command::Completion(CompletionCommand::Install {
            shell,
            path: value_after(&args[2..], "--path")?.map(PathBuf::from),
        })),
        other => Err(format!("unknown completion command: {other}")),
    }
}

fn parse_observability_command(args: &[String], context: bool) -> Result<Command, String> {
    let db_path = path_option(args, "--db-path")?;
    let session = value_after(args, "--session")?;
    let json = args.iter().any(|arg| arg == "--json");
    if context {
        Ok(Command::Context {
            db_path,
            session,
            json,
        })
    } else {
        Ok(Command::Usage {
            db_path,
            session,
            json,
        })
    }
}

fn parse_memory(args: &[String]) -> Result<Command, String> {
    let db_path = path_option(args, "--db-path")?;
    let limit = value_after(args, "--limit")?
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| "--limit expects an integer")
        })
        .transpose()?
        .unwrap_or(20);
    let subcommand = args.first().map(String::as_str).unwrap_or("list");
    match subcommand {
        "list" => Ok(Command::Memory(MemoryCommand::List { db_path, limit })),
        "search" => Ok(Command::Memory(MemoryCommand::Search {
            db_path,
            query: value_after(&args[1..], "--query")?.ok_or("memory search requires --query")?,
            limit,
        })),
        "add" => Ok(Command::Memory(MemoryCommand::Add {
            db_path,
            key: value_after(&args[1..], "--key")?.ok_or("memory add requires --key")?,
            content: value_after(&args[1..], "--content")?
                .ok_or("memory add requires --content")?,
            kind: value_after(&args[1..], "--kind")?.unwrap_or_else(|| "fact".into()),
            importance: value_after(&args[1..], "--importance")?
                .map(|value| {
                    value
                        .parse::<i64>()
                        .map_err(|_| "--importance expects an integer")
                })
                .transpose()?
                .unwrap_or(1),
        })),
        "supersede" => Ok(Command::Memory(MemoryCommand::Supersede {
            db_path,
            id: required_id(&args[1..], "--id")?,
        })),
        "delete" => Ok(Command::Memory(MemoryCommand::Delete {
            db_path,
            id: required_id(&args[1..], "--id")?,
        })),
        other => Err(format!("unknown memory command: {other}")),
    }
}

fn required_id(args: &[String], name: &str) -> Result<i64, String> {
    value_after(args, name)?
        .ok_or_else(|| format!("{name} requires an exact integer id"))?
        .parse::<i64>()
        .map_err(|_| format!("{name} requires an exact integer id"))
}

fn parse_plugins(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "list" {
        return Ok(Command::Plugins(PluginCommand::List));
    }
    let id = value_after(&args[1..], "--id")?.ok_or("plugin command requires --id")?;
    if action == "uninstall" {
        return Ok(Command::Plugins(PluginCommand::Uninstall { id }));
    }
    let registry =
        value_after(&args[1..], "--registry")?.ok_or("plugin command requires --registry")?;
    let trust_store = value_after(&args[1..], "--trust-store")?
        .map(PathBuf::from)
        .unwrap_or(default_trust_path()?);
    let command = match action {
        "verify" => PluginCommand::Verify {
            registry,
            trust_store,
            id,
        },
        "install" => PluginCommand::Install {
            registry,
            trust_store,
            id,
        },
        "update" => PluginCommand::Update {
            registry,
            trust_store,
            id,
        },
        other => return Err(format!("unknown plugin command: {other}")),
    };
    Ok(Command::Plugins(command))
}
fn parse_run(args: &[String], exec: bool) -> Result<Command, String> {
    let mut options = RunOptions {
        provider: "fake".into(),
        model: "gpt-5".into(),
        base_url: None,
        prompt: String::new(),
        db_path: default_db_path()?,
        resume: None,
        tui: false,
        config: None,
        provider_explicit: false,
        model_explicit: false,
        base_url_explicit: false,
        output_format: OutputFormat::Text,
        exec,
        event_sender: None,
        approval: None,
    };
    let mut prompt = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--provider" => {
                options.provider = required_value(args, &mut index, "--provider")?;
                options.provider_explicit = true;
            }
            "--model" => {
                options.model = required_value(args, &mut index, "--model")?;
                options.model_explicit = true;
            }
            "--base-url" => {
                options.base_url = Some(required_value(args, &mut index, "--base-url")?);
                options.base_url_explicit = true
            }
            "--db-path" => {
                options.db_path = PathBuf::from(required_value(args, &mut index, "--db-path")?)
            }
            "--resume" => options.resume = Some(required_value(args, &mut index, "--resume")?),
            "--tui" => options.tui = true,
            "--output-format" => {
                options.output_format =
                    match required_value(args, &mut index, "--output-format")?.as_str() {
                        "text" => OutputFormat::Text,
                        "json" => OutputFormat::Json,
                        "stream-json" => OutputFormat::StreamJson,
                        other => return Err(format!("unknown output format: {other}")),
                    };
            }
            "--config" => {
                options.config = Some(PathBuf::from(required_value(args, &mut index, "--config")?))
            }
            "run" => {}
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => prompt.push(value.to_owned()),
        }
        index += 1;
    }
    options.prompt = if prompt.is_empty() {
        "hello".into()
    } else {
        prompt.join(" ")
    };
    if !exec && options.output_format != OutputFormat::Text {
        return Err("--output-format is only available with exec".into());
    }
    if exec && options.tui {
        return Err("exec is headless and cannot initialize the TUI".into());
    }
    Ok(Command::Run(options))
}
fn required_value(args: &[String], index: &mut usize, name: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| format!("{name} requires a value"))
}
fn value_after(args: &[String], name: &str) -> Result<Option<String>, String> {
    args.iter()
        .position(|arg| arg == name)
        .map(|index| {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        })
        .transpose()
}
fn path_option(args: &[String], name: &str) -> Result<PathBuf, String> {
    Ok(value_after(args, name)?
        .map(PathBuf::from)
        .unwrap_or(default_db_path()?))
}
fn default_db_path() -> Result<PathBuf, String> {
    Ok(app_paths()?.db_path)
}

fn interactive_command() -> Result<(), String> {
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
    let ui_cancellation = CancellationToken::default();
    let first_turn = Arc::new(AtomicBool::new(true));
    let session_model = Arc::new(Mutex::new(None::<String>));
    let session_provider = Arc::new(Mutex::new(None::<String>));
    let active_session = Arc::new(Mutex::new(None::<i64>));
    let project_config_path = paths.effective_project_config().to_path_buf();
    let db_path = paths.db_path;
    let workspace_path = paths.workspace.clone();
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
                return true;
            }
            if command.starts_with('!') {
                let input = prompt.trim().to_owned();
                let worker_events = events.clone();
                let worker_workspace = workspace_path.clone();
                let worker_approval = approval.clone();
                let worker_cancellation = turn_cancellation.clone();
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
                return false;
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
                let result = id
                    .and_then(|id| Database::open(&db_path).ok().map(|db| (db, id)))
                    .map(|(db, id)| {
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
            } else if prompt.trim_start().starts_with('@') {
                Some(path_target_output(prompt.trim()))
            } else {
                slash_command_output(&command)
            };
            if let Some(output) = local_output {
                if command == "/retry" && prompt_for_run.is_empty() {
                    return false;
                }
                if command == "/retry" {
                    // Continue into the normal runtime with the restored user prompt.
                } else {
                    let _ = events.send(agent_protocol::Event::TurnStarted);
                    let _ = events.send(agent_protocol::Event::ModelDelta(output));
                    let _ = events.send(agent_protocol::Event::TurnCompleted);
                    return false;
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
            let options = match resolve_runtime_options(options) {
                Ok((options, _policy, _workspace)) => options,
                Err(error) => {
                    let _ = events.send(agent_protocol::Event::Error(redact_output(&error)));
                    return false;
                }
            };
            let worker_db_path = db_path.clone();
            let worker_workspace = workspace_path.clone();
            let worker_active_session = active_session.clone();
            std::thread::spawn(move || {
                let result = run_command_with_cancellation(options, turn_cancellation);
                if let Err(error) = result {
                    let _ = events.send(agent_protocol::Event::Error(redact_output(&error)));
                } else if let Ok(db) = Database::open(&worker_db_path) {
                    if let Ok(Some(record)) =
                        db.latest_session(&worker_workspace.display().to_string())
                    {
                        if let Ok(mut active) = worker_active_session.lock() {
                            *active = Some(record.id);
                        }
                    }
                }
            });
            false
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

fn interactive_tui_context(paths: &AppPaths) -> agent_tui::TuiContext {
    let config_path = paths.effective_project_config();
    let config = load_user_config(config_path).ok();
    let policy = load_runtime_policy(config_path).ok();
    let (branch, dirty_files) = git_state_snapshot(&paths.workspace);
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
    agent_tui::TuiContext {
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
        diff,
        diff_variants,
        ..agent_tui::TuiContext::default()
    }
}

fn session_option(summary: SessionSummary) -> agent_tui::SessionOption {
    agent_tui::SessionOption {
        id: summary.id,
        title: summary.objective,
        turn_count: summary.turn_count,
        last_activity: summary.last_activity,
        model: summary.model,
        git_branch: summary.git_branch,
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
    let capabilities = match provider {
        "openai" => openai_profile().capabilities,
        "anthropic" => anthropic_profile().capabilities,
        "gemini" => gemini_profile().capabilities,
        "generic" | "openai-compatible" => generic_profile().capabilities,
        _ => return "capabilities=unknown".into(),
    };
    format!(
        "tools,caching={},compaction={},reasoning={},streaming={}",
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
        "/effort" => Some("effort=provider-default (no session override)".into()),
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
            .map(|(role, content)| {
                Ok(Message {
                    role: parse_role(&role)?,
                    content,
                })
            })
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
        }
    }
    database
        .set_session_override(
            session_id,
            &workspace_id,
            Some(&options.provider),
            Some(&options.model),
            None,
            None,
        )
        .map_err(|error| error.to_string())?;
    let config_path = runtime_config_path.is_file().then_some(runtime_config_path);
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
    let builtin = ToolRegistry::new(&workspace, permission_mode)
        .map_err(|error| error.to_string())?
        .with_cancellation(cancellation.clone());
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
    let startup_memory = load_startup_memory(&options.db_path, &workspace_id, &options.prompt)?;
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
            ),
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
            ),
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
            ),
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
            ),
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
        ),
        other => Err(format!("unknown provider: {other}")),
    }
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
) -> Result<RunOutcome, String> {
    let previous = messages.len();
    let memory_scope = workspace_root()?.display().to_string();
    let mut agent = AgentLoop::new(provider)
        .with_memory_scope(memory_scope)
        .with_memory_retriever(Box::new(StaticMemory(startup_memory)))
        .with_compactor(Box::new(LocalCompactor));
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
                transaction.execute("INSERT INTO messages(session_id, turn_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))", rusqlite::params![session_id, turn_id, role_name(&message.role), message.content])?;
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
        Event::TurnStarted => ("turn_started", String::new()),
        Event::TurnCompleted => ("turn_completed", String::new()),
    }
}

fn add_usage(total: &mut UsageRecord, value: &str) {
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

fn sessions_command(command: SessionCommand, db_path: PathBuf) -> Result<(), String> {
    match command {
        SessionCommand::List => list_sessions(db_path),
        SessionCommand::Show(id) => session_show(db_path, id),
        SessionCommand::Archive(id, archived) => session_archive(db_path, id, archived),
        SessionCommand::Delete(id) => session_delete(db_path, id),
        SessionCommand::Export(id, output) => session_export(db_path, id, output),
        SessionCommand::Import(input) => session_import(db_path, input),
    }
}

fn current_workspace() -> Result<PathBuf, String> {
    workspace_root()
}

fn session_show(db_path: PathBuf, id: i64) -> Result<(), String> {
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

fn session_archive(db_path: PathBuf, id: i64, archived: bool) -> Result<(), String> {
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

fn session_delete(db_path: PathBuf, id: i64) -> Result<(), String> {
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

fn session_export(db_path: PathBuf, id: i64, output: PathBuf) -> Result<(), String> {
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

fn session_import(db_path: PathBuf, input: PathBuf) -> Result<(), String> {
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

fn redact_export_text(value: &str) -> String {
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

fn list_sessions(db_path: PathBuf) -> Result<(), String> {
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
fn search_sessions(db_path: PathBuf, query: &str) -> Result<(), String> {
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let db = open_database(&db_path)?;
    for (id, role, content) in db
        .search_messages_for_workspace(query, &workspace.display().to_string(), 20)
        .map_err(|error| error.to_string())?
    {
        println!("{id}\t{role}\t{content}");
    }
    Ok(())
}

fn database_command(command: DatabaseCommand) -> Result<(), String> {
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

fn audit_command(command: AuditCommand) -> Result<(), String> {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JobMeta {
    id: String,
    workspace: String,
    executable: String,
    argv_hash: String,
    pid: Option<u32>,
    pgid: Option<i32>,
    start_ticks: Option<u64>,
    status: String,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn jobs_dir(workspace: &Path) -> PathBuf {
    AppPaths::discover(workspace)
        .map(|paths| paths.jobs_dir)
        .unwrap_or_else(|_| workspace.join(".helios/jobs"))
}
fn job_path(workspace: &Path, id: &str) -> PathBuf {
    jobs_dir(workspace).join(format!("{id}.json"))
}
fn safe_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
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

fn job_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn argv_digest(argv: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    argv.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn read_job(workspace: &Path, id: &str) -> Result<JobMeta, String> {
    if !safe_job_id(id) {
        return Err("invalid job id".into());
    }
    let path = job_path(workspace, id);
    serde_json::from_slice(&fs::read(path).map_err(|_| "job not found".to_owned())?)
        .map_err(|error| format!("invalid job metadata: {error}"))
}

fn write_job(workspace: &Path, meta: &JobMeta) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(|error| error.to_string())?;
    atomic_write(&job_path(workspace, &meta.id), &bytes)
}

fn jobs_command(command: JobCommand) -> Result<(), String> {
    let workspace = current_workspace()?;
    fs::create_dir_all(jobs_dir(&workspace)).map_err(|error| error.to_string())?;
    match command {
        JobCommand::Start { argv } => {
            let entries = fs::read_dir(jobs_dir(&workspace))
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count();
            if entries >= 32 {
                return Err("job limit reached (32)".into());
            }
            let id = job_id();
            let meta = JobMeta {
                id: id.clone(),
                workspace: workspace.display().to_string(),
                executable: argv[0].clone(),
                argv_hash: argv_digest(&argv),
                pid: None,
                pgid: None,
                start_ticks: None,
                status: "starting".into(),
                exit_code: None,
                stdout: format!("{id}.stdout"),
                stderr: format!("{id}.stderr"),
            };
            write_job(&workspace, &meta)?;
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            let mut supervisor = OsCommand::new(executable);
            supervisor.args([
                "__job-supervisor",
                "--workspace",
                &workspace.display().to_string(),
                "--job-id",
                &id,
                "--",
            ]);
            supervisor.args(argv);
            supervisor
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            supervisor.spawn().map_err(|error| error.to_string())?;
            println!("{id}");
            Ok(())
        }
        JobCommand::List => {
            let mut ids = Vec::new();
            for entry in fs::read_dir(jobs_dir(&workspace)).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    if let Some(id) = path.file_stem().and_then(|value| value.to_str()) {
                        ids.push(id.to_owned());
                    }
                }
            }
            ids.sort();
            for id in ids {
                let meta = read_job(&workspace, &id)?;
                println!("{}\t{}\t{}", meta.id, meta.status, meta.executable);
            }
            Ok(())
        }
        JobCommand::Status(id) => {
            let meta = read_job(&workspace, &id)?;
            println!(
                "{}",
                serde_json::to_string(&meta).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        JobCommand::Logs { id, stderr, tail } => {
            let meta = read_job(&workspace, &id)?;
            let path = jobs_dir(&workspace).join(if stderr { meta.stderr } else { meta.stdout });
            let text = fs::read_to_string(path).unwrap_or_default();
            let lines = text.lines().collect::<Vec<_>>();
            println!(
                "{}",
                lines
                    .into_iter()
                    .rev()
                    .take(tail)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            Ok(())
        }
        JobCommand::Cancel(id) => cancel_job(&workspace, &id),
        JobCommand::Wait { id, timeout } => wait_job(&workspace, &id, timeout),
        JobCommand::Reap { id } => {
            let ids = id.into_iter().collect::<Vec<_>>();
            let candidates = if ids.is_empty() {
                fs::read_dir(jobs_dir(&workspace))
                    .map_err(|error| error.to_string())?
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry
                            .path()
                            .file_stem()
                            .and_then(|value| value.to_str())
                            .map(str::to_owned)
                    })
                    .collect()
            } else {
                ids
            };
            for id in candidates {
                let meta = read_job(&workspace, &id)?;
                if matches!(meta.status.as_str(), "completed" | "failed" | "cancelled") {
                    let _ = fs::remove_file(job_path(&workspace, &id));
                    let _ = fs::remove_file(jobs_dir(&workspace).join(meta.stdout));
                    let _ = fs::remove_file(jobs_dir(&workspace).join(meta.stderr));
                    println!("{id}");
                }
            }
            Ok(())
        }
    }
}

fn proc_start_ticks(pid: u32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = content
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    fields.get(19)?.parse().ok()
}

fn cancel_job(workspace: &Path, id: &str) -> Result<(), String> {
    let mut meta = read_job(workspace, id)?;
    let (Some(pid), Some(pgid), Some(start)) = (meta.pid, meta.pgid, meta.start_ticks) else {
        return Err("job has not started".into());
    };
    if proc_start_ticks(pid) != Some(start) {
        return Err("job PID identity mismatch; refusing cancel".into());
    }
    #[cfg(unix)]
    unsafe {
        if libc::kill(-pgid, libc::SIGTERM) != 0 {
            return Err("failed to cancel job process group".into());
        }
    }
    #[cfg(not(unix))]
    {
        return Err("job cancellation requires Unix".into());
    }
    meta.status = "cancelling".into();
    write_job(workspace, &meta)
}

fn wait_job(workspace: &Path, id: &str, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let meta = read_job(workspace, id)?;
        if matches!(meta.status.as_str(), "completed" | "failed" | "cancelled") {
            println!("{}\t{}", meta.id, meta.status);
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("job wait timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_job_supervisor(args: &[String]) -> Result<(), String> {
    let workspace = value_after(args, "--workspace")?
        .map(PathBuf::from)
        .ok_or("supervisor workspace missing")?;
    let id = value_after(args, "--job-id")?.ok_or("supervisor job id missing")?;
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or("supervisor argv missing")?;
    let argv = &args[separator + 1..];
    if argv.is_empty() || !safe_job_id(&id) {
        return Err("invalid supervisor argv".into());
    }
    let mut meta = read_job(&workspace, &id)?;
    let stdout = fs::File::create(jobs_dir(&workspace).join(&meta.stdout))
        .map_err(|error| error.to_string())?;
    let stderr = fs::File::create(jobs_dir(&workspace).join(&meta.stderr))
        .map_err(|error| error.to_string())?;
    let mut child = OsCommand::new(&argv[0]);
    child
        .args(&argv[1..])
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", env::var("PATH").unwrap_or_default())
        .stdout(stdout)
        .stderr(stderr)
        .stdin(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        child.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.spawn().map_err(|error| error.to_string())?;
    let pid = child.id();
    meta.pid = Some(pid);
    meta.pgid = Some(pid as i32);
    meta.start_ticks = proc_start_ticks(pid);
    meta.status = "running".into();
    write_job(&workspace, &meta)?;
    let status = child.wait().map_err(|error| error.to_string())?;
    meta.exit_code = status.code();
    meta.status = if status.success() {
        "completed"
    } else if status.code() == Some(143) {
        "cancelled"
    } else {
        "failed"
    }
    .into();
    write_job(&workspace, &meta)
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

fn eval_command(command: EvalCommand) -> Result<(), String> {
    let workspace = current_workspace()?;
    match command {
        EvalCommand::Run {
            suite,
            report,
            baseline,
            max_regressions,
        } => {
            let suite = embedded_suite(&suite)?;
            let fixture_root =
                std::env::temp_dir().join(format!("helios-eval-fixtures-{}", std::process::id()));
            let work_root =
                std::env::temp_dir().join(format!("helios-eval-work-{}", std::process::id()));
            write_embedded_fixtures(&fixture_root)?;
            let result = agent_evals::run_suite(&suite, &fixture_root, &work_root)
                .map_err(|error| error.to_string());
            let _ = fs::remove_dir_all(&fixture_root);
            let _ = fs::remove_dir_all(&work_root);
            let report_data = result?;
            let path = report_path(&workspace, "eval", report)?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&report_data).map_err(|error| error.to_string())?,
            )?;
            println!(
                "suite={} passed={} failed={} report={}",
                report_data.suite,
                report_data.passed,
                report_data.failed,
                path.display()
            );
            if let Some(baseline) = baseline {
                let old: agent_evals::EvalReport =
                    serde_json::from_slice(&fs::read(baseline).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                agent_evals::check_baseline(&report_data, &old, max_regressions)
                    .map_err(|error| error.to_string())?;
            }
            if report_data.failed > 0 {
                return Err(format!("eval failed: {} task(s)", report_data.failed));
            }
            Ok(())
        }
        EvalCommand::Show(path) => {
            let report: agent_evals::EvalReport =
                serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        EvalCommand::Baseline { input, output } => {
            let report: agent_evals::EvalReport =
                serde_json::from_slice(&fs::read(input).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            let path = report_path(&workspace, "baseline", Some(output))?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
            )?;
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn perf_command(command: PerfCommand) -> Result<(), String> {
    let workspace = current_workspace()?;
    match command {
        PerfCommand::Run {
            runs,
            suite,
            fixtures,
            output,
            baseline,
            max_cold_start_ms,
            max_peak_rss_bytes,
            max_regressions,
        } => {
            let binary = env::current_exe().map_err(|error| error.to_string())?;
            let report = agent_perf::run_full_report(
                &binary,
                runs,
                suite.as_deref(),
                fixtures.as_deref(),
                max_cold_start_ms,
                max_peak_rss_bytes,
            )
            .map_err(|error| error.to_string())?;
            let path = report_path(&workspace, "perf", output)?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
            )?;
            if let Some(baseline) = baseline {
                let old: agent_perf::FullPerfReport =
                    serde_json::from_slice(&fs::read(baseline).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                let regressions = usize::from(report.startup.p95_ms > old.startup.p95_ms)
                    + usize::from(
                        report
                            .short_session
                            .peak_rss_bytes
                            .zip(old.short_session.peak_rss_bytes)
                            .is_some_and(|(now, previous)| now > previous),
                    );
                if regressions > max_regressions {
                    return Err(format!(
                        "{regressions} performance regressions exceed {max_regressions}"
                    ));
                }
            }
            println!(
                "runs={} p50_ms={} p95_ms={} peak_rss_bytes={:?} benchmark_tasks={:?} report={}",
                report.startup.runs,
                report.startup.p50_ms,
                report.startup.p95_ms,
                report.short_session.peak_rss_bytes,
                report
                    .benchmark
                    .as_ref()
                    .map(|benchmark| benchmark.task_count),
                path.display()
            );
            Ok(())
        }
        PerfCommand::Show(path) => {
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        PerfCommand::Compare { current, baseline } => {
            let current: serde_json::Value =
                serde_json::from_slice(&fs::read(current).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            let baseline: serde_json::Value =
                serde_json::from_slice(&fs::read(baseline).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            let current_p95 = current
                .pointer("/startup/p95_ms")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| current.get("p95_ms").and_then(serde_json::Value::as_u64))
                .unwrap_or(0);
            let baseline_p95 = baseline
                .pointer("/startup/p95_ms")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| baseline.get("p95_ms").and_then(serde_json::Value::as_u64))
                .unwrap_or(0);
            if current_p95 > baseline_p95 {
                return Err(format!(
                    "p95 {current_p95}ms exceeds baseline {baseline_p95}ms"
                ));
            }
            println!("performance baseline passed");
            Ok(())
        }
    }
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

fn safe_install_prefix(prefix: &Path) -> Result<PathBuf, String> {
    let prefix = expand_user_path(prefix.to_path_buf())?
        .canonicalize()
        .unwrap_or_else(|_| prefix.to_path_buf());
    if prefix.parent().is_none() || prefix == Path::new("/") {
        return Err("refusing unsafe install prefix".into());
    }
    let text = prefix.to_string_lossy();
    if ["/usr", "/bin", "/sbin", "/etc", "/var"]
        .iter()
        .any(|item| text == *item)
    {
        return Err("refusing system install prefix; use a user-owned prefix".into());
    }
    Ok(prefix)
}

fn install_one(source: &Path, target: &Path) -> Result<String, String> {
    let digest = sha256_file(source)?;
    let parent = target.parent().ok_or("invalid install target")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    fs::copy(source, &temp).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    fs::rename(&temp, target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })?;
    atomic_write(&target.with_extension("install.sha256"), digest.as_bytes())?;
    Ok(digest)
}

fn self_command(command: SelfCommand) -> Result<(), String> {
    match command {
        SelfCommand::Version => {
            println!("helios {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        SelfCommand::Install {
            prefix,
            name,
            compat,
        } => {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err("invalid installation name".into());
            }
            let prefix = safe_install_prefix(&prefix)?;
            let source = env::current_exe()
                .map_err(|error| error.to_string())?
                .canonicalize()
                .map_err(|error| error.to_string())?;
            let target = prefix.join("bin").join(&name);
            let digest = install_one(&source, &target)?;
            if compat && name != "agent-cli" {
                install_one(&source, &prefix.join("bin/agent-cli"))?;
            }
            println!("installed {} ({})", target.display(), digest);
            if !env::var_os("PATH").is_some_and(|path| {
                env::split_paths(&path).any(|entry| entry == prefix.join("bin"))
            }) {
                println!("add {} to PATH", prefix.join("bin").display());
            }
            Ok(())
        }
        SelfCommand::Uninstall { name, confirm: _ } => {
            let home = env::var_os("HOME").ok_or("HOME is not set")?;
            let mut candidates = env::var_os("PATH")
                .map(|path| {
                    env::split_paths(&path)
                        .map(|dir| dir.join(&name))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            candidates.push(PathBuf::from(home.clone()).join(".local/bin").join(&name));
            let target = candidates
                .into_iter()
                .find(|path| path.is_file())
                .ok_or("installed binary not found")?;
            let marker = target.with_extension("install.sha256");
            let expected =
                fs::read_to_string(&marker).map_err(|_| "refusing to remove untracked binary")?;
            if sha256_file(&target)? != expected.trim() {
                return Err("installed binary identity does not match its marker".into());
            }
            let trash = PathBuf::from(home).join(".local/share/helios/trash");
            fs::create_dir_all(&trash).map_err(|error| error.to_string())?;
            let stamp = job_id();
            fs::rename(&target, trash.join(format!("{name}-{stamp}")))
                .map_err(|error| error.to_string())?;
            let _ = fs::rename(
                &marker,
                trash.join(format!("{name}-{stamp}.install.sha256")),
            );
            println!("moved {} to {}", target.display(), trash.display());
            Ok(())
        }
        SelfCommand::Update {
            source: Some(source),
            sha256: Some(expected),
        } => {
            if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("--sha256 must be a 64-character hexadecimal digest".into());
            }
            let source = source.canonicalize().map_err(|error| error.to_string())?;
            let actual = sha256_file(&source)?;
            if !actual.eq_ignore_ascii_case(&expected) {
                return Err("local update checksum mismatch; no mutation performed".into());
            }
            let target = env::current_exe().map_err(|error| error.to_string())?;
            let digest = install_one(&source, &target)?;
            println!("updated {} ({})", target.display(), digest);
            Ok(())
        }
        SelfCommand::Update {
            source: None,
            sha256: None,
        } => remote_self_update(),
        SelfCommand::Update { .. } => {
            Err("self update requires both --from PATH and --sha256 HEX".into())
        }
    }
}

fn remote_self_update() -> Result<(), String> {
    #[derive(Deserialize)]
    struct Release {
        assets: Vec<ReleaseAsset>,
    }
    #[derive(Deserialize)]
    struct ReleaseAsset {
        name: String,
        browser_download_url: String,
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent("helios-agent-self-update")
        .build()
        .map_err(|error| error.to_string())?;
    let mut request =
        client.get("https://api.github.com/repos/Shidori5417/helios-agent/releases/latest");
    if let Some(token) = env::var_os("GITHUB_TOKEN") {
        request = request.bearer_auth(token.to_string_lossy());
    }
    let response = request.send().map_err(|error| error.to_string())?;
    if response.status().as_u16() == 404 {
        return Err("no release available".into());
    }
    if !response.status().is_success() {
        return Err(format!(
            "release lookup failed with HTTP {}",
            response.status()
        ));
    }
    let release: Release = response.json().map_err(|error| error.to_string())?;
    let artifact = release
        .assets
        .iter()
        .find(|asset| {
            asset.name.contains("x86_64")
                && asset.name.contains("linux")
                && !asset.name.ends_with(".bundle")
        })
        .ok_or("release has no x86_64 Linux artifact")?;
    let checksums = release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .ok_or("release has no SHA256SUMS")?;
    let bundle = release
        .assets
        .iter()
        .find(|asset| asset.name.ends_with(".bundle"))
        .ok_or("release has no cosign bundle")?;
    let download = |url: &str| -> Result<Vec<u8>, String> {
        let mut request = client.get(url);
        if let Some(token) = env::var_os("GITHUB_TOKEN") {
            request = request.bearer_auth(token.to_string_lossy());
        }
        let response = request.send().map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "asset download failed with HTTP {}",
                response.status()
            ));
        }
        let bytes = response.bytes().map_err(|error| error.to_string())?;
        if bytes.len() > 100 * 1024 * 1024 {
            return Err("release asset exceeds 100 MiB bound".into());
        }
        Ok(bytes.to_vec())
    };
    let artifact_bytes = download(&artifact.browser_download_url)?;
    let checksum_text = String::from_utf8(download(&checksums.browser_download_url)?)
        .map_err(|_| "invalid checksum file".to_string())?;
    let expected = checksum_text
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == artifact.name).then(|| digest.to_string())
        })
        .ok_or("artifact is absent from SHA256SUMS")?;
    if !format!("{:x}", Sha256::digest(&artifact_bytes)).eq_ignore_ascii_case(&expected) {
        return Err("release checksum mismatch; no mutation performed".into());
    }
    let root = env::temp_dir().join(format!("helios-update-{}", job_id()));
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let artifact_path = root.join(&artifact.name);
    let bundle_path = root.join(&bundle.name);
    fs::write(&artifact_path, &artifact_bytes).map_err(|error| error.to_string())?;
    fs::write(&bundle_path, download(&bundle.browser_download_url)?)
        .map_err(|error| error.to_string())?;
    let verified = OsCommand::new("cosign").args(["verify-blob", "--bundle"]).arg(&bundle_path).args(["--certificate-identity", "https://github.com/Shidori5417/helios-agent/.github/workflows/release.yml@refs/heads/main", "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com"]).arg(&artifact_path).stdout(Stdio::null()).stderr(Stdio::null()).status();
    if !verified.is_ok_and(|status| status.success()) {
        let _ = fs::remove_dir_all(&root);
        return Err(
            "cosign verification failed or cosign is unavailable; no mutation performed".into(),
        );
    }
    let target = env::current_exe().map_err(|error| error.to_string())?;
    let result = install_one(&artifact_path, &target);
    let _ = fs::remove_dir_all(&root);
    result.map(|digest| println!("updated {} ({})", target.display(), digest))
}

fn read_trust_store_or_empty(path: &Path) -> Result<TrustStore, String> {
    if !path.exists() {
        return Ok(TrustStore {
            keys: Default::default(),
            revoked: Default::default(),
            disabled: Default::default(),
        });
    }
    load_trust_store(path).map_err(|error| error.to_string())
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

fn write_trust_store(path: &Path, store: &TrustStore) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(store).map_err(|error| error.to_string())?;
    atomic_write(path, &bytes)
}

fn trust_command(command: TrustCommand) -> Result<(), String> {
    match command {
        TrustCommand::List { path } => {
            let store = read_trust_store_or_empty(&path)?;
            for id in store.keys.keys() {
                let state = if store.revoked.contains(id) {
                    "revoked"
                } else if store.disabled.contains(id) {
                    "disabled"
                } else {
                    "enabled"
                };
                println!("{id}\t{state}");
            }
            Ok(())
        }
        TrustCommand::Show { path, id } => {
            let store = read_trust_store_or_empty(&path)?;
            let key = store.keys.get(&id).ok_or("trust key not found")?;
            let state = if store.revoked.contains(&id) {
                "revoked"
            } else if store.disabled.contains(&id) {
                "disabled"
            } else {
                "enabled"
            };
            println!("id={id} state={state} public_key={key}");
            Ok(())
        }
        TrustCommand::Add { path, id, key_file } => {
            if !valid_key_id(&id) {
                return Err("invalid trust key id".into());
            }
            let key = read_public_key_file(&key_file)?;
            let mut store = read_trust_store_or_empty(&path)?;
            if store.keys.contains_key(&id) || store.revoked.contains(&id) {
                return Err("trust key id already exists or is revoked".into());
            }
            store.keys.insert(id, key);
            write_trust_store(&path, &store)
        }
        TrustCommand::Disable { path, id } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.disabled.insert(id);
            write_trust_store(&path, &store)
        }
        TrustCommand::Enable { path, id } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if store.revoked.contains(&id) {
                return Err("revoked trust keys cannot be re-enabled".into());
            }
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.disabled.remove(&id);
            write_trust_store(&path, &store)
        }
        TrustCommand::Revoke { path, id } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.disabled.remove(&id);
            store.revoked.insert(id);
            write_trust_store(&path, &store)
        }
        TrustCommand::Remove {
            path,
            id,
            confirm: _,
        } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.keys.remove(&id);
            store.disabled.remove(&id);
            store.revoked.remove(&id);
            write_trust_store(&path, &store)
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct RegistryStateFile {
    #[serde(default)]
    registries: std::collections::BTreeMap<String, RegistryStateValue>,
}
#[derive(Debug, Deserialize, Serialize)]
struct RegistryStateValue {
    sequence: u64,
    digest: String,
}

fn registry_command(command: RegistryCommand) -> Result<(), String> {
    match command {
        RegistryCommand::Status { path, id } => {
            let state: RegistryStateFile = if path.exists() {
                serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?
            } else {
                RegistryStateFile::default()
            };
            for (name, value) in state.registries {
                if id.as_deref().is_none_or(|wanted| wanted == name) {
                    println!(
                        "{name}\tsequence={}\tdigest={}",
                        value.sequence, value.digest
                    );
                }
            }
            Ok(())
        }
        RegistryCommand::Reset {
            path,
            id,
            confirm: _,
        } => {
            let mut state: RegistryStateFile =
                serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            if state.registries.remove(&id).is_none() {
                return Err("registry id not found".into());
            }
            let home = env::var_os("HOME").ok_or("HOME is not set")?;
            let trash = PathBuf::from(home).join(".local/share/helios/trash");
            fs::create_dir_all(&trash).map_err(|error| error.to_string())?;
            fs::rename(&path, trash.join(format!("registry-state-{}", job_id())))
                .map_err(|error| error.to_string())?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?,
            )
        }
        RegistryCommand::Verify { source, trust } => {
            let store = load_trust_store(&trust).map_err(|error| error.to_string())?;
            let verified =
                verify_signed_registry(source, &store).map_err(|error| error.to_string())?;
            println!(
                "verified registry={} signer={} sequence={} expires_at={}",
                verified.registry_id, verified.key_id, verified.sequence, verified.expires_at
            );
            Ok(())
        }
    }
}

fn resolve_observability_session(
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

fn context_command(db_path: PathBuf, selector: Option<String>, json: bool) -> Result<(), String> {
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
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
                "turn={} budget={} estimated={} selected={} dropped={} truncated={} fingerprint={}",
                index + 1,
                record.budget,
                record.estimated_tokens,
                record.selected_count,
                record.dropped_count,
                record.truncated_count,
                record.fingerprint
            );
        }
    }
    Ok(())
}

fn usage_command(db_path: PathBuf, selector: Option<String>, json: bool) -> Result<(), String> {
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
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

fn usage_json(usage: &UsageRecord) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "total_tokens": usage.total_tokens,
    })
}

fn memory_command(command: MemoryCommand) -> Result<(), String> {
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?
        .display()
        .to_string();
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
        } => {
            let mut store = SqliteMemoryStore::open(db_path, 128, 16 * 1024)
                .map_err(|error| error.to_string())?;
            let id = store
                .put(MemoryEntry {
                    scope: workspace,
                    kind,
                    key,
                    content,
                    confidence: 1.0,
                    importance,
                    source_session_id: None,
                })
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
    }
    Ok(())
}

fn plugins_command(command: PluginCommand) -> Result<(), String> {
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    match command {
        PluginCommand::List => {
            let registry = PluginRegistry::discover(plugin_root(&workspace))
                .map_err(|error| error.to_string())?;
            for plugin in registry.plugins() {
                println!(
                    "{}\t{}\t{}",
                    plugin.manifest.name, plugin.manifest.version, plugin.enabled
                );
            }
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

fn load_startup_memory(
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

fn load_runtime_hooks(
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

struct StaticMemory(Vec<RetrievedMemory>);
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

fn verify_command(argv: Vec<String>) -> Result<(), String> {
    let workspace = env::current_dir().map_err(|error| error.to_string())?;
    let registry = ToolRegistry::new(workspace, PermissionMode::WorkspaceWrite)
        .map_err(|error| error.to_string())?;
    let result = registry
        .verify_argv(&argv)
        .map_err(|error| error.to_string())?;
    print!("{}", result.output);
    if result.status == ToolStatus::Ok {
        Ok(())
    } else {
        Err(format!("verification status: {:?}", result.status))
    }
}
fn diff_command() -> Result<(), String> {
    let workspace = env::current_dir().map_err(|error| error.to_string())?;
    let registry = ToolRegistry::new(workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    let result = registry.git_diff().map_err(|error| error.to_string())?;
    print!("{}", result.data);
    Ok(())
}
fn open_database(path: &PathBuf) -> Result<Database, String> {
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
    env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())
}

fn app_paths() -> Result<AppPaths, String> {
    AppPaths::discover(workspace_root()?).map_err(|error| error.to_string())
}

fn plugin_root(workspace: &Path) -> PathBuf {
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

fn load_runtime_policy(path: &Path) -> Result<RuntimePolicy, String> {
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

fn doctor_command() -> Result<(), String> {
    let workspace = workspace_root()?;
    println!("binary=helios version=0.1.0 compatibility=agent-cli");
    println!("workspace={}", workspace.display());
    println!("config={}", config_path()?.display());
    println!("config_valid={}", load_user_config(&config_path()?).is_ok());
    println!(
        "keyring_available={}",
        !KeyringSecretStore.get("openai").is_err()
    );
    println!("db_present={}", app_paths()?.db_path.is_file());
    for command in ["git", "rg"] {
        println!("{command}_available={}", command_available(command));
    }
    for provider in ["openai", "anthropic", "gemini", "generic"] {
        println!(
            "{provider}_credential_present={}",
            env::var_os(provider_env(provider)).is_some()
        );
    }
    Ok(())
}
fn command_available(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn completion_command(command: CompletionCommand) -> Result<(), String> {
    let (shell, source) = match &command {
        CompletionCommand::Generate { shell } | CompletionCommand::Install { shell, .. } => {
            (shell.clone(), completion_source(shell)?)
        }
    };
    match command {
        CompletionCommand::Generate { .. } => print!("{source}"),
        CompletionCommand::Install { path, .. } => {
            let target = path.unwrap_or_else(|| match shell.as_str() {
                "bash" => PathBuf::from("~/.local/share/bash-completion/completions/helios"),
                "zsh" => PathBuf::from("~/.zfunc/_helios"),
                _ => PathBuf::from("~/.config/fish/completions/helios.fish"),
            });
            if target.to_string_lossy().starts_with("~") {
                return Err("completion install requires an explicit absolute --path".into());
            }
            atomic_write(&target, source.as_bytes())?;
            println!("{}", target.display());
        }
    }
    Ok(())
}
fn completion_source(shell: &str) -> Result<String, String> {
    let source = match shell {
        "bash" => include_str!("../../../completions/agent-cli.bash"),
        "zsh" => include_str!("../../../completions/agent-cli.zsh"),
        "fish" => include_str!("../../../completions/agent-cli.fish"),
        other => return Err(format!("unsupported completion shell: {other}")),
    };
    for definition in command_registry() {
        if !source.contains(definition.name) {
            return Err(format!(
                "completion source is missing command {}",
                definition.name
            ));
        }
    }
    Ok(source.into())
}

fn runtime_command(command: RuntimeCommand) -> Result<(), String> {
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
        RuntimeCommand::Status(_) => unreachable!(),
    };
    let action = args.first().map(String::as_str).unwrap_or("list");
    let path = config_path()?;
    if section == "mcp" || section == "lsp" {
        if action == "test" {
            return configured_server_test(section, &args, &path);
        }
        return runtime_server_array_command(section, &args, &path);
    }
    if section == "permissions" {
        return runtime_permissions_command(&args, &path);
    }
    if section == "tui" {
        return tui_config_command(&args, &path);
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

fn enabled_plugin_names(path: &Path) -> Result<Vec<String>, String> {
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

fn set_enabled_plugin(path: &Path, id: &str, enabled: bool) -> Result<(), String> {
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

fn parse_hook_event(name: &str) -> Result<HookEvent, String> {
    match name.to_ascii_lowercase().as_str() {
        "sessionstart" | "session_start" => Ok(HookEvent::SessionStart),
        "userpromptsubmit" | "user_prompt_submit" => Ok(HookEvent::UserPromptSubmit),
        "pretooluse" | "pre_tool_use" => Ok(HookEvent::PreToolUse),
        "posttooluse" | "post_tool_use" => Ok(HookEvent::PostToolUse),
        "preedit" | "pre_edit" => Ok(HookEvent::PreEdit),
        "postedit" | "post_edit" => Ok(HookEvent::PostEdit),
        "permissionrequest" | "permission_request" => Ok(HookEvent::PermissionRequest),
        "precompact" | "pre_compact" => Ok(HookEvent::PreCompact),
        "stop" => Ok(HookEvent::Stop),
        _ => Err(format!("unknown hook event: {name}")),
    }
}

fn runtime_extensions_command(section: &str, args: &[String], path: &Path) -> Result<(), String> {
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
                _ => Err(format!("unsupported hooks action: {action}")),
            }
        }
        "skills" => {
            let permissions = PermissionEngine::new(&workspace, PermissionMode::ReadOnly)
                .map_err(|e| e.to_string())?;
            let mut roots = vec![workspace.join("skills"), workspace.join(".helios/skills")];
            if !workspace.join(".helios/skills").is_dir() {
                roots.push(workspace.join(".agent/skills"));
            }
            if let Some(root) = value_after(args, "--root")? {
                roots = vec![workspace.join(root)];
            }
            let name = args.get(1).filter(|value| !value.starts_with('-')).cloned();
            let mut found = Vec::new();
            for root in roots {
                if root.exists() {
                    found.push(
                        SkillCatalog::discover(root, permissions.clone())
                            .map_err(|e| e.to_string())?,
                    );
                }
            }
            match action {
                "list" => {
                    for catalog in &found {
                        for skill in catalog.metadata() {
                            println!(
                                "{}\t{}\tauto_invoke={}\trisk={}",
                                skill.name, skill.description, skill.auto_invoke, skill.risk
                            );
                        }
                    }
                    Ok(())
                }
                "show" => {
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

fn subagent_budgets(path: &Path) -> Result<SubagentBudgets, String> {
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

fn subagent_concurrency(path: &Path) -> Result<usize, String> {
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

fn subagents_command(args: &[String], path: &Path, workspace: &Path) -> Result<(), String> {
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
            let mut task = TaskSpec::new("cli-subagent", objective).with_scope(vec![scope]).with_context(context).with_permission(policy.mode).with_budgets(subagent_budgets(path)?);
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

struct RealSubagentExecutor {
    provider: String,
    model: String,
    base_url: Option<String>,
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
                    ),
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
                    ),
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
                    ),
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
                    ),
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

fn execute_subagent_provider<P: ModelProvider>(
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
        task.permission_mode,
    ) {
        Ok(tools) => tools,
        Err(error) => return failed_subagent(task, error),
    };
    let mut agent = AgentLoop::new(provider).with_context_budget(task.budgets.max_tokens);
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

fn failed_subagent(task: &TaskSpec, error: String) -> TaskResult {
    failed_result(task, 0, 0, 0, StopReason::PermissionDenied).with_summary(error)
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
        let mut client = McpClient::new(server.config, policy, 64 * 1024);
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

fn runtime_permissions_command(args: &[String], path: &Path) -> Result<(), String> {
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

fn safe_control_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn runtime_server_array_command(section: &str, args: &[String], path: &Path) -> Result<(), String> {
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
mod tests {
    use super::*;

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
            provider: "fake".into(),
            model: "gpt-5".into(),
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
        let root =
            std::env::temp_dir().join(format!("helios-runtime-config-{}", std::process::id()));
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
        assert_eq!(value["lsp_server"][0]["name"].as_str(), Some("rust"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn permission_mode_is_consumed_from_runtime_config() {
        let root =
            std::env::temp_dir().join(format!("helios-permission-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let path = root.join("config.toml");
        runtime_permissions_command(&["set-mode".into(), "read-only".into()], &path)
            .expect("set mode");
        assert_eq!(
            load_runtime_policy(&path).expect("load mode").mode,
            PermissionMode::ReadOnly
        );
        assert!(
            runtime_permissions_command(&["set-mode".into(), "unsafe".into()], &path,).is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_policy_normalizes_hosts_and_rejects_secret_env_names() {
        let root =
            std::env::temp_dir().join(format!("helios-policy-config-{}", std::process::id()));
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
}
