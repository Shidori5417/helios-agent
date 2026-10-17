mod filter;
mod web;

use agent_sandbox::{PermissionClass, PermissionEngine, PermissionError, PermissionMode};
use agent_terminal::{run_with_hook, CancellationToken, CommandSpec, CommandStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

static OUTPUT_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub const CORE_TOOL_NAMES: &[&str] = &[
    "read_file",
    "search_files",
    "glob",
    "apply_patch",
    "shell",
    "process",
    "update_plan",
];

#[derive(Clone, Debug)]
pub struct ToolMetadata {
    pub id: String,
    pub version: &'static str,
    pub short_description: &'static str,
    pub input_schema: Value,
    pub permission_class: PermissionClass,
    pub context_cost_hint: usize,
    pub supports_parallel: bool,
    pub output_budget: usize,
    pub timeout: Duration,
    pub cancellation: &'static str,
    pub logging_policy: &'static str,
    pub secret_policy: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Timeout,
    Cancelled,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: u64,
    pub class: PermissionClass,
    pub risk: &'static str,
    pub action: String,
}

#[derive(Debug)]
struct PendingApproval {
    request: ApprovalRequest,
    response: mpsc::SyncSender<ApprovalDecision>,
}

/// Bounded, fail-closed approval bridge for interactive frontends. The broker
/// emits only redacted request text and never grants policy access by itself.
#[derive(Clone, Debug)]
pub struct ApprovalBroker {
    events: mpsc::SyncSender<agent_protocol::Event>,
    pending: Arc<Mutex<HashMap<u64, PendingApproval>>>,
    session: Arc<Mutex<HashMap<String, bool>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    timeout: Duration,
}

impl ApprovalBroker {
    pub fn new(events: mpsc::SyncSender<agent_protocol::Event>) -> Self {
        Self {
            events,
            pending: Arc::new(Mutex::new(HashMap::new())),
            session: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            timeout: Duration::from_secs(60),
        }
    }

    pub fn decide(&self, id: u64, decision: ApprovalDecision) -> bool {
        let pending = self
            .pending
            .lock()
            .ok()
            .and_then(|mut values| values.remove(&id));
        let Some(pending) = pending else { return false };
        if decision == ApprovalDecision::AllowSession && pending.request.risk != "high" {
            if let Ok(mut values) = self.session.lock() {
                values.insert(session_key(&pending.request), true);
            }
        }
        pending.response.send(decision).is_ok()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub fn request_approval(
        &self,
        class: PermissionClass,
        action: impl Into<String>,
        cancellation: &CancellationToken,
    ) -> Result<bool, ToolError> {
        self.approve(class, action.into(), cancellation)
    }

    fn approve(
        &self,
        class: PermissionClass,
        action: String,
        cancellation: &CancellationToken,
    ) -> Result<bool, ToolError> {
        let request = ApprovalRequest {
            id: self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            class,
            risk: risk_for(class),
            action: action.chars().take(512).collect(),
        };
        if self
            .session
            .lock()
            .ok()
            .and_then(|values| values.get(&session_key(&request)).copied())
            == Some(true)
        {
            return Ok(true);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .map_err(|_| ToolError::Approval("approval state unavailable".into()))?
            .insert(
                request.id,
                PendingApproval {
                    request: request.clone(),
                    response: sender,
                },
            );
        let event = agent_protocol::Event::PermissionRequested(format!(
            "id={};class={:?};risk={};action={}",
            request.id, request.class, request.risk, request.action
        ));
        if self.events.send(event).is_err() {
            let _ = self
                .pending
                .lock()
                .map(|mut values| values.remove(&request.id));
            return Ok(false);
        }
        let started = std::time::Instant::now();
        while started.elapsed() < self.timeout {
            if cancellation.is_cancelled() {
                let _ = self
                    .pending
                    .lock()
                    .map(|mut values| values.remove(&request.id));
                return Ok(false);
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(ApprovalDecision::AllowOnce | ApprovalDecision::AllowSession) => {
                    return Ok(request.risk != "high")
                }
                Ok(ApprovalDecision::Deny) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(false)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let _ = self
            .pending
            .lock()
            .map(|mut values| values.remove(&request.id));
        Ok(false)
    }
}

fn session_key(request: &ApprovalRequest) -> String {
    format!("{:?}:{}", request.class, request.action)
}

fn risk_for(class: PermissionClass) -> &'static str {
    match class {
        PermissionClass::ReadOnly => "low",
        PermissionClass::WorkspaceWrite => "medium",
        PermissionClass::Process | PermissionClass::Network | PermissionClass::Dangerous => "high",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: ToolStatus,
    pub summary: String,
    pub data: String,
    pub truncated: bool,
    pub spill_ref: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum SnapshotEvent {
    Before {
        path: PathBuf,
        content: Vec<u8>,
        existed: bool,
    },
    After {
        path: PathBuf,
        content: Vec<u8>,
        existed: bool,
    },
}

pub type SnapshotHook = Arc<dyn Fn(SnapshotEvent) -> Result<(), String> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationResult {
    pub status: ToolStatus,
    pub output: String,
    pub truncated: bool,
    pub spill_ref: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error("invalid arguments for {tool}: {reason}")]
    InvalidArguments { tool: String, reason: String },
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("tool I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal failed: {0}")]
    Terminal(#[from] agent_terminal::TerminalError),
    #[error(
        "patch conflict: the expected text was not found in the current file; re-read it and retry"
    )]
    PatchConflict,
    #[error("approval denied or timed out")]
    Approval(String),
}

#[derive(Clone)]
pub struct ToolRegistry {
    permissions: PermissionEngine,
    output_budget: usize,
    timeout: Duration,
    spill_dir: PathBuf,
    cancellation: CancellationToken,
    approval: Option<ApprovalBroker>,
    disabled: BTreeSet<String>,
    snapshot_hook: Option<SnapshotHook>,
    /// Hosts from `permissions.allowed_network_hosts`. Empty disables `web_fetch`.
    web_hosts: Vec<String>,
    /// Kernel restriction applied to every child process (Linux, WorkspaceWrite).
    os_sandbox: Option<std::sync::Arc<agent_sandbox::landlock::ChildSandbox>>,
}

impl agent_core::ToolExecutor for ToolRegistry {
    fn definitions(&self) -> Vec<agent_protocol::ToolDefinition> {
        let mut names: Vec<&str> = CORE_TOOL_NAMES.to_vec();
        if !self.web_hosts.is_empty() {
            names.push("web_fetch");
        }
        names
            .into_iter()
            .filter(|name| !self.disabled.contains(*name))
            .filter_map(|name| self.metadata(name))
            .map(|metadata| agent_protocol::ToolDefinition {
                name: metadata.id,
                description: metadata.short_description.into(),
                input_schema: metadata.input_schema,
            })
            .collect()
    }

    fn execute(
        &mut self,
        call: &agent_protocol::ToolCall,
    ) -> Result<agent_core::ToolExecution, String> {
        let outcome =
            ToolRegistry::execute(self, &call.name, call.arguments.clone(), &self.cancellation)
                .map_err(|error| error.to_string())?;
        Ok(agent_core::ToolExecution {
            status: format!("{:?}", outcome.status),
            output: outcome.data,
        })
    }

    fn execute_typed(
        &mut self,
        call: &agent_protocol::ToolCall,
    ) -> Result<agent_core::ToolExecution, agent_core::ToolFailure> {
        ToolRegistry::execute(self, &call.name, call.arguments.clone(), &self.cancellation)
            .map(|outcome| agent_core::ToolExecution {
                status: format!("{:?}", outcome.status),
                output: outcome.data,
            })
            .map_err(|error| {
                let kind = match &error {
                    ToolError::PatchConflict | ToolError::InvalidArguments { .. } => {
                        agent_core::ToolFailureKind::Recoverable
                    }
                    ToolError::Permission(permission) => match permission {
                        PermissionError::OutsideWorkspace(_) => {
                            agent_core::ToolFailureKind::PathViolation
                        }
                        _ => agent_core::ToolFailureKind::PermissionDenied,
                    },
                    ToolError::UnknownTool(_) => agent_core::ToolFailureKind::InvalidInput,
                    ToolError::Io(io)
                        if matches!(
                            io.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::InvalidData
                        ) =>
                    {
                        agent_core::ToolFailureKind::Recoverable
                    }
                    ToolError::Io(_) | ToolError::Terminal(_) => agent_core::ToolFailureKind::Fatal,
                    ToolError::Approval(_) => agent_core::ToolFailureKind::PermissionDenied,
                };
                agent_core::ToolFailure {
                    kind,
                    message: error.to_string(),
                }
            })
    }
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>, mode: PermissionMode) -> Result<Self, ToolError> {
        let root = root.into();
        let spill_dir = tool_output_cache_dir(&root);
        Ok(Self {
            permissions: PermissionEngine::new(&root, mode)?,
            output_budget: 8 * 1024,
            timeout: Duration::from_secs(30),
            spill_dir: spill_dir.clone(),
            cancellation: CancellationToken::default(),
            approval: None,
            disabled: BTreeSet::new(),
            snapshot_hook: None,
            os_sandbox: build_os_sandbox(&root, mode, &spill_dir),
            web_hosts: Vec::new(),
        })
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_approval_broker(mut self, approval: ApprovalBroker) -> Self {
        self.approval = Some(approval);
        self
    }

    pub fn with_disabled_tools<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.disabled = names.into_iter().map(Into::into).collect();
        self
    }

    /// Enables `web_fetch` for exactly these hosts (lowercase, https only).
    /// Without a non-empty list the tool is neither offered nor executable.
    pub fn with_web_allowed_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.web_hosts = hosts
            .into_iter()
            .map(|host| host.as_ref().trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect();
        self
    }

    pub fn with_snapshot_hook(mut self, hook: SnapshotHook) -> Self {
        self.snapshot_hook = Some(hook);
        self
    }

    pub fn metadata(&self, name: &str) -> Option<ToolMetadata> {
        let command_schema = || {
            json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "Executable name or path; no shell parsing, pipes or globbing"},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "Argument list passed verbatim"},
                    "cwd": {"type": "string", "description": "Workspace-relative working directory (default: workspace root)"},
                    "requires_pty": {"type": "boolean", "description": "Allocate a pseudo-terminal; only for programs that need one"}
                },
                "required": ["program", "args"],
                "additionalProperties": false
            })
        };
        let (description, class, schema, parallel, budget, timeout) = match name {
            "read_file" => (
                "Read a UTF-8 text file, optionally a 1-based inclusive line range. Long output is cut at a line boundary with a hint naming the next start_line.",
                PermissionClass::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Workspace-relative file path"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "First line to return (default 1)"},
                        "end_line": {"type": "integer", "minimum": 1, "description": "Last line to return, inclusive (default end of file)"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                true,
                self.output_budget,
                self.timeout,
            ),
            "search_files" => (
                "Search file contents for a literal string; returns relative-path:line:text. Skips .git, target, node_modules, binary and very large files. Use glob to find files by name.",
                PermissionClass::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Literal text to find (not a regex)"},
                        "path": {"type": "string", "description": "Workspace-relative directory or file to search (default: whole workspace)"},
                        "glob": {"type": "string", "description": "Only search files whose workspace-relative path matches this glob, e.g. **/*.rs"},
                        "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)"},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum matching lines to return (default 200)"}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
                true,
                self.output_budget,
                self.timeout,
            ),
            "glob" => (
                "List workspace files matching a glob (** across directories, * and ? within a name). A pattern without '/' matches file names at any depth. Sorted by path.",
                PermissionClass::ReadOnly,
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob relative to the workspace root, e.g. src/**/*.rs or *.toml"},
                        "path": {"type": "string", "description": "Workspace-relative directory to restrict the listing to (default: whole workspace)"}
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
                true,
                self.output_budget,
                self.timeout,
            ),
            "web_fetch" => (
                "Fetch a text page over https from an allowlisted host and return readable text (scripts and styles removed). Redirects are not followed; request the new URL explicitly if its host is also allowlisted.",
                PermissionClass::Network,
                json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "Absolute https URL with a default port, no credentials"}
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
                false,
                self.output_budget,
                self.timeout,
            ),
            "apply_patch" => (
                "Edit or create a file. Preferred: old_text/new_text replaces one unique snippet (replace_all for every occurrence); an empty old_text creates a new file. Alternative: expected_content/replacement swaps the whole file when expected_content equals its current content. Use exactly one form.",
                PermissionClass::WorkspaceWrite,
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Workspace-relative file path"},
                        "old_text": {"type": "string", "description": "Exact text to replace; must match once unless replace_all is set; empty creates a new file"},
                        "new_text": {"type": "string", "description": "Replacement text (may be empty to delete the snippet)"},
                        "replace_all": {"type": "boolean", "description": "Replace every occurrence of old_text (default false)"},
                        "expected_content": {"type": "string", "description": "Whole-file form: the file's complete current content (empty for a new file)"},
                        "replacement": {"type": "string", "description": "Whole-file form: the complete new content"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                false,
                1024,
                self.timeout,
            ),
            "shell" => (
                "Run one program with an explicit argument list (no shell parsing, pipes or globbing); returns stdout and stderr, truncated if large. Use for builds, tests and git.",
                PermissionClass::WorkspaceWrite,
                command_schema(),
                false,
                self.output_budget,
                self.timeout,
            ),
            "process" => (
                "Run one long-running or cancellable program with an explicit argument list; returns stdout and stderr.",
                PermissionClass::Process,
                command_schema(),
                false,
                self.output_budget,
                self.timeout,
            ),
            "update_plan" => (
                "Record the current task plan. Send the full step list each time; at most one step is in_progress.",
                PermissionClass::WorkspaceWrite,
                json!({
                    "type": "object",
                    "properties": {
                        "explanation": {"type": "string", "description": "Why the plan changed (optional)"},
                        "steps": {
                            "type": "array",
                            "description": "Ordered plan steps",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "step": {"type": "string"},
                                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                                },
                                "required": ["step", "status"]
                            }
                        }
                    },
                    "required": ["steps"]
                }),
                false,
                1024,
                self.timeout,
            ),
            _ => return None,
        };
        Some(ToolMetadata {
            id: name.into(),
            version: "1",
            short_description: description,
            input_schema: schema,
            permission_class: class,
            context_cost_hint: 1,
            supports_parallel: parallel,
            output_budget: budget,
            timeout,
            cancellation: "cooperative; process is killed on cancellation/timeout",
            logging_policy: "log tool id, duration and result class; never raw arguments",
            secret_policy: "do not read or log secrets; child environment is not extended",
        })
    }

    pub fn execute(
        &self,
        name: &str,
        args: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutcome, ToolError> {
        if self.disabled.contains(name) {
            return Err(ToolError::UnknownTool(name.into()));
        }
        let metadata = self
            .metadata(name)
            .ok_or_else(|| ToolError::UnknownTool(name.into()))?;
        // Network is never granted by the engine's class policy; it is granted
        // per host here, before any approval is requested, so an unlisted host
        // is refused without bothering the user.
        let web_target = if metadata.permission_class == PermissionClass::Network {
            Some(self.web_target(&args)?)
        } else {
            None
        };
        let policy_result = match web_target {
            Some(_) => Ok(()),
            None => self.permissions.authorize(metadata.permission_class),
        };
        let interactive_approval = self
            .approval
            .as_ref()
            .filter(|_| metadata.permission_class != PermissionClass::ReadOnly)
            .map(|broker| {
                broker.approve(
                    metadata.permission_class,
                    safe_action(name, &args),
                    cancellation,
                )
            })
            .transpose()?;
        if let Some(approved) = interactive_approval {
            if !approved {
                return Err(ToolError::Approval(format!("{name} requires approval")));
            }
        } else if let Err(permission_error) = policy_result {
            return Err(ToolError::Permission(permission_error));
        }
        match name {
            "read_file" => self.read_file(args),
            "search_files" => self.search_files(args),
            "glob" => self.glob(args),
            "apply_patch" => self.apply_patch(args),
            "shell" | "process" => self.command(name, args, cancellation),
            "web_fetch" => self.web_fetch(args, cancellation),
            "update_plan" => Ok(ToolOutcome {
                status: ToolStatus::Ok,
                summary: "plan accepted".into(),
                data: "{}".into(),
                truncated: false,
                spill_ref: None,
            }),
            _ => unreachable!("metadata checked above"),
        }
    }

    pub fn git_diff(&self) -> Result<ToolOutcome, ToolError> {
        self.git_diff_variant("worktree")
    }

    pub fn git_diff_variant(&self, variant: &str) -> Result<ToolOutcome, ToolError> {
        self.permissions.authorize(PermissionClass::ReadOnly)?;
        let args = match variant {
            "worktree" | "unstaged" => vec!["diff", "--no-ext-diff", "--"],
            "staged" => vec!["diff", "--cached", "--no-ext-diff", "--"],
            "all" => vec!["diff", "HEAD", "--no-ext-diff", "--"],
            "stat" => vec!["diff", "--stat", "--no-ext-diff", "--"],
            "session" => {
                return Err(ToolError::InvalidArguments {
                    tool: "git_diff".into(),
                    reason: "session baseline is unavailable for this workspace".into(),
                })
            }
            other => {
                return Err(ToolError::InvalidArguments {
                    tool: "git_diff".into(),
                    reason: format!("unsupported diff variant: {other}"),
                })
            }
        };
        let result = run_with_hook(
            CommandSpec {
                program: "git".into(),
                args: args.into_iter().map(String::from).collect(),
                cwd: self.permissions.root().to_path_buf(),
                timeout: self.timeout,
                output_limit: self.output_budget,
                spill_dir: self.spill_dir.clone(),
                requires_pty: false,
                env_allowlist: vec![],
            },
            &CancellationToken::default(),
            self.child_hook(),
        )?;
        Ok(ToolOutcome {
            status: match result.status {
                CommandStatus::TimedOut => ToolStatus::Timeout,
                CommandStatus::Cancelled => ToolStatus::Cancelled,
                _ => ToolStatus::Ok,
            },
            summary: "git diff collected".into(),
            data: result.stdout.text,
            truncated: result.stdout.truncated,
            spill_ref: result.stdout.spill_ref,
        })
    }

    pub fn verify_argv(&self, argv: &[String]) -> Result<VerificationResult, ToolError> {
        self.permissions.authorize(PermissionClass::Process)?;
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: "verify".into(),
                reason: "argv must not be empty".into(),
            })?;
        let result = run_with_hook(
            CommandSpec {
                program: program.clone(),
                args: args.to_vec(),
                cwd: self.permissions.root().to_path_buf(),
                timeout: self.timeout,
                output_limit: self.output_budget,
                spill_dir: self.spill_dir.clone(),
                requires_pty: false,
                env_allowlist: vec![],
            },
            &CancellationToken::default(),
            self.child_hook(),
        )?;
        Ok(VerificationResult {
            status: match result.status {
                CommandStatus::Exited(0) => ToolStatus::Ok,
                CommandStatus::TimedOut => ToolStatus::Timeout,
                CommandStatus::Cancelled => ToolStatus::Cancelled,
                _ => ToolStatus::Error,
            },
            output: format!(
                "stdout:\n{}\nstderr:\n{}",
                result.stdout.text, result.stderr.text
            ),
            truncated: result.stdout.truncated || result.stderr.truncated,
            spill_ref: result.stdout.spill_ref.or(result.stderr.spill_ref),
        })
    }

    fn read_file(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            path: String,
            start_line: Option<usize>,
            end_line: Option<usize>,
        }
        let input: Input = parse("read_file", args)?;
        let path = self.permissions.resolve_path(&input.path)?;
        let invalid = |reason: String| ToolError::InvalidArguments {
            tool: "read_file".into(),
            reason,
        };
        let metadata =
            fs::metadata(&path).map_err(|error| io_failure("read_file", &input.path, &error))?;
        if !metadata.is_file() {
            return Err(invalid(format!("{}: not a regular file", input.path)));
        }
        if metadata.len() > MAX_READ_FILE_BYTES {
            return Err(invalid(format!(
                "{}: file is larger than {} MiB; use search_files or read a line range with a shell tool",
                input.path,
                MAX_READ_FILE_BYTES / (1024 * 1024)
            )));
        }
        let start = input.start_line.unwrap_or(1).max(1);
        let end = input.end_line.unwrap_or(usize::MAX);
        if end < start {
            return Err(invalid(
                "end_line must not be smaller than start_line".into(),
            ));
        }
        let file =
            fs::File::open(&path).map_err(|error| io_failure("read_file", &input.path, &error))?;
        let mut full = String::new();
        let mut collected_all = true;
        for (index, line) in std::io::BufRead::lines(std::io::BufReader::new(file)).enumerate() {
            let number = index + 1;
            let line = line.map_err(|error| io_failure("read_file", &input.path, &error))?;
            if number < start {
                continue;
            }
            if number > end {
                break;
            }
            if full.len() + line.len() + 1 > MAX_READ_COLLECT_BYTES {
                collected_all = false;
                break;
            }
            if number > start {
                full.push('\n');
            }
            full.push_str(&line);
        }
        if collected_all && full.len() <= self.output_budget {
            return Ok(bounded(
                ToolStatus::Ok,
                "file read",
                full,
                self.output_budget,
            ));
        }
        let window = &full[..floor_char_boundary(&full, self.output_budget)];
        let (shown, hint) = match window.rfind('\n') {
            Some(cut) => {
                let lines = window[..cut].matches('\n').count() + 1;
                (
                    window[..cut].to_owned(),
                    format!("\n[truncated after line {}; use start_line={} to continue]", start + lines - 1, start + lines),
                )
            }
            None => (
                window.to_owned(),
                format!("\n[line {start} exceeds the output limit and was cut; inspect it with a narrower tool]"),
            ),
        };
        Ok(ToolOutcome {
            status: ToolStatus::Ok,
            summary: "file read".into(),
            data: format!("{shown}{hint}"),
            truncated: true,
            spill_ref: bounded_spill(&full),
        })
    }

    fn search_files(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            query: String,
            path: Option<String>,
            glob: Option<String>,
            ignore_case: Option<bool>,
            max_results: Option<usize>,
        }
        let input: Input = parse("search_files", args)?;
        if input.query.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "search_files".into(),
                reason: "query must not be empty".into(),
            });
        }
        if let Some(pattern) = &input.glob {
            validate_glob("search_files", pattern)?;
        }
        let max_results = input
            .max_results
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, 1000);
        let ignore_case = input.ignore_case.unwrap_or(false);
        let needle = if ignore_case {
            input.query.to_lowercase()
        } else {
            input.query.clone()
        };
        let scope = self.scope("search_files", input.path.as_deref())?;
        let root = self.permissions.root();
        let (files, scan_cut) = walk_files(&scope);
        let mut out = String::new();
        let mut results = 0_usize;
        let mut limit_hit = false;
        'files: for file in files {
            let relative = relative_display(root, &file);
            if let Some(pattern) = &input.glob {
                if !glob_match(pattern, &relative) {
                    continue;
                }
            }
            let Some(content) = read_searchable(&file) else {
                continue;
            };
            for (index, text) in content.lines().enumerate() {
                let hit = if ignore_case {
                    text.to_lowercase().contains(&needle)
                } else {
                    text.contains(&needle)
                };
                if !hit {
                    continue;
                }
                let line = format!(
                    "{relative}:{}:{}\n",
                    index + 1,
                    cap_chars(text, MAX_SEARCH_LINE_CHARS)
                );
                if results >= max_results || out.len() + line.len() > self.output_budget {
                    limit_hit = true;
                    break 'files;
                }
                out.push_str(&line);
                results += 1;
            }
        }
        if limit_hit {
            out.push_str(&format!(
                "[stopped at the limit of {results} results; narrow the query, path or glob]\n"
            ));
        }
        if scan_cut {
            out.push_str(&format!(
                "[file scan stopped after {WALK_MAX_FILES} files; narrow the path]\n"
            ));
        }
        Ok(ToolOutcome {
            status: ToolStatus::Ok,
            summary: "search complete".into(),
            data: out,
            truncated: limit_hit || scan_cut,
            spill_ref: None,
        })
    }

    fn glob(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            pattern: String,
            path: Option<String>,
        }
        let input: Input = parse("glob", args)?;
        validate_glob("glob", &input.pattern)?;
        let scope = self.scope("glob", input.path.as_deref())?;
        let root = self.permissions.root();
        let (files, scan_cut) = walk_files(&scope);
        let mut out = Vec::new();
        let mut used = 0_usize;
        let mut limit_hit = false;
        for file in files {
            let relative = relative_display(root, &file);
            if !glob_match(&input.pattern, &relative) {
                continue;
            }
            if out.len() >= MAX_GLOB_RESULTS || used + relative.len() + 1 > self.output_budget {
                limit_hit = true;
                break;
            }
            used += relative.len() + 1;
            out.push(relative);
        }
        let mut data = out.join("\n");
        if out.is_empty() {
            data = "no files matched".into();
        }
        if limit_hit {
            data.push_str(&format!(
                "\n[stopped at {} files; use a narrower pattern or path]",
                out.len()
            ));
        }
        if scan_cut {
            data.push_str(&format!(
                "\n[file scan stopped after {WALK_MAX_FILES} files; narrow the path]"
            ));
        }
        Ok(ToolOutcome {
            status: ToolStatus::Ok,
            summary: "glob complete".into(),
            data,
            truncated: limit_hit || scan_cut,
            spill_ref: None,
        })
    }

    /// Resolves an optional workspace-relative search scope to an existing path.
    fn scope(&self, tool: &str, path: Option<&str>) -> Result<PathBuf, ToolError> {
        let Some(path) = path else {
            return Ok(self.permissions.root().to_path_buf());
        };
        let resolved = self.permissions.resolve_path(path)?;
        if !resolved.exists() {
            return Err(ToolError::InvalidArguments {
                tool: tool.into(),
                reason: format!("{path}: no such file or directory"),
            });
        }
        Ok(resolved)
    }

    fn apply_patch(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            path: String,
            expected_content: Option<String>,
            replacement: Option<String>,
            old_text: Option<String>,
            new_text: Option<String>,
            replace_all: Option<bool>,
        }
        let input: Input = parse("apply_patch", args)?;
        let invalid = |reason: &str| ToolError::InvalidArguments {
            tool: "apply_patch".into(),
            reason: reason.into(),
        };
        let snippet_form = input.old_text.is_some() || input.new_text.is_some();
        let whole_form = input.expected_content.is_some() || input.replacement.is_some();
        if snippet_form == whole_form {
            return Err(invalid(
                "provide either old_text and new_text, or expected_content and replacement",
            ));
        }
        let path = self.permissions.resolve_path(&input.path)?;
        let current = fs::read(&path).unwrap_or_default();
        let existed = path.exists();
        if !existed
            && input
                .expected_content
                .as_deref()
                .is_some_and(|text| !text.is_empty())
        {
            return Err(invalid("missing file requires empty expected_content"));
        }
        if let Some(hook) = &self.snapshot_hook {
            hook(SnapshotEvent::Before {
                path: path.clone(),
                content: current.clone(),
                existed,
            })
            .map_err(ToolError::Approval)?;
        }
        let current_text = String::from_utf8(current).map_err(|_| {
            invalid("apply_patch requires UTF-8; use a binary-safe tool for binary files")
        })?;
        let (updated, note) = if snippet_form {
            let old = input
                .old_text
                .ok_or_else(|| invalid("old_text is required with new_text"))?;
            let new = input
                .new_text
                .ok_or_else(|| invalid("new_text is required with old_text"))?;
            if old.is_empty() {
                if existed {
                    return Err(invalid(
                        "old_text is empty but the file exists; provide the text to replace",
                    ));
                }
                (new, "file created".to_owned())
            } else {
                if !existed {
                    return Err(invalid(
                        "file does not exist; use an empty old_text to create it",
                    ));
                }
                let matches = current_text.matches(old.as_str()).count();
                if matches == 0 {
                    return Err(ToolError::PatchConflict);
                }
                let replace_all = input.replace_all.unwrap_or(false);
                if matches > 1 && !replace_all {
                    return Err(ToolError::InvalidArguments {
                        tool: "apply_patch".into(),
                        reason: format!(
                            "old_text matches {matches} locations; include more surrounding context or set replace_all"
                        ),
                    });
                }
                let updated = if replace_all {
                    current_text.replace(old.as_str(), &new)
                } else {
                    current_text.replacen(old.as_str(), &new, 1)
                };
                (
                    updated,
                    format!(
                        "replaced {} occurrence(s)",
                        if replace_all { matches } else { 1 }
                    ),
                )
            }
        } else {
            let expected = input
                .expected_content
                .ok_or_else(|| invalid("expected_content is required with replacement"))?;
            let replacement = input
                .replacement
                .ok_or_else(|| invalid("replacement is required with expected_content"))?;
            if current_text != expected {
                return Err(ToolError::PatchConflict);
            }
            (replacement, String::new())
        };
        if !existed {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| io_failure("apply_patch", &input.path, &error))?;
            }
        }
        fs::write(&path, updated.as_bytes())
            .map_err(|error| io_failure("apply_patch", &input.path, &error))?;
        if let Some(hook) = &self.snapshot_hook {
            let _ = hook(SnapshotEvent::After {
                path,
                content: updated.into_bytes(),
                existed: true,
            });
        }
        Ok(bounded(
            ToolStatus::Ok,
            "patch applied",
            note,
            self.output_budget,
        ))
    }

    /// Whether child processes run under the kernel sandbox.
    pub fn os_sandbox_active(&self) -> bool {
        self.os_sandbox.is_some()
    }

    fn child_hook(&self) -> Option<agent_terminal::ChildHook> {
        let sandbox = self.os_sandbox.clone()?;
        Some(std::sync::Arc::new(move || sandbox.restrict_current()))
    }

    /// Validates a `web_fetch` request against the allowlist and the mode.
    fn web_target(&self, args: &Value) -> Result<web::WebTarget, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            url: String,
        }
        let input: Input = parse("web_fetch", args.clone())?;
        if self.permissions.mode() == PermissionMode::ReadOnly {
            return Err(ToolError::Permission(PermissionError::Denied {
                class: PermissionClass::Network,
            }));
        }
        let target =
            web::parse_https_target(&input.url).map_err(|reason| ToolError::InvalidArguments {
                tool: "web_fetch".into(),
                reason,
            })?;
        if !self.web_hosts.contains(&target.host) {
            return Err(ToolError::Permission(PermissionError::Denied {
                class: PermissionClass::Network,
            }));
        }
        Ok(target)
    }

    fn web_fetch(
        &self,
        args: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutcome, ToolError> {
        let target = self.web_target(&args)?;
        if cancellation.is_cancelled() {
            return Ok(ToolOutcome {
                status: ToolStatus::Cancelled,
                summary: "web_fetch cancelled".into(),
                data: String::new(),
                truncated: false,
                spill_ref: None,
            });
        }
        let invalid = |reason: String| ToolError::InvalidArguments {
            tool: "web_fetch".into(),
            reason,
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("helios-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| invalid(format!("HTTP client unavailable: {error}")))?;
        // Errors name only the host: the path and query may carry private data.
        let response = client.get(&target.url).send().map_err(|error| {
            let kind = if error.is_timeout() {
                "timed out"
            } else {
                "failed"
            };
            invalid(format!("request to {} {kind}", target.host))
        })?;
        let status = response.status();
        if status.is_redirection() {
            return Err(invalid(format!(
                "{} answered with HTTP {status}; redirects are not followed",
                target.host
            )));
        }
        if !status.is_success() {
            return Err(invalid(format!(
                "{} answered with HTTP {status}",
                target.host
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if !web::is_textual(&content_type) {
            return Err(invalid(format!(
                "{} returned a non-text content type; web_fetch only reads text",
                target.host
            )));
        }
        let (bytes, body_truncated) = web::read_bounded(response)
            .map_err(|error| invalid(format!("reading {} failed: {error}", target.host)))?;
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let text = if content_type.to_ascii_lowercase().contains("html") {
            web::html_to_text(&raw)
        } else {
            raw
        };
        let data = format!("source: {}\n{text}", target.host);
        let mut outcome = bounded(ToolStatus::Ok, "web fetched", data, self.output_budget);
        outcome.truncated |= body_truncated;
        Ok(outcome)
    }

    fn command(
        &self,
        name: &str,
        args: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            program: String,
            args: Vec<String>,
            cwd: Option<String>,
            requires_pty: Option<bool>,
        }
        let input: Input = parse(name, args)?;
        let cwd = match input.cwd {
            Some(path) => self.permissions.resolve_path(path)?,
            None => self.permissions.root().to_path_buf(),
        };
        let program_name = input.program.clone();
        let input_args = input.args.clone();
        let result = run_with_hook(
            CommandSpec {
                program: input.program,
                args: input.args,
                cwd,
                timeout: self.timeout,
                output_limit: self.output_budget,
                spill_dir: self.spill_dir.clone(),
                requires_pty: input.requires_pty.unwrap_or(false),
                env_allowlist: vec![],
            },
            cancellation,
            self.child_hook(),
        )
        .map_err(|error| match error {
            agent_terminal::TerminalError::Start(io) => ToolError::InvalidArguments {
                tool: name.into(),
                reason: format!("cannot start `{program_name}`: {io}"),
            },
            other => ToolError::Terminal(other),
        })?;
        let status = match result.status {
            CommandStatus::Exited(0) => ToolStatus::Ok,
            CommandStatus::Exited(_) => ToolStatus::Error,
            CommandStatus::TimedOut => ToolStatus::Timeout,
            CommandStatus::Cancelled => ToolStatus::Cancelled,
        };
        let spill_ref = result
            .stdout
            .spill_ref
            .clone()
            .or(result.stderr.spill_ref.clone());
        let truncated = result.stdout.truncated || result.stderr.truncated;
        // Build/test output that overflows the budget is condensed to its
        // failures and summary, read from the full spill when one exists.
        if truncated {
            if let Some(family) = filter::family_of(&program_name, &input_args) {
                let full = match &spill_ref {
                    Some(path) => read_prefix(path, filter::MAX_SCAN_BYTES),
                    None => result.stdout.text.clone(),
                };
                let condensed = filter::condense(family, &full);
                let text = format!(
                    "stdout (condensed: {} of {} lines kept; full output in spill_ref):\n{}\nstderr:\n{}",
                    condensed.kept_lines,
                    condensed.total_lines,
                    condensed.text,
                    result.stderr.text
                );
                return Ok(bounded_with_spill(
                    status,
                    &format!("{name} completed (condensed)"),
                    text,
                    self.output_budget,
                    spill_ref,
                ));
            }
        }
        let text = format!(
            "stdout:\n{}\nstderr:\n{}",
            result.stdout.text, result.stderr.text
        );
        Ok(ToolOutcome {
            status,
            summary: format!("{name} completed"),
            data: text,
            truncated,
            spill_ref,
        })
    }
}

/// Reads at most `limit` bytes of `path`, cut on a char boundary. Failures
/// yield an empty string: condensing is best-effort and the spill still exists.
fn read_prefix(path: &Path, limit: usize) -> String {
    use std::io::Read;
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::with_capacity(limit.min(1 << 20));
    let _ = file.take(limit as u64).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Like [`bounded`], but keeps an existing raw-output spill instead of writing
/// a second copy of the same data.
fn bounded_with_spill(
    status: ToolStatus,
    summary: &str,
    data: String,
    limit: usize,
    spill_ref: Option<PathBuf>,
) -> ToolOutcome {
    if data.len() <= limit {
        return ToolOutcome {
            status,
            summary: summary.into(),
            data,
            truncated: false,
            spill_ref,
        };
    }
    ToolOutcome {
        status,
        summary: summary.into(),
        data: data[..floor_char_boundary(&data, limit)].to_owned(),
        truncated: true,
        spill_ref,
    }
}

/// Builds the child-process sandbox for WorkspaceWrite on Linux. Writable:
/// the workspace, temp dir, tool spill dir and cargo's download caches (never
/// `~/.cargo` as a whole, which holds credentials). Readable: system roots,
/// PATH entries, the rust toolchain and git configuration. Everything else in
/// the home directory is denied. `HELIOS_OS_SANDBOX=off` disables it.
fn build_os_sandbox(
    root: &Path,
    mode: PermissionMode,
    spill_dir: &Path,
) -> Option<std::sync::Arc<agent_sandbox::landlock::ChildSandbox>> {
    if mode != PermissionMode::WorkspaceWrite
        || std::env::var("HELIOS_OS_SANDBOX").is_ok_and(|value| value == "off")
    {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let _ = fs::create_dir_all(spill_dir);
        let mut writable = vec![
            root.to_path_buf(),
            std::env::temp_dir(),
            spill_dir.to_path_buf(),
        ];
        let mut readable = agent_sandbox::landlock::system_read_roots();
        if let Some(path) = std::env::var_os("PATH") {
            readable.extend(std::env::split_paths(&path));
        }
        if let Some(home) = &home {
            writable.push(home.join(".cargo/registry"));
            writable.push(home.join(".cargo/git"));
            readable.push(home.join(".cargo/bin"));
            readable.push(home.join(".cargo/config.toml"));
            readable.push(home.join(".rustup"));
            readable.push(home.join(".gitconfig"));
            readable.push(home.join(".config/git"));
        }
        match agent_sandbox::landlock::ChildSandbox::new(&writable, &readable) {
            Ok(Some(sandbox)) => Some(std::sync::Arc::new(sandbox)),
            // Unsupported kernels run unrestricted; `os_sandbox_active` reports it.
            Ok(None) | Err(_) => None,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, spill_dir);
        None
    }
}

fn tool_output_cache_dir(root: &Path) -> PathBuf {
    let _ = root;
    resolve_tool_output_dir(
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::temp_dir(),
    )
}

fn resolve_tool_output_dir(
    xdg_cache: Option<PathBuf>,
    home: Option<PathBuf>,
    temp: PathBuf,
) -> PathBuf {
    xdg_cache
        .or_else(|| home.map(|path| path.join(".cache")))
        .unwrap_or(temp)
        .join("helios/tool-output")
}

fn safe_action(name: &str, args: &Value) -> String {
    let mut value = args.clone();
    redact_secret_values(&mut value);
    format!("{name} {value}")
}

fn redact_secret_values(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if [
                    "token",
                    "password",
                    "secret",
                    "api_key",
                    "apikey",
                    "authorization",
                ]
                .iter()
                .any(|marker| key.to_ascii_lowercase().contains(marker))
                {
                    *item = Value::String("[REDACTED]".into());
                } else {
                    redact_secret_values(item);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_secret_values),
        _ => {}
    }
}

fn parse<T: for<'a> Deserialize<'a>>(tool: &str, args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|error| ToolError::InvalidArguments {
        tool: tool.into(),
        reason: error.to_string(),
    })
}

fn bounded(status: ToolStatus, summary: &str, data: String, limit: usize) -> ToolOutcome {
    if data.len() <= limit {
        return ToolOutcome {
            status,
            summary: summary.into(),
            data,
            truncated: false,
            spill_ref: None,
        };
    }
    let spill_ref = bounded_spill(&data);
    ToolOutcome {
        status,
        summary: summary.into(),
        data: data[..floor_char_boundary(&data, limit)].to_owned(),
        truncated: true,
        spill_ref,
    }
}

/// Largest index `<= limit` that is a char boundary of `text`.
fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Largest regular file `read_file` will open.
const MAX_READ_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Most text `read_file` collects for one request before it stops reading.
const MAX_READ_COLLECT_BYTES: usize = 1024 * 1024;
const DEFAULT_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_LINE_CHARS: usize = 300;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GLOB_RESULTS: usize = 500;
const WALK_MAX_FILES: usize = 20_000;
const WALK_MAX_DEPTH: usize = 32;
const WALK_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
];

/// Maps an I/O failure to a recoverable, path-bearing tool error so a typo in
/// a model-supplied path is reported back instead of ending the turn.
fn io_failure(tool: &str, path: &str, error: &std::io::Error) -> ToolError {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound | PermissionDenied | InvalidData | IsADirectory | NotADirectory
        | AlreadyExists | InvalidInput => ToolError::InvalidArguments {
            tool: tool.into(),
            reason: format!("{path}: {error}"),
        },
        _ => ToolError::Io(std::io::Error::new(
            error.kind(),
            format!("{path}: {error}"),
        )),
    }
}

fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut capped = text.chars().take(max).collect::<String>();
    capped.push('…');
    capped
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Reads a small UTF-8 text file; binary, oversized and unreadable files are skipped.
fn read_searchable(path: &Path) -> Option<String> {
    if fs::metadata(path).ok()?.len() > MAX_SEARCH_FILE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Deterministic, bounded file listing below `scope`. Symlinks are never
/// followed (no loops, no escapes) and vendor/VCS directories are skipped.
/// Returns the files and whether the scan stopped early.
fn walk_files(scope: &Path) -> (Vec<PathBuf>, bool) {
    fn visit(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name();
                if depth >= WALK_MAX_DEPTH
                    || WALK_SKIP_DIRS
                        .iter()
                        .any(|skip| name == std::ffi::OsStr::new(skip))
                {
                    continue;
                }
                if visit(&path, depth + 1, out) {
                    return true;
                }
            } else if kind.is_file() {
                if out.len() >= WALK_MAX_FILES {
                    return true;
                }
                out.push(path);
            }
        }
        false
    }
    let mut files = Vec::new();
    if fs::symlink_metadata(scope).is_ok_and(|meta| meta.is_file()) {
        files.push(scope.to_path_buf());
        return (files, false);
    }
    let cut = visit(scope, 0, &mut files);
    (files, cut)
}

fn validate_glob(tool: &str, pattern: &str) -> Result<(), ToolError> {
    let invalid = |reason: &str| ToolError::InvalidArguments {
        tool: tool.into(),
        reason: reason.into(),
    };
    if pattern.is_empty() || pattern.len() > 200 {
        return Err(invalid("glob must be 1-200 characters"));
    }
    if pattern.starts_with('/') || pattern.split('/').any(|part| part == "..") {
        return Err(invalid(
            "glob must be workspace-relative and must not contain ..",
        ));
    }
    if pattern.split('/').filter(|part| *part == "**").count() > 4 {
        return Err(invalid("glob may contain at most four ** segments"));
    }
    Ok(())
}

/// Matches a workspace-relative path against a glob. `**` spans directories,
/// `*`/`?` stay inside one name, and a pattern without `/` matches at any depth.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn segments(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| segments(rest, &path[skip..])),
            Some((head, rest)) => path
                .split_first()
                .is_some_and(|(name, tail)| name_match(head, name) && segments(rest, tail)),
        }
    }
    fn name_match(pattern: &str, name: &str) -> bool {
        let pattern = pattern.chars().collect::<Vec<_>>();
        let name = name.chars().collect::<Vec<_>>();
        let (mut p, mut n) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while n < name.len() {
            if p < pattern.len()
                && (pattern[p] == '?' || pattern[p] == name[n])
                && pattern[p] != '*'
            {
                p += 1;
                n += 1;
            } else if p < pattern.len() && pattern[p] == '*' {
                star = Some((p, n));
                p += 1;
            } else if let Some((star_p, star_n)) = star {
                p = star_p + 1;
                n = star_n + 1;
                star = Some((star_p, star_n + 1));
            } else {
                return false;
            }
        }
        pattern[p..].iter().all(|ch| *ch == '*')
    }
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let expanded;
    let pattern = if pattern.contains('/') {
        pattern
    } else {
        expanded = format!("**/{pattern}");
        expanded.as_str()
    };
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let path = path.split('/').collect::<Vec<_>>();
    segments(&pattern, &path)
}

fn bounded_spill(data: &str) -> Option<PathBuf> {
    let base = resolve_tool_output_dir(
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::temp_dir(),
    );
    fs::create_dir_all(&base).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
    }
    let sequence = OUTPUT_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = base.join(format!("spill-{}-{sequence}", std::process::id()));
    fs::write(&path, data).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("root");
        path
    }

    #[test]
    fn patch_conflict_is_rejected() {
        let root = root("patch");
        let file = root.join("file.txt");
        fs::write(&file, "actual").expect("write");
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        let error = registry
            .execute(
                "apply_patch",
                json!({"path":"file.txt","expected_content":"stale","replacement":"new"}),
                &CancellationToken::default(),
            )
            .expect_err("conflict");
        assert!(matches!(error, ToolError::PatchConflict));
    }

    fn run_tool(
        registry: &ToolRegistry,
        name: &str,
        args: Value,
    ) -> Result<ToolOutcome, ToolError> {
        registry.execute(name, args, &CancellationToken::default())
    }

    #[cfg(unix)]
    #[test]
    fn failing_test_output_is_condensed_and_full_output_stays_spilled() {
        use std::os::unix::fs::PermissionsExt;
        let root = root("condense");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let script = bin.join("pytest");
        fs::write(
            &script,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 4000 ]; do echo \"ok line $i\"; i=$((i+1)); done\n\
echo 'E   AssertionError: expected 2'\necho '=== 1 failed in 0.1s ==='\nexit 1\n",
        )
        .expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        let outcome = run_tool(
            &registry,
            "shell",
            json!({"program": script.display().to_string(), "args": []}),
        )
        .expect("run");
        assert!(outcome.summary.contains("condensed"), "{}", outcome.summary);
        assert!(outcome.data.contains("E   AssertionError: expected 2"));
        assert!(outcome.data.contains("1 failed in 0.1s"));
        assert!(!outcome.data.contains("ok line 1000"));
        assert!(
            outcome.data.len() <= 8 * 1024 + 64,
            "{}",
            outcome.data.len()
        );
        let spill = outcome.spill_ref.expect("full output spill");
        let full = fs::read_to_string(spill).expect("spill");
        assert!(full.contains("ok line 3999"));
    }

    #[test]
    fn web_fetch_is_offered_and_allowed_only_for_listed_hosts() {
        use agent_core::ToolExecutor;
        let root = root("web-policy");
        let plain = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        assert!(!plain
            .definitions()
            .iter()
            .any(|tool| tool.name == "web_fetch"));
        let denied = run_tool(&plain, "web_fetch", json!({"url": "https://example.com/"}))
            .expect_err("no allowlist");
        assert!(matches!(
            denied,
            ToolError::Permission(PermissionError::Denied {
                class: PermissionClass::Network
            })
        ));

        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
            .expect("registry")
            .with_web_allowed_hosts(["Docs.RS"]);
        assert!(registry
            .definitions()
            .iter()
            .any(|tool| tool.name == "web_fetch"));
        let unlisted = run_tool(
            &registry,
            "web_fetch",
            json!({"url": "https://example.com/"}),
        )
        .expect_err("unlisted host");
        let mut failure_registry = registry.clone();
        let failure = failure_registry
            .execute_typed(&agent_protocol::ToolCall {
                id: None,
                name: "web_fetch".into(),
                arguments: json!({"url": "https://example.com/"}),
            })
            .expect_err("unlisted host");
        assert_eq!(failure.kind, agent_core::ToolFailureKind::PermissionDenied);
        assert!(matches!(unlisted, ToolError::Permission(_)));

        let read_only = ToolRegistry::new(&root, PermissionMode::ReadOnly)
            .expect("registry")
            .with_web_allowed_hosts(["docs.rs"]);
        assert!(matches!(
            run_tool(&read_only, "web_fetch", json!({"url": "https://docs.rs/"})),
            Err(ToolError::Permission(_))
        ));

        let malformed = run_tool(&registry, "web_fetch", json!({"url": "http://docs.rs/"}))
            .expect_err("plain http");
        assert!(matches!(malformed, ToolError::InvalidArguments { .. }));
    }

    /// Makes a real HTTPS request; run explicitly with `--ignored` when the
    /// environment has outbound access. Kept out of the default suite so the
    /// offline benchmark stays deterministic.
    #[test]
    #[ignore = "requires outbound HTTPS"]
    fn web_fetch_reads_an_allowlisted_page_as_text() {
        let root = root("web-live");
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
            .expect("registry")
            .with_web_allowed_hosts(["example.com"]);
        let outcome = run_tool(
            &registry,
            "web_fetch",
            json!({"url": "https://example.com/"}),
        )
        .expect("fetch");
        assert!(
            outcome.data.starts_with("source: example.com\n"),
            "{}",
            outcome.data
        );
        assert!(outcome.data.contains("Example Domain"), "{}", outcome.data);
        assert!(!outcome.data.contains("<html"));
    }

    /// Runs `sh -c` under the registry's sandbox and returns the exit code.
    #[cfg(target_os = "linux")]
    fn sandboxed_sh(registry: &ToolRegistry, script: &str) -> Result<ToolOutcome, ToolError> {
        run_tool(
            registry,
            "shell",
            json!({"program": "/bin/sh", "args": ["-c", script]}),
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn os_sandbox_confines_child_processes_to_the_workspace_and_no_network() {
        let root = root("os-sandbox");
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        if !registry.os_sandbox_active() {
            eprintln!("skipped: kernel does not provide Landlock");
            return;
        }
        // A PTY cannot take the hook, so it must be refused rather than run free.
        let pty = run_tool(
            &registry,
            "process",
            json!({"program": "/bin/sh", "args": ["-c", "true"], "requires_pty": true}),
        );
        assert!(pty.is_err(), "PTY command ran outside the sandbox");

        // Inside the workspace: allowed.
        let inside =
            sandboxed_sh(&registry, "echo ok > inside.txt && cat inside.txt").expect("run");
        assert_eq!(inside.status, ToolStatus::Ok, "{}", inside.data);
        assert!(inside.data.contains("ok"));

        // Outside every allowed root: writes and reads are refused.
        let outside = std::path::PathBuf::from("/var/tmp")
            .join(format!("helios-os-sandbox-probe-{}", std::process::id()));
        let _ = fs::remove_file(&outside);
        let write =
            sandboxed_sh(&registry, &format!("echo leak > {}", outside.display())).expect("run");
        assert_eq!(write.status, ToolStatus::Error, "{}", write.data);
        assert!(!outside.exists(), "sandbox let a write escape");

        let secret_dir = std::path::PathBuf::from("/var/tmp")
            .join(format!("helios-fake-home-{}", std::process::id()));
        fs::create_dir_all(secret_dir.join(".ssh")).expect("fake credentials");
        fs::write(secret_dir.join(".ssh/id_test"), "PRIVATE").expect("secret");
        let read = sandboxed_sh(
            &registry,
            &format!("cat {}", secret_dir.join(".ssh/id_test").display()),
        )
        .expect("run");
        let _ = fs::remove_dir_all(&secret_dir);
        assert_eq!(read.status, ToolStatus::Error, "{}", read.data);
        assert!(!read.data.contains("PRIVATE"));
        let _ = fs::remove_file(&outside);

        // TCP to a local listener must be refused by the sandbox, while the
        // same probe succeeds without it (control), so the result is meaningful.
        if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") {
            let port = listener.local_addr().expect("addr").port();
            let probe = format!(
                "import socket\ntry:\n    socket.create_connection(('127.0.0.1', {port}), timeout=2)\n    print('connected')\nexcept OSError:\n    print('denied')\n"
            );
            let script = root.join("probe.py");
            fs::write(&script, probe).expect("probe");
            let sandboxed =
                sandboxed_sh(&registry, &format!("python3 {}", script.display())).expect("run");
            assert!(
                sandboxed.data.contains("denied") && !sandboxed.data.contains("connected"),
                "tcp connect was not denied: {}",
                sandboxed.data
            );
            let control = agent_terminal::run(
                agent_terminal::CommandSpec {
                    program: "/usr/bin/env".into(),
                    args: vec!["python3".into(), script.display().to_string()],
                    cwd: root.clone(),
                    timeout: Duration::from_secs(10),
                    output_limit: 4096,
                    spill_dir: root.join("spill"),
                    requires_pty: false,
                    env_allowlist: vec!["PATH".into()],
                },
                &CancellationToken::default(),
            )
            .expect("control run");
            assert!(
                control.stdout.text.contains("connected"),
                "control probe must connect without the sandbox: {}",
                control.stdout.text
            );
        }
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let text = "ş".repeat(10);
        let outcome = bounded(ToolStatus::Ok, "ok", text, 5);
        assert!(outcome.truncated);
        assert_eq!(outcome.data, "ş".repeat(2));
    }

    #[test]
    fn missing_file_is_a_recoverable_tool_failure() {
        use agent_core::ToolExecutor;
        let root = root("missing");
        let mut registry =
            ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        let failure = registry
            .execute_typed(&agent_protocol::ToolCall {
                id: None,
                name: "read_file".into(),
                arguments: json!({"path": "nope.rs"}),
            })
            .expect_err("missing");
        assert_eq!(failure.kind, agent_core::ToolFailureKind::Recoverable);
        assert!(failure.message.contains("nope.rs"), "{}", failure.message);
    }

    #[test]
    fn read_file_reports_where_to_continue_when_truncated() {
        let root = root("read-range");
        let body = (1..=2_000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.join("big.txt"), body).expect("write");
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let outcome = run_tool(&registry, "read_file", json!({"path": "big.txt"})).expect("read");
        assert!(outcome.truncated);
        assert!(outcome.data.contains("line 1\n"));
        assert!(outcome.data.contains("use start_line="), "{}", outcome.data);
        let tail = run_tool(
            &registry,
            "read_file",
            json!({"path": "big.txt", "start_line": 1_999, "end_line": 2_000}),
        )
        .expect("range");
        assert_eq!(tail.data, "line 1999\nline 2000");
        assert!(!tail.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn search_ignores_symlink_loops_and_vendor_directories() {
        let root = root("search-loop");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join("target/debug")).expect("target");
        fs::create_dir_all(root.join("node_modules/pkg")).expect("node_modules");
        fs::write(root.join("src/lib.rs"), "fn needle() {}\n").expect("lib");
        fs::write(root.join("target/debug/out.txt"), "needle\n").expect("target file");
        fs::write(root.join("node_modules/pkg/index.js"), "needle\n").expect("vendor file");
        std::os::unix::fs::symlink(&root, root.join("src/loop")).expect("loop");
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let outcome =
            run_tool(&registry, "search_files", json!({"query": "needle"})).expect("search");
        assert_eq!(outcome.data.trim(), "src/lib.rs:1:fn needle() {}");
    }

    #[test]
    fn search_is_bounded_and_supports_glob_and_ignore_case() {
        let root = root("search-bounds");
        fs::create_dir_all(root.join("docs")).expect("docs");
        let many = "Match\n".repeat(500);
        fs::write(root.join("a.txt"), &many).expect("a");
        fs::write(root.join("docs/b.md"), "match here\n").expect("b");
        fs::write(root.join("bin.dat"), b"match\0\x01\x02").expect("binary");
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let limited = run_tool(
            &registry,
            "search_files",
            json!({"query": "Match", "max_results": 3}),
        )
        .expect("search");
        assert_eq!(limited.data.matches("a.txt:").count(), 3);
        assert!(limited.data.contains("limit of 3 results"));
        let filtered = run_tool(
            &registry,
            "search_files",
            json!({"query": "MATCH", "ignore_case": true, "glob": "**/*.md"}),
        )
        .expect("search");
        assert_eq!(filtered.data.trim(), "docs/b.md:1:match here");
    }

    #[test]
    fn glob_matches_nested_patterns_deterministically() {
        let root = root("glob");
        fs::create_dir_all(root.join("src/deep")).expect("dirs");
        for file in ["src/a.rs", "src/deep/b.rs", "src/deep/c.txt", "top.rs"] {
            fs::write(root.join(file), "").expect("file");
        }
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let rust = run_tool(&registry, "glob", json!({"pattern": "**/*.rs"})).expect("glob");
        assert_eq!(rust.data, "src/a.rs\nsrc/deep/b.rs\ntop.rs");
        let scoped = run_tool(&registry, "glob", json!({"pattern": "src/*.rs"})).expect("glob");
        assert_eq!(scoped.data, "src/a.rs");
        let single =
            run_tool(&registry, "glob", json!({"pattern": "src/deep/?.txt"})).expect("glob");
        assert_eq!(single.data, "src/deep/c.txt");
        assert!(run_tool(&registry, "glob", json!({"pattern": ""})).is_err());
    }

    #[test]
    fn patch_replaces_unique_snippet_without_resending_the_file() {
        let root = root("snippet");
        let file = root.join("main.rs");
        fs::write(&file, "fn a() {}\nfn b() {}\nfn c() {}\n").expect("write");
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        run_tool(
            &registry,
            "apply_patch",
            json!({"path": "main.rs", "old_text": "fn b() {}", "new_text": "fn b() { 1 }"}),
        )
        .expect("snippet patch");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "fn a() {}\nfn b() { 1 }\nfn c() {}\n"
        );
        let missing = run_tool(
            &registry,
            "apply_patch",
            json!({"path": "main.rs", "old_text": "fn zzz", "new_text": "x"}),
        )
        .expect_err("missing");
        assert!(matches!(missing, ToolError::PatchConflict));
        fs::write(&file, "x\nx\n").expect("rewrite");
        let ambiguous = run_tool(
            &registry,
            "apply_patch",
            json!({"path": "main.rs", "old_text": "x", "new_text": "y"}),
        )
        .expect_err("ambiguous");
        assert!(matches!(ambiguous, ToolError::InvalidArguments { .. }));
        run_tool(
            &registry,
            "apply_patch",
            json!({"path": "main.rs", "old_text": "x", "new_text": "y", "replace_all": true}),
        )
        .expect("replace all");
        assert_eq!(fs::read_to_string(&file).expect("read"), "y\ny\n");
        let both = run_tool(
            &registry,
            "apply_patch",
            json!({"path": "main.rs", "old_text": "y", "new_text": "z", "expected_content": "y\ny\n", "replacement": "q"}),
        )
        .expect_err("mixed modes");
        assert!(matches!(both, ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn tool_schemas_describe_every_parameter() {
        let registry =
            ToolRegistry::new(root("schemas"), PermissionMode::ReadOnly).expect("registry");
        for name in CORE_TOOL_NAMES {
            let schema = registry.metadata(name).expect("metadata").input_schema;
            let properties = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} has no properties"));
            for (property, definition) in properties {
                assert!(
                    definition["description"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty()),
                    "{name}.{property} lacks a description"
                );
            }
            for required in schema["required"].as_array().into_iter().flatten() {
                assert!(
                    properties.contains_key(required.as_str().unwrap_or_default()),
                    "{name} requires undeclared {required}"
                );
            }
        }
    }

    #[test]
    fn large_read_output_spills_to_bounded_secure_reference() {
        let root = root("read-spill");
        fs::write(root.join("large.txt"), "x".repeat(32 * 1024)).expect("write");
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let outcome = registry
            .execute(
                "read_file",
                json!({"path":"large.txt"}),
                &CancellationToken::default(),
            )
            .expect("read");
        let spill = outcome.spill_ref.expect("spill reference");
        assert!(outcome.truncated);
        assert!(spill.starts_with(
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
                .unwrap_or_else(std::env::temp_dir)
                .join("helios/tool-output")
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(spill).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cache_resolver_never_uses_workspace_without_xdg_or_home() {
        let fallback = PathBuf::from("/tmp/helios-safe-temp");
        assert_eq!(
            resolve_tool_output_dir(None, None, fallback.clone()),
            fallback.join("helios/tool-output")
        );
        assert!(!resolve_tool_output_dir(None, None, fallback)
            .to_string_lossy()
            .contains(".agent"));
    }

    #[test]
    fn every_call_passes_permission_engine() {
        let root = root("permission");
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let error = registry
            .execute(
                "apply_patch",
                json!({"path":"new.txt","expected_content":"","replacement":"x"}),
                &CancellationToken::default(),
            )
            .expect_err("denied");
        assert!(matches!(
            error,
            ToolError::Permission(PermissionError::Denied {
                class: PermissionClass::WorkspaceWrite
            })
        ));
    }

    #[test]
    fn approval_blocks_before_write_and_allows_exact_request() {
        let root = root("approval");
        let file = root.join("file.txt");
        fs::write(&file, "old").expect("write");
        let (events, receiver) = mpsc::sync_channel(4);
        let broker = ApprovalBroker::new(events).with_timeout(Duration::from_millis(10));
        let worker_broker = broker.clone();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            ToolRegistry::new(&worker_root, PermissionMode::ReadOnly)
                .expect("registry")
                .with_approval_broker(worker_broker)
                .execute(
                    "apply_patch",
                    json!({"path":"file.txt","expected_content":"old","replacement":"new"}),
                    &CancellationToken::default(),
                )
        });
        let event = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("request");
        let agent_protocol::Event::PermissionRequested(text) = event else {
            panic!("request event")
        };
        let id = text
            .split(';')
            .find_map(|part| part.strip_prefix("id=")?.parse::<u64>().ok())
            .expect("request id");
        assert_eq!(fs::read_to_string(&file).expect("before approval"), "old");
        assert!(broker.decide(id, ApprovalDecision::AllowOnce));
        worker.join().expect("worker").expect("approved");
        assert_eq!(fs::read_to_string(file).expect("after approval"), "new");
    }

    #[test]
    fn approval_timeout_and_cancellation_fail_closed() {
        let root = root("approval-timeout");
        fs::write(root.join("file.txt"), "old").expect("write");
        let (events, _receiver) = mpsc::sync_channel(4);
        let broker = ApprovalBroker::new(events).with_timeout(Duration::from_millis(10));
        let result = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
            .expect("registry")
            .with_approval_broker(broker)
            .execute(
                "apply_patch",
                json!({"path":"file.txt","expected_content":"old","replacement":"new"}),
                &CancellationToken::default(),
            );
        assert!(matches!(result, Err(ToolError::Approval(_))));
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let (events, _receiver) = mpsc::sync_channel(4);
        let result = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
            .expect("registry")
            .with_approval_broker(ApprovalBroker::new(events))
            .execute(
                "apply_patch",
                json!({"path":"file.txt","expected_content":"old","replacement":"new"}),
                &cancellation,
            );
        assert!(matches!(result, Err(ToolError::Approval(_))));
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).expect("unchanged"),
            "old"
        );
    }

    #[test]
    fn approval_session_scope_is_exact_and_high_risk_not_persistent() {
        let root = root("approval-session");
        fs::write(root.join("file.txt"), "old").expect("write");
        let (events, receiver) = mpsc::sync_channel(8);
        let broker = ApprovalBroker::new(events).with_timeout(Duration::from_millis(10));
        let first = {
            let broker = broker.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                ToolRegistry::new(root, PermissionMode::WorkspaceWrite)
                    .expect("registry")
                    .with_approval_broker(broker)
                    .execute(
                        "apply_patch",
                        json!({"path":"file.txt","expected_content":"old","replacement":"new"}),
                        &CancellationToken::default(),
                    )
            })
        };
        let event = receiver.recv().expect("first request");
        let agent_protocol::Event::PermissionRequested(text) = event else {
            panic!("request")
        };
        let id = text
            .split(';')
            .find_map(|part| part.strip_prefix("id=")?.parse().ok())
            .unwrap();
        assert!(broker.decide(id, ApprovalDecision::AllowSession));
        first.join().expect("join").expect("first allowed");
        let second = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
            .expect("registry")
            .with_approval_broker(broker.clone())
            .execute(
                "apply_patch",
                json!({"path":"file.txt","expected_content":"new","replacement":"newer"}),
                &CancellationToken::default(),
            );
        assert!(
            second.is_err(),
            "different preimage must not inherit approval"
        );
    }

    #[test]
    fn snapshot_hook_captures_preimage_and_postimage_before_write() {
        let root = root("snapshot-hook");
        let file = root.join("file.txt");
        fs::write(&file, "old").expect("write");
        let events = Arc::new(Mutex::new(Vec::<SnapshotEvent>::new()));
        let receiver = events.clone();
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite)
            .expect("registry")
            .with_snapshot_hook(Arc::new(move |event| {
                receiver.lock().expect("events").push(event);
                Ok(())
            }));
        registry
            .execute(
                "apply_patch",
                json!({"path":"file.txt","expected_content":"old","replacement":"new"}),
                &CancellationToken::default(),
            )
            .expect("patch");
        let events = events.lock().expect("events");
        assert!(
            matches!(&events[0], SnapshotEvent::Before { content, existed: true, .. } if content == b"old")
        );
        assert!(
            matches!(&events[1], SnapshotEvent::After { content, existed: true, .. } if content == b"new")
        );
    }

    #[cfg(unix)]
    #[test]
    fn verification_argv_returns_result() {
        let root = root("verify");
        let registry = ToolRegistry::new(&root, PermissionMode::WorkspaceWrite).expect("registry");
        let result = registry
            .verify_argv(&["sh".into(), "-c".into(), "printf verified".into()])
            .expect("verification");
        assert_eq!(result.status, ToolStatus::Ok);
        assert!(result.output.contains("verified"));
    }

    #[cfg(unix)]
    #[test]
    fn git_diff_is_bounded_and_read_only() {
        let root = root("diff");
        let file = root.join("tracked.txt");
        fs::write(&file, "before\n").expect("file");
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.invalid"])
            .current_dir(&root)
            .status()
            .expect("git config");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status()
            .expect("git config");
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&root)
            .status()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-qm", "baseline"])
            .current_dir(&root)
            .status()
            .expect("git commit");
        fs::write(&file, "after\n").expect("change");
        let registry = ToolRegistry::new(&root, PermissionMode::ReadOnly).expect("registry");
        let diff = registry.git_diff().expect("diff");
        assert!(diff.data.contains("-before"));
        assert!(diff.data.contains("+after"));
        assert!(registry.git_diff_variant("stat").is_ok());
        assert!(matches!(
            registry.git_diff_variant("session"),
            Err(ToolError::InvalidArguments { .. })
        ));
    }
}
