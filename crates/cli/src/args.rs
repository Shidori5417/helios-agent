//! Command-line grammar: the `Command` model and the hand-written parser.

use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Version,
    Help,
    Interactive,
    ContextHelp(Vec<String>),
    Resume(Option<String>),
    Models {
        json: bool,
    },
    Review {
        focus: Option<String>,
        json: bool,
    },
    Run(RunOptions),
    Sessions {
        command: SessionCommand,
        db_path: PathBuf,
    },
    Database(DatabaseCommand),
    Audit(AuditCommand),
    Jobs(JobCommand),
    Tasks(TaskCommand),
    Agents(TaskCommand),
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
    Doctor {
        json: bool,
    },
    Completion(CompletionCommand),
    Runtime(RuntimeCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandDefinition {
    pub(crate) name: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) description: &'static str,
    pub(crate) category: &'static str,
    pub(crate) arguments: &'static str,
    pub(crate) destructive: bool,
    pub(crate) requires_agent: bool,
    pub(crate) local: bool,
    pub(crate) handler_id: &'static str,
    pub(crate) source: &'static str,
}

pub(crate) fn command_registry() -> Vec<CommandDefinition> {
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
            name: "resume",
            aliases: &[],
            description: "resume a workspace session in the interactive TTY",
            category: "sessions",
            arguments: "[latest|ID|TITLE]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "resume",
            source: "builtin",
        },
        CommandDefinition {
            name: "models",
            aliases: &[],
            description: "list local provider/model metadata",
            category: "model",
            arguments: "[--json]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "models",
            source: "builtin",
        },
        CommandDefinition {
            name: "review",
            aliases: &[],
            description: "run a bounded read-only current-diff review",
            category: "diagnostics",
            arguments: "[security|performance|tests] [--json]",
            destructive: false,
            requires_agent: false,
            local: true,
            handler_id: "review",
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
            name: "tasks",
            aliases: &[],
            description: "manage durable background agent tasks",
            category: "runtime",
            arguments: "bg|list|show|status|cancel|adopt",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "tasks",
            source: "builtin",
        },
        CommandDefinition {
            name: "agents",
            aliases: &[],
            description: "run and inspect isolated subagents",
            category: "runtime",
            arguments: "run|list|status|show|cancel|adopt",
            destructive: true,
            requires_agent: false,
            local: true,
            handler_id: "agents",
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
pub(crate) enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug)]
pub(crate) struct RunOutcome {
    pub(crate) turn_id: i64,
    pub(crate) events: Vec<agent_protocol::Event>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionCommand {
    List,
    Show(i64),
    Archive(i64, bool),
    Delete(i64),
    Export(i64, PathBuf),
    Import(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DatabaseCommand {
    Backup { input: PathBuf, output: PathBuf },
    Restore { input: PathBuf, workspace: PathBuf },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuditCommand {
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
pub(crate) enum JobCommand {
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
pub(crate) enum TaskCommand {
    Background(String),
    Run {
        objective: String,
        scope: Option<String>,
        max_iterations: Option<u32>,
        max_tokens: Option<u64>,
    },
    List,
    Show(String),
    Status(String),
    Cancel(String),
    Adopt(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EvalCommand {
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
pub(crate) enum PerfCommand {
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
pub(crate) enum SelfCommand {
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
pub(crate) enum TrustCommand {
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
pub(crate) enum RegistryCommand {
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
pub(crate) enum RuntimeCommand {
    Providers(Vec<String>),
    Permissions(Vec<String>),
    Mcp(Vec<String>),
    Lsp(Vec<String>),
    Hooks(Vec<String>),
    Skills(Vec<String>),
    Subagents(Vec<String>),
    Tui(Vec<String>),
    Tools(Vec<String>),
    Status(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigCommand {
    List,
    Sources,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
    Path,
    Validate,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuthCommand {
    Status { provider: String },
    Login { provider: String, token_stdin: bool },
    Logout { provider: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompletionCommand {
    Generate {
        shell: String,
    },
    Install {
        shell: String,
        path: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct UserConfig {
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) base_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimePolicy {
    pub(crate) mode: PermissionMode,
    pub(crate) allowed_network_hosts: Vec<String>,
    pub(crate) env_allow: Vec<String>,
    pub(crate) env_deny: Vec<String>,
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
    pub(crate) fn filter_env(&self, names: &mut Vec<String>) {
        names.retain(|name| {
            !self.env_deny.iter().any(|denied| denied == name)
                && (self.env_allow.is_empty()
                    || self.env_allow.iter().any(|allowed| allowed == name))
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MemoryCommand {
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
        pending: bool,
    },
    Supersede {
        db_path: PathBuf,
        id: i64,
    },
    Delete {
        db_path: PathBuf,
        id: i64,
    },
    Pending {
        db_path: PathBuf,
        limit: usize,
    },
    Approve {
        db_path: PathBuf,
        id: i64,
    },
    Reject {
        db_path: PathBuf,
        id: i64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PluginCommand {
    List,
    Info {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Reload,
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
pub(crate) struct RunOptions {
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Workspace-relative image paths attached to the prompt (`--image`).
    pub(crate) images: Vec<String>,
    pub(crate) effort: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) prompt: String,
    pub(crate) db_path: PathBuf,
    pub(crate) resume: Option<String>,
    pub(crate) tui: bool,
    pub(crate) config: Option<PathBuf>,
    pub(crate) provider_explicit: bool,
    pub(crate) model_explicit: bool,
    pub(crate) base_url_explicit: bool,
    pub(crate) output_format: OutputFormat,
    pub(crate) exec: bool,
    pub(crate) event_sender: Option<SyncSender<agent_protocol::Event>>,
    pub(crate) approval: Option<ApprovalBroker>,
    pub(crate) steering: Option<SteeringChannel>,
}

impl PartialEq for RunOptions {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.effort == other.effort
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

pub(crate) fn default_run_options(
    prompt: String,
    db_path: PathBuf,
    resume: Option<String>,
    event_sender: Option<SyncSender<agent_protocol::Event>>,
) -> RunOptions {
    RunOptions {
        images: Vec::new(),
        provider: "fake".into(),
        model: "gpt-5".into(),
        effort: None,
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
        steering: None,
    }
}

pub(crate) fn parse_args(args: Vec<String>) -> Result<Command, String> {
    let args = hoist_run_subcommand(extract_workspace_flag(args)?);
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
        let known = command_registry().iter().any(|definition| {
            definition.name == args[0] || definition.aliases.iter().any(|alias| *alias == args[0])
        });
        if !known {
            return Err(format!("unknown command: {}", args[0]));
        }
        let path = args
            .iter()
            .take_while(|arg| *arg != "--help" && *arg != "-h")
            .cloned()
            .collect::<Vec<_>>();
        return Ok(Command::ContextHelp(path));
    }
    match args[0].as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "resume" => Ok(Command::Resume(args.get(1).cloned())),
        "models" => Ok(Command::Models {
            json: args.iter().any(|arg| arg == "--json"),
        }),
        "review" => Ok(Command::Review {
            focus: args.get(1).filter(|value| !value.starts_with('-')).cloned(),
            json: args.iter().any(|arg| arg == "--json"),
        }),
        "sessions" => parse_sessions_command(&args[1..]),
        "database" => parse_database_command(&args[1..]),
        "audit" => parse_audit_command(&args[1..]),
        "jobs" => parse_jobs_command(&args[1..]),
        "tasks" => parse_tasks_command(&args[1..]),
        "agents" => parse_agents_command(&args[1..]),
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
        "doctor" => Ok(Command::Doctor {
            json: args.iter().any(|arg| arg == "--json"),
        }),
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
        "tools" => Ok(Command::Runtime(RuntimeCommand::Tools(args[1..].to_vec()))),
        "status" => Ok(Command::Runtime(RuntimeCommand::Status(args[1..].to_vec()))),
        _ => parse_run(&args, false),
    }
}

/// Moves an explicit `run`/`exec` subcommand that follows leading run options
/// (for example `helios --db-path X exec PROMPT`) to the front, so it is not
/// mistaken for prompt text. Only known run options are skipped; anything
/// else leaves the argument order untouched.
pub(crate) fn hoist_run_subcommand(mut args: Vec<String>) -> Vec<String> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--provider" | "--model" | "--effort" | "--base-url" | "--db-path" | "--resume"
            | "--output-format" | "--config" => index += 2,
            "--tui" => index += 1,
            _ => break,
        }
    }
    if index > 0
        && args
            .get(index)
            .is_some_and(|arg| matches!(arg.as_str(), "run" | "exec" | "r" | "x"))
    {
        let subcommand = args.remove(index);
        args.insert(0, subcommand);
    }
    args
}

pub(crate) fn extract_workspace_flag(args: Vec<String>) -> Result<Vec<String>, String> {
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

pub(crate) fn parse_config_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_auth_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn exact_id(args: &[String], action: &str) -> Result<i64, String> {
    args.get(1)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .or(value_after(args, "--id")?)
        .ok_or_else(|| format!("sessions {action} requires exact id"))?
        .parse()
        .map_err(|_| "id must be an integer".into())
}

pub(crate) fn parse_sessions_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_database_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_audit_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_jobs_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_tasks_command(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    let command = match action {
        "bg" | "background" => {
            let prompt = args[1..].join(" ").trim().to_owned();
            if prompt.is_empty() {
                return Err("tasks bg requires a prompt".into());
            }
            TaskCommand::Background(prompt)
        }
        "list" => TaskCommand::List,
        "show" => TaskCommand::Show(args.get(1).cloned().ok_or("tasks show requires ID")?),
        "status" => TaskCommand::Status(args.get(1).cloned().ok_or("tasks status requires ID")?),
        "cancel" => TaskCommand::Cancel(args.get(1).cloned().ok_or("tasks cancel requires ID")?),
        "adopt" => TaskCommand::Adopt(args.get(1).cloned().ok_or("tasks adopt requires ID")?),
        other => return Err(format!("unknown tasks command: {other}")),
    };
    Ok(Command::Tasks(command))
}

pub(crate) fn parse_agents_command(args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Ok(Command::Agents(TaskCommand::List));
    }
    if args.first().map(String::as_str) == Some("run") {
        let objective = value_after(&args[1..], "--objective")?
            .or_else(|| args.get(1).filter(|value| !value.starts_with('-')).cloned())
            .ok_or("agents run requires --objective TEXT")?;
        return Ok(Command::Agents(TaskCommand::Run {
            objective,
            scope: value_after(&args[1..], "--scope")?,
            max_iterations: value_after(&args[1..], "--max-iterations")?
                .and_then(|v| v.parse().ok()),
            max_tokens: value_after(&args[1..], "--max-tokens")?.and_then(|v| v.parse().ok()),
        }));
    }
    parse_tasks_command(args).map(|command| match command {
        Command::Tasks(task) => Command::Agents(task),
        _ => unreachable!(),
    })
}

pub(crate) fn parse_eval_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_perf_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_self_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn default_trust_path() -> Result<PathBuf, String> {
    Ok(app_paths()?.trust_store)
}

pub(crate) fn trust_path(args: &[String]) -> Result<PathBuf, String> {
    Ok(value_after(args, "--trust-store")?
        .map(PathBuf::from)
        .unwrap_or(default_trust_path()?))
}

pub(crate) fn parse_trust_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_registry_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_completion_command(args: &[String]) -> Result<Command, String> {
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

pub(crate) fn parse_observability_command(
    args: &[String],
    context: bool,
) -> Result<Command, String> {
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

pub(crate) fn parse_memory(args: &[String]) -> Result<Command, String> {
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
            pending: args.iter().any(|arg| arg == "--pending"),
        })),
        "supersede" => Ok(Command::Memory(MemoryCommand::Supersede {
            db_path,
            id: required_id(&args[1..], "--id")?,
        })),
        "delete" => Ok(Command::Memory(MemoryCommand::Delete {
            db_path,
            id: required_id(&args[1..], "--id")?,
        })),
        "pending" => Ok(Command::Memory(MemoryCommand::Pending { db_path, limit })),
        "approve" => Ok(Command::Memory(MemoryCommand::Approve {
            db_path,
            id: required_id(&args[1..], "approve")?,
        })),
        "reject" => Ok(Command::Memory(MemoryCommand::Reject {
            db_path,
            id: required_id(&args[1..], "reject")?,
        })),
        other => Err(format!("unknown memory command: {other}")),
    }
}

pub(crate) fn required_id(args: &[String], name: &str) -> Result<i64, String> {
    value_after(args, name)?
        .ok_or_else(|| format!("{name} requires an exact integer id"))?
        .parse::<i64>()
        .map_err(|_| format!("{name} requires an exact integer id"))
}

pub(crate) fn parse_plugins(args: &[String]) -> Result<Command, String> {
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "list" {
        return Ok(Command::Plugins(PluginCommand::List));
    }
    if action == "reload" {
        return Ok(Command::Plugins(PluginCommand::Reload));
    }
    let id = value_after(&args[1..], "--id")?.ok_or("plugin command requires --id")?;
    if action == "info" {
        return Ok(Command::Plugins(PluginCommand::Info { id }));
    }
    if action == "enable" {
        return Ok(Command::Plugins(PluginCommand::Enable { id }));
    }
    if action == "disable" {
        return Ok(Command::Plugins(PluginCommand::Disable { id }));
    }
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

pub(crate) fn parse_run(args: &[String], exec: bool) -> Result<Command, String> {
    let mut options = RunOptions {
        provider: "fake".into(),
        model: "gpt-5".into(),
        images: Vec::new(),
        effort: None,
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
        steering: None,
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
            "--effort" => {
                let value = required_value(args, &mut index, "--effort")?;
                validate_effort(&value)?;
                options.effort = Some(value);
            }
            "--base-url" => {
                options.base_url = Some(required_value(args, &mut index, "--base-url")?);
                options.base_url_explicit = true
            }
            "--db-path" => {
                options.db_path = PathBuf::from(required_value(args, &mut index, "--db-path")?)
            }
            "--resume" => options.resume = Some(required_value(args, &mut index, "--resume")?),
            "--image" => options
                .images
                .push(required_value(args, &mut index, "--image")?),
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

pub(crate) fn required_value(
    args: &[String],
    index: &mut usize,
    name: &str,
) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| format!("{name} requires a value"))
}

/// Builds provider request tuning from the session effort and the optional
/// `HELIOS_MAX_OUTPUT_TOKENS` override (useful for models with small output caps).
pub(crate) fn request_tuning(effort: Option<&str>) -> RequestTuning {
    RequestTuning {
        effort: effort.map(str::to_owned),
        max_output_tokens: env::var("HELIOS_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|value| *value > 0),
    }
}

pub(crate) fn validate_effort(value: &str) -> Result<(), String> {
    if matches!(
        value,
        "auto" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        Ok(())
    } else {
        Err("--effort must be auto, minimal, low, medium, high, xhigh, or max".into())
    }
}

pub(crate) fn value_after(args: &[String], name: &str) -> Result<Option<String>, String> {
    args.iter()
        .position(|arg| arg == name)
        .map(|index| {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        })
        .transpose()
}

pub(crate) fn path_option(args: &[String], name: &str) -> Result<PathBuf, String> {
    Ok(value_after(args, name)?
        .map(PathBuf::from)
        .unwrap_or(default_db_path()?))
}
