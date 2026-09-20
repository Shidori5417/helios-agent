use agent_context::LocalCompactor;
use agent_core::{
    AgentLoop, EventSink, HookRunner, LifecycleEvent, MemoryRetriever, RetrievedMemory,
    ToolExecution, ToolExecutor, ToolFailure,
};
use agent_lsp::{load_config as load_lsp_config, LspClient, LspServerConfig, StdioTransport};
use agent_mcp::{load_config as load_mcp_config, HostAllowlist, McpClient, PermissionPolicy};
use agent_memory::{MemoryEntry, MemoryStore, SqliteMemoryStore};
use agent_plugins::{
    fetch_registry_manifest, install_from_registry, load_signed_registry, load_trust_store,
    uninstall_exact, verify_registry_entry,
};
use agent_plugins::{HookEvent, PluginRegistry};
use agent_protocol::{Message, Role};
use agent_providers::{
    AnthropicProvider, FakeProvider, GeminiProvider, ModelProvider, OpenAiCompatibleProvider,
    OpenAiProvider,
};
use agent_sandbox::PermissionEngine;
use agent_sandbox::PermissionMode;
use agent_storage::{ContextRecord, Database, UsageRecord};
use agent_terminal::CancellationToken;
use agent_tools::{ToolRegistry, ToolStatus};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
        Arc,
    },
};

#[tokio::main]
async fn main() -> ExitCode {
    match parse_args(env::args().skip(1).collect()) {
        Ok(Command::Help) => {
            print_help();
            ExitCode::SUCCESS
        }
        Ok(Command::Run(options)) => match run_command(options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Sessions { db_path }) => match list_sessions(db_path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Ok(Command::Search { db_path, query }) => match search_sessions(db_path, &query) {
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
    eprintln!("agent error: {error}");
    ExitCode::from(1)
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
    ) -> Result<Self, String> {
        let mut mcp = Vec::new();
        for server in servers {
            let engine = PermissionEngine::new(workspace, PermissionMode::WorkspaceWrite)
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
        let permissions = PermissionEngine::new(workspace, PermissionMode::WorkspaceWrite)
            .map_err(|error| error.to_string())?;
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
    Help,
    Run(RunOptions),
    Sessions {
        db_path: PathBuf,
    },
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
#[derive(Debug, PartialEq, Eq)]
enum RuntimeCommand {
    Providers(Vec<String>),
    Permissions(Vec<String>),
    Mcp(Vec<String>),
    Lsp(Vec<String>),
    Hooks(Vec<String>),
    Skills(Vec<String>),
    Subagents(Vec<String>),
    Status,
}
#[derive(Debug, PartialEq, Eq)]
enum ConfigCommand {
    List,
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
#[derive(Debug, PartialEq, Eq)]
struct RunOptions {
    provider: String,
    model: String,
    base_url: Option<String>,
    prompt: String,
    db_path: PathBuf,
    resume: Option<String>,
    tui: bool,
    config: Option<PathBuf>,
}

fn parse_args(args: Vec<String>) -> Result<Command, String> {
    if args.is_empty() || args[0] == "run" {
        return parse_run(if args.first().is_some_and(|arg| arg == "run") {
            &args[1..]
        } else {
            &args
        });
    }
    match args[0].as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "sessions" => Ok(Command::Sessions {
            db_path: path_option(&args[1..], "--db-path")?,
        }),
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
        "status" => Ok(Command::Runtime(RuntimeCommand::Status)),
        _ => parse_run(&args),
    }
}

fn parse_config_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let command = match action {
        "list" => ConfigCommand::List,
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
    let trust_store = PathBuf::from(
        value_after(&args[1..], "--trust-store")?.ok_or("plugin command requires --trust-store")?,
    );
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
fn parse_run(args: &[String]) -> Result<Command, String> {
    let mut options = RunOptions {
        provider: "fake".into(),
        model: "gpt-5".into(),
        base_url: None,
        prompt: String::new(),
        db_path: default_db_path()?,
        resume: None,
        tui: false,
        config: None,
    };
    let mut prompt = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--provider" => options.provider = required_value(args, &mut index, "--provider")?,
            "--model" => options.model = required_value(args, &mut index, "--model")?,
            "--base-url" => {
                options.base_url = Some(required_value(args, &mut index, "--base-url")?)
            }
            "--db-path" => {
                options.db_path = PathBuf::from(required_value(args, &mut index, "--db-path")?)
            }
            "--resume" => options.resume = Some(required_value(args, &mut index, "--resume")?),
            "--tui" => options.tui = true,
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
    Ok(env::current_dir()
        .map_err(|error| error.to_string())?
        .join(".agent/state.db"))
}

fn run_command(options: RunOptions) -> Result<(), String> {
    if options.tui && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal()) {
        return Err("--tui requires an interactive TTY".into());
    }
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut database = open_database(&options.db_path)?;
    let workspace_id = workspace.display().to_string();
    let (session_id, objective, messages) = if let Some(selector) = options.resume.as_deref() {
        let record = if selector == "latest" {
            database
                .latest_session_any()
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
            .session_messages(record.id)
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
    let config_path = options.config.clone().or_else(|| {
        let path = workspace.join(".agent/config.toml");
        path.is_file().then_some(path)
    });
    let servers = config_path
        .as_ref()
        .map(|path| load_mcp_config(path, &workspace))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let lsp_servers = config_path
        .as_ref()
        .map(|path| load_lsp_config(path, &workspace))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let cancellation = CancellationToken::default();
    let builtin = ToolRegistry::new(&workspace, PermissionMode::WorkspaceWrite)
        .map_err(|error| error.to_string())?
        .with_cancellation(cancellation.clone());
    let mut tools = RuntimeExecutor::new(
        builtin,
        servers,
        &workspace,
        cancellation.clone(),
        lsp_servers,
    )?;
    let hooks = load_runtime_hooks(&workspace, config_path.as_deref(), cancellation.clone())?;
    let startup_memory = load_startup_memory(&options.db_path, &workspace_id, &options.prompt)?;
    if options.tui {
        let (sender, receiver) = agent_tui::bounded_event_channel(128);
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
            agent_tui::run_channel(receiver, cancellation.clone())
                .map_err(|error| error.to_string())?;
            producer
                .join()
                .map_err(|_| "agent producer panicked".to_owned())?
                .map(|_| ())
        });
    }
    let events = run_selected_provider(
        &options,
        &mut tools,
        &mut database,
        session_id,
        objective,
        messages,
        startup_memory,
        hooks,
        None,
        !options.tui,
        cancellation,
    )?;
    for event in events {
        if !matches!(event, agent_protocol::Event::ModelDelta(_)) {
            println!("{event:?}");
        }
    }
    Ok(())
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
) -> Result<Vec<agent_protocol::Event>, String> {
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
            OpenAiProvider::from_env_with_base_url(options.model.clone(), options.base_url.clone())
                .map_err(|error| error.to_string())?,
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
            AnthropicProvider::from_env_with_base_url(
                options.model.clone(),
                options.base_url.clone(),
            )
            .map_err(|error| error.to_string())?,
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
            GeminiProvider::from_env_with_base_url(options.model.clone(), options.base_url.clone())
                .map_err(|error| error.to_string())?,
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
            OpenAiCompatibleProvider::from_env_with_base_url(
                options.model.clone(),
                options.base_url.clone(),
            )
            .map_err(|error| error.to_string())?,
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
) -> Result<Vec<agent_protocol::Event>, String> {
    let previous = messages.len();
    let memory_scope = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?
        .display()
        .to_string();
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
    Ok(sink.events)
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

fn list_sessions(db_path: PathBuf) -> Result<(), String> {
    let workspace = env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())?;
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
            let registry = PluginRegistry::discover(workspace.join(".agent/plugins"))
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
            let state = workspace.join(".agent/registry-state.json");
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
) -> Result<Option<Box<dyn HookRunner>>, String> {
    let plugins_root = workspace.join(".agent/plugins");
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
    if !registry.plugins().iter().any(|plugin| plugin.enabled) {
        return Ok(None);
    }
    let permissions = PermissionEngine::new(workspace, PermissionMode::WorkspaceWrite)
        .map_err(|error| error.to_string())?;
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
    println!("agent-cli [run] [OPTIONS] [PROMPT]\n\nCommands:\n  init [--force]       Initialize workspace control files\n  config list|get|set|unset|path|validate\n                       Manage typed workspace configuration\n  auth status|login|logout PROVIDER\n                       Manage provider credentials without printing secrets\n  doctor               Show local configuration/tool/provider diagnostics\n  completion generate|install SHELL\n                       Generate or install shell completion\n  run                  Run one agent turn (default)\n  sessions             List sessions for this workspace\n  search --query TEXT Search persisted messages\n  context [--session ID|latest] [--json]\n                       Show durable context-plan summaries\n  usage [--session ID|latest] [--json]\n                       Show normalized durable token usage\n  memory list|search|add|supersede|delete\n                       Manage bounded workspace memory (delete needs exact --id)\n  plugins list|verify|install|update|uninstall\n                       Manage explicitly trusted signed plugins\n  verify -- PROGRAM    Run explicit verification argv\n  diff                 Show bounded read-only git diff\n\nOptions:\n  --provider NAME      fake, openai, anthropic, gemini, generic\n  --model NAME         Provider model\n  --base-url URL       Provider endpoint override\n  --db-path PATH       SQLite session database\n  --resume ID|latest   Resume a workspace session\n  --registry PATH|URL  Signed plugin registry source\n  --trust-store PATH   Trusted public-key JSON store for plugins\n  --tui                Use the interactive TTY UI (TTY only)\n  -h, --help           Show this help");
}

fn workspace_root() -> Result<PathBuf, String> {
    env::current_dir()
        .map_err(|error| error.to_string())?
        .canonicalize()
        .map_err(|error| error.to_string())
}

fn config_path() -> Result<PathBuf, String> {
    Ok(workspace_root()?.join(".agent/config.toml"))
}

fn init_command(force: bool) -> Result<(), String> {
    let path = config_path()?;
    fs::create_dir_all(path.parent().ok_or("invalid config path")?).map_err(|e| e.to_string())?;
    if path.exists() && !force {
        return Err(".agent/config.toml already exists; use --force to overwrite".into());
    }
    atomic_write(&path, b"# Helios Agent workspace configuration\n")?;
    println!("{}", path.display());
    Ok(())
}

fn valid_config_key(key: &str) -> bool {
    matches!(key, "provider" | "model" | "base_url")
}

fn load_user_config(path: &Path) -> Result<UserConfig, String> {
    if !path.exists() {
        return Ok(UserConfig::default());
    }
    toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("invalid config: {e}"))
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

trait SecretStore {
    fn set(&self, provider: &str, token: &str) -> Result<(), String>;
    fn delete(&self, provider: &str) -> Result<(), String>;
}
struct UnavailableSecretStore;
impl SecretStore for UnavailableSecretStore {
    fn set(&self, _provider: &str, _token: &str) -> Result<(), String> {
        Err("Secret Service/keyring is unavailable; refusing plaintext fallback".into())
    }
    fn delete(&self, _provider: &str) -> Result<(), String> {
        Err("Secret Service/keyring is unavailable".into())
    }
}

fn auth_command(command: AuthCommand) -> Result<(), String> {
    let store = UnavailableSecretStore;
    match command {
        AuthCommand::Status { provider } => {
            let env_name = provider_env(&provider);
            println!(
                "provider={provider} credential_source=keyring-or-env env_present={}",
                env::var_os(env_name).is_some()
            );
            Ok(())
        }
        AuthCommand::Login {
            provider,
            token_stdin,
        } => {
            if !token_stdin || io::stdin().is_terminal() {
                return Err(
                    "login requires non-TTY --token-stdin; token arguments are forbidden".into(),
                );
            }
            let mut token = String::new();
            io::stdin()
                .read_to_string(&mut token)
                .map_err(|e| e.to_string())?;
            let token = token.trim();
            if token.is_empty() {
                return Err("empty token".into());
            }
            store.set(&provider, token)
        }
        AuthCommand::Logout { provider } => store.delete(&provider),
    }
}

fn doctor_command() -> Result<(), String> {
    let workspace = workspace_root()?;
    println!("binary=agent-cli version=0.1.0");
    println!("workspace={}", workspace.display());
    println!("config={}", config_path()?.display());
    println!("config_valid={}", load_user_config(&config_path()?).is_ok());
    println!("db_present={}", workspace.join(".agent/state.db").is_file());
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
                "bash" => PathBuf::from("~/.local/share/bash-completion/completions/agent-cli"),
                "zsh" => PathBuf::from("~/.zfunc/_agent-cli"),
                _ => PathBuf::from("~/.config/fish/completions/agent-cli.fish"),
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
    let path = workspace_root()?.join(format!("completions/agent-cli.{shell}"));
    fs::read_to_string(path).map_err(|e| e.to_string())
}

fn runtime_command(command: RuntimeCommand) -> Result<(), String> {
    if matches!(&command, RuntimeCommand::Status) {
        println!("workspace={}", workspace_root()?.display());
        println!("config={}", config_path()?.display());
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
        RuntimeCommand::Status => unreachable!(),
    };
    let action = args.first().map(String::as_str).unwrap_or("list");
    let path = config_path()?;
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
    if matches!(section, "mcp" | "lsp") && action == "test" {
        return Err("external MCP/LSP tests are disabled in this offline control plane; use --help for configuration".into());
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

fn safe_control_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
    fn help_and_verify_parse() {
        assert_eq!(
            parse_args(vec!["--help".into()]).expect("help"),
            Command::Help
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
}
