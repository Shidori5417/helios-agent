use agent_sandbox::{PermissionClass, PermissionEngine, PermissionError, PermissionMode};
use agent_terminal::{run, CancellationToken, CommandSpec, CommandStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

pub const CORE_TOOL_NAMES: &[&str] = &[
    "read_file",
    "search_files",
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
    #[error("patch conflict: file changed since it was read")]
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
}

impl agent_core::ToolExecutor for ToolRegistry {
    fn definitions(&self) -> Vec<agent_protocol::ToolDefinition> {
        CORE_TOOL_NAMES
            .iter()
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
        Ok(Self {
            permissions: PermissionEngine::new(&root, mode)?,
            output_budget: 8 * 1024,
            timeout: Duration::from_secs(30),
            spill_dir: root.join(".agent/spill"),
            cancellation: CancellationToken::default(),
            approval: None,
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

    pub fn metadata(&self, name: &str) -> Option<ToolMetadata> {
        let read_schema = || json!({"type":"object","required":["path"]});
        let command_schema = || json!({"type":"object","required":["program","args"],"properties":{"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"requires_pty":{"type":"boolean"}}});
        let (description, class, schema, parallel, budget, timeout) = match name {
            "read_file" => (
                "Read a bounded file range",
                PermissionClass::ReadOnly,
                read_schema(),
                false,
                self.output_budget,
                self.timeout,
            ),
            "search_files" => (
                "Search workspace text",
                PermissionClass::ReadOnly,
                json!({"type":"object","required":["query"]}),
                true,
                self.output_budget,
                self.timeout,
            ),
            "apply_patch" => (
                "Apply an exact preimage patch",
                PermissionClass::WorkspaceWrite,
                json!({"type":"object","required":["path","expected_content","replacement"]}),
                false,
                1024,
                self.timeout,
            ),
            "shell" => (
                "Run one direct process",
                PermissionClass::WorkspaceWrite,
                command_schema(),
                false,
                self.output_budget,
                self.timeout,
            ),
            "process" => (
                "Run a cancellable process",
                PermissionClass::Process,
                command_schema(),
                false,
                self.output_budget,
                self.timeout,
            ),
            "update_plan" => (
                "Update machine-readable task state",
                PermissionClass::WorkspaceWrite,
                json!({"type":"object"}),
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
        let metadata = self
            .metadata(name)
            .ok_or_else(|| ToolError::UnknownTool(name.into()))?;
        let policy_result = self.permissions.authorize(metadata.permission_class);
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
            "apply_patch" => self.apply_patch(args),
            "shell" | "process" => self.command(name, args, cancellation),
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
        self.permissions.authorize(PermissionClass::ReadOnly)?;
        let result = run(
            CommandSpec {
                program: "git".into(),
                args: vec!["diff".into(), "--no-ext-diff".into(), "--".into()],
                cwd: self.permissions.root().to_path_buf(),
                timeout: self.timeout,
                output_limit: self.output_budget,
                spill_dir: self.spill_dir.clone(),
                requires_pty: false,
                env_allowlist: vec![],
            },
            &CancellationToken::default(),
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
        let result = run(
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
        let path = self.permissions.resolve_path(input.path)?;
        let content = fs::read_to_string(path)?;
        let start = input.start_line.unwrap_or(1).max(1);
        let end = input.end_line.unwrap_or(usize::MAX);
        let selected = content
            .lines()
            .enumerate()
            .filter(|(index, _)| *index + 1 >= start && *index < end)
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(bounded(
            ToolStatus::Ok,
            "file read",
            selected,
            self.output_budget,
        ))
    }

    fn search_files(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            query: String,
            path: Option<String>,
        }
        let input: Input = parse("search_files", args)?;
        let root = match input.path {
            Some(path) => self.permissions.resolve_path(path)?,
            None => self.permissions.root().to_path_buf(),
        };
        let mut hits = String::new();
        search_recursive(&self.permissions, &root, &input.query, &mut hits)?;
        Ok(bounded(
            ToolStatus::Ok,
            "search complete",
            hits,
            self.output_budget,
        ))
    }

    fn apply_patch(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        #[derive(Deserialize)]
        struct Input {
            path: String,
            expected_content: String,
            replacement: String,
        }
        let input: Input = parse("apply_patch", args)?;
        let path = self.permissions.resolve_path(input.path)?;
        let current = fs::read_to_string(&path)?;
        if current != input.expected_content {
            return Err(ToolError::PatchConflict);
        }
        fs::write(path, input.replacement)?;
        Ok(bounded(
            ToolStatus::Ok,
            "patch applied",
            "".into(),
            self.output_budget,
        ))
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
        let result = run(
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
        )?;
        let status = match result.status {
            CommandStatus::Exited(0) => ToolStatus::Ok,
            CommandStatus::Exited(_) => ToolStatus::Error,
            CommandStatus::TimedOut => ToolStatus::Timeout,
            CommandStatus::Cancelled => ToolStatus::Cancelled,
        };
        let text = format!(
            "stdout:\n{}\nstderr:\n{}",
            result.stdout.text, result.stderr.text
        );
        Ok(ToolOutcome {
            status,
            summary: format!("{name} completed"),
            data: text,
            truncated: result.stdout.truncated || result.stderr.truncated,
            spill_ref: result.stdout.spill_ref.or(result.stderr.spill_ref),
        })
    }
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
    ToolOutcome {
        status,
        summary: summary.into(),
        data: data[..limit].to_owned(),
        truncated: true,
        spill_ref: None,
    }
}

fn search_recursive(
    engine: &PermissionEngine,
    root: &Path,
    query: &str,
    hits: &mut String,
) -> Result<(), ToolError> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if engine.resolve_path(&path).is_err() {
            continue;
        }
        if path.is_dir() {
            search_recursive(engine, &path, query, hits)?;
            continue;
        }
        if let Ok(content) = fs::read_to_string(&path) {
            for (line, text) in content.lines().enumerate() {
                if text.contains(query) {
                    hits.push_str(&format!("{}:{}:{}\n", path.display(), line + 1, text));
                }
            }
        }
    }
    Ok(())
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
        let broker = ApprovalBroker::new(events);
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
    }
}
