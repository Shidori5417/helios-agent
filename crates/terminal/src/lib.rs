//! Process and PTY runtime boundary; execution is added in Milestone 2.

pub const DEFAULT_OUTPUT_LIMIT: usize = 8 * 1024;
static SPILL_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    io::{BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub output_limit: usize,
    pub spill_dir: PathBuf,
    pub requires_pty: bool,
    pub env_allowlist: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandStatus {
    Exited(i32),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputData {
    pub text: String,
    pub truncated: bool,
    pub spill_ref: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub status: CommandStatus,
    pub stdout: OutputData,
    pub stderr: OutputData,
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("failed to start command: {0}")]
    Start(#[from] std::io::Error),
    #[error("failed to read command output: {0}")]
    Read(std::io::Error),
    #[error("background job limit reached ({limit})")]
    TooManyBackgroundJobs { limit: usize },
    #[error("unknown background job: {0}")]
    UnknownJob(String),
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Runs once in the child between fork and exec (Unix only; ignored
/// elsewhere). It must only make async-signal-safe calls, e.g. applying a
/// sandbox, and must not allocate.
pub type ChildHook = std::sync::Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>;

pub fn run(
    spec: CommandSpec,
    cancellation: &CancellationToken,
) -> Result<CommandResult, TerminalError> {
    run_with_hook(spec, cancellation, None)
}

/// Like [`run`], with a hook applied in the child before exec. A PTY command
/// cannot take a hook, so it is refused instead of silently running unrestricted.
pub fn run_with_hook(
    spec: CommandSpec,
    cancellation: &CancellationToken,
    child_hook: Option<ChildHook>,
) -> Result<CommandResult, TerminalError> {
    if spec.requires_pty {
        if child_hook.is_some() {
            return Err(TerminalError::Start(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "PTY commands cannot run under the OS sandbox",
            )));
        }
        return run_pty(spec, cancellation);
    }
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(safe_environment(&spec.env_allowlist))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(hook) = &child_hook {
                hook()?;
            }
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = child_hook;
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let out_dir = spec.spill_dir.clone();
    let limit = spec.output_limit;
    let out_handle = thread::spawn(move || collect(stdout, limit, &out_dir, "stdout"));
    let err_dir = spec.spill_dir.clone();
    let err_handle = thread::spawn(move || collect(stderr, limit, &err_dir, "stderr"));
    let started = Instant::now();
    let status = loop {
        if cancellation.is_cancelled() {
            terminate_child(&mut child);
            break CommandStatus::Cancelled;
        }
        if started.elapsed() >= spec.timeout {
            terminate_child(&mut child);
            break CommandStatus::TimedOut;
        }
        if let Some(exit) = child.try_wait()? {
            break CommandStatus::Exited(exit.code().unwrap_or(-1));
        }
        thread::sleep(Duration::from_millis(5));
    };
    let _ = child.wait();
    let stdout = out_handle
        .join()
        .map_err(|_| TerminalError::Read(std::io::Error::other("stdout reader panicked")))??;
    let stderr = err_handle
        .join()
        .map_err(|_| TerminalError::Read(std::io::Error::other("stderr reader panicked")))??;
    Ok(CommandResult {
        status,
        stdout,
        stderr,
    })
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn run_pty(
    spec: CommandSpec,
    cancellation: &CancellationToken,
) -> Result<CommandResult, TerminalError> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| TerminalError::Start(std::io::Error::other(error.to_string())))?;
    let mut command = portable_pty::CommandBuilder::new(&spec.program);
    command.args(&spec.args);
    command.cwd(&spec.cwd);
    for (key, value) in safe_environment(&spec.env_allowlist) {
        command.env(key, value);
    }
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| TerminalError::Start(std::io::Error::other(error.to_string())))?;
    // Keep only the master after spawning; retaining the slave prevents EOF on the reader.
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| TerminalError::Start(std::io::Error::other(error.to_string())))?;
    let output_dir = spec.spill_dir.clone();
    let limit = spec.output_limit;
    let output = thread::spawn(move || collect(reader, limit, &output_dir, "pty"));
    let started = Instant::now();
    let status = loop {
        if cancellation.is_cancelled() {
            let _ = child.kill();
            break CommandStatus::Cancelled;
        }
        if started.elapsed() >= spec.timeout {
            let _ = child.kill();
            break CommandStatus::TimedOut;
        }
        if let Some(exit) = child
            .try_wait()
            .map_err(|error| TerminalError::Start(std::io::Error::other(error.to_string())))?
        {
            break CommandStatus::Exited(exit.exit_code() as i32);
        }
        thread::sleep(Duration::from_millis(5));
    };
    let _ = child.wait();
    let stdout = output
        .join()
        .map_err(|_| TerminalError::Read(std::io::Error::other("PTY reader panicked")))??;
    Ok(CommandResult {
        status,
        stdout,
        stderr: OutputData {
            text: String::new(),
            truncated: false,
            spill_ref: None,
        },
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundStatus {
    Running,
    Exited(i32),
    TimedOut,
    Cancelled,
}

struct BackgroundJob {
    cancellation: CancellationToken,
    result: std::sync::mpsc::Receiver<Result<CommandResult, TerminalError>>,
    completed: Option<Result<CommandResult, TerminalError>>,
}

pub struct BackgroundJobRegistry {
    max_jobs: usize,
    next_id: usize,
    jobs: std::collections::BTreeMap<String, BackgroundJob>,
}
impl BackgroundJobRegistry {
    pub fn new(max_jobs: usize) -> Self {
        Self {
            max_jobs: max_jobs.max(1),
            next_id: 1,
            jobs: std::collections::BTreeMap::new(),
        }
    }
    pub fn start(&mut self, spec: CommandSpec) -> Result<String, TerminalError> {
        if self.jobs.len() >= self.max_jobs {
            return Err(TerminalError::TooManyBackgroundJobs {
                limit: self.max_jobs,
            });
        }
        let id = format!("job-{:04}", self.next_id);
        self.next_id += 1;
        let token = CancellationToken::default();
        let copy = token.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = sender.send(run(spec, &copy));
        });
        self.jobs.insert(
            id.clone(),
            BackgroundJob {
                cancellation: token,
                result: receiver,
                completed: None,
            },
        );
        Ok(id)
    }
    pub fn cancel(&self, id: &str) -> Result<(), TerminalError> {
        self.jobs
            .get(id)
            .ok_or_else(|| TerminalError::UnknownJob(id.into()))?
            .cancellation
            .cancel();
        Ok(())
    }
    pub fn status(&mut self, id: &str) -> Result<BackgroundStatus, TerminalError> {
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| TerminalError::UnknownJob(id.into()))?;
        if job.completed.is_none() {
            match job.result.try_recv() {
                Ok(result) => job.completed = Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(BackgroundStatus::Running),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(TerminalError::Start(std::io::Error::other(
                        "background worker disconnected",
                    )))
                }
            }
        }
        match job.completed.as_ref().expect("completed result") {
            Ok(result) => Ok(status_of(result)),
            Err(error) => Err(TerminalError::Start(std::io::Error::other(
                error.to_string(),
            ))),
        }
    }
    pub fn wait(&mut self, id: &str) -> Result<CommandResult, TerminalError> {
        let job = self
            .jobs
            .remove(id)
            .ok_or_else(|| TerminalError::UnknownJob(id.into()))?;
        match job.completed {
            Some(result) => result,
            None => job.result.recv().map_err(|_| {
                TerminalError::Start(std::io::Error::other("background worker disconnected"))
            })?,
        }
    }
}
fn status_of(result: &CommandResult) -> BackgroundStatus {
    match result.status {
        CommandStatus::Exited(code) => BackgroundStatus::Exited(code),
        CommandStatus::TimedOut => BackgroundStatus::TimedOut,
        CommandStatus::Cancelled => BackgroundStatus::Cancelled,
    }
}

fn safe_environment(allowlist: &[String]) -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            if secret_env_name(key) {
                return false;
            }
            (allowlist.is_empty()
                && matches!(key.as_str(), "PATH" | "HOME" | "TERM" | "LANG" | "SHELL"))
                || allowlist.iter().any(|allowed| allowed == key)
                || key.starts_with("LC_")
        })
        .collect()
}

fn secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|marker| upper.contains(marker))
}

fn collect<R: Read>(
    reader: R,
    limit: usize,
    spill_dir: &PathBuf,
    stream: &str,
) -> Result<OutputData, TerminalError> {
    let mut reader = BufReader::new(reader);
    let mut text = Vec::new();
    let mut spill = None;
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).map_err(TerminalError::Read)?;
        if count == 0 {
            break;
        }
        let keep = limit.saturating_sub(text.len()).min(count);
        if keep < count && spill.is_none() {
            std::fs::create_dir_all(spill_dir).map_err(TerminalError::Read)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(spill_dir, std::fs::Permissions::from_mode(0o700));
            }
            let sequence = SPILL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = spill_dir.join(format!(
                "spill-{}-{}-{}",
                std::process::id(),
                sequence,
                stream
            ));
            let mut file = std::fs::File::create(&path).map_err(TerminalError::Read)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            file.write_all(&text).map_err(TerminalError::Read)?;
            spill = Some((path, file));
        }
        text.extend_from_slice(&buffer[..keep]);
        if let Some((_, file)) = spill.as_mut() {
            file.write_all(&buffer[..count])
                .map_err(TerminalError::Read)?;
        }
    }
    let spill_ref = spill.map(|(path, _)| path);
    Ok(OutputData {
        text: String::from_utf8_lossy(&text).into_owned(),
        truncated: spill_ref.is_some(),
        spill_ref,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(program: &str, args: &[&str], limit: usize, timeout: Duration) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args: args.iter().map(|value| (*value).into()).collect(),
            cwd: std::env::temp_dir(),
            timeout,
            output_limit: limit,
            spill_dir: std::env::temp_dir().join("agent-terminal-spill"),
            requires_pty: false,
            env_allowlist: vec![],
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounds_output_and_spills_tail() {
        let result = run(
            spec(
                "sh",
                &["-c", "printf 1234567890"],
                4,
                Duration::from_secs(1),
            ),
            &CancellationToken::default(),
        )
        .expect("run");
        assert_eq!(result.stdout.text, "1234");
        assert!(result.stdout.truncated);
        assert!(result
            .stdout
            .spill_ref
            .as_ref()
            .is_some_and(|path| path.exists()));
        let spill = std::fs::read_to_string(result.stdout.spill_ref.expect("spill reference"))
            .expect("spill contents");
        assert_eq!(spill, "1234567890");
    }

    #[cfg(unix)]
    #[test]
    fn times_out_and_can_cancel() {
        let timeout = run(
            spec("sh", &["-c", "sleep 1"], 128, Duration::from_millis(20)),
            &CancellationToken::default(),
        )
        .expect("timeout");
        assert_eq!(timeout.status, CommandStatus::TimedOut);
        let token = CancellationToken::default();
        let copy = token.clone();
        let handle = std::thread::spawn(move || {
            run(
                spec("sh", &["-c", "sleep 1"], 128, Duration::from_secs(2)),
                &copy,
            )
            .expect("cancel")
        });
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        assert_eq!(
            handle.join().expect("join").status,
            CommandStatus::Cancelled
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_process_group_descendant() {
        let pid_file =
            std::env::temp_dir().join(format!("agent-terminal-pid-{}", std::process::id()));
        let mut command = spec(
            "sh",
            &[
                "-c",
                &format!("tail -f /dev/null & echo $! > {}; wait", pid_file.display()),
            ],
            128,
            Duration::from_millis(80),
        );
        command.spill_dir = std::env::temp_dir().join("agent-terminal-spill-group");
        let result = run(command, &CancellationToken::default()).expect("run");
        assert_eq!(result.status, CommandStatus::TimedOut);
        let pid = std::fs::read_to_string(&pid_file)
            .expect("pid")
            .trim()
            .parse::<i32>()
            .expect("pid number");
        let mut gone = false;
        for _ in 0..20 {
            // A killed orphan may linger as a zombie when PID 1 does not reap
            // (common in containers); it no longer runs, so treat it as gone.
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
                });
            if state.is_none_or(|state| state == "Z" || state == "X") {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(gone, "descendant process survived group termination: {pid}");
    }

    #[test]
    fn background_registry_has_bounded_lifecycle() {
        let mut registry = BackgroundJobRegistry::new(1);
        let first = registry
            .start(spec("printf", &["background"], 4, Duration::from_secs(1)))
            .expect("job");
        assert_eq!(first, "job-0001");
        assert!(matches!(
            registry.start(spec("printf", &["second"], 4, Duration::from_secs(1))),
            Err(TerminalError::TooManyBackgroundJobs { limit: 1 })
        ));
        let result = registry.wait(&first).expect("wait");
        assert_eq!(result.stdout.text, "back");
        assert!(matches!(
            registry.status(&first),
            Err(TerminalError::UnknownJob(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn background_registry_cancels_and_reaps() {
        let mut registry = BackgroundJobRegistry::new(2);
        let id = registry
            .start(spec(
                "sh",
                &["-c", "tail -f /dev/null"],
                32,
                Duration::from_secs(2),
            ))
            .expect("job");
        registry.cancel(&id).expect("cancel");
        assert_eq!(
            registry.wait(&id).expect("wait").status,
            CommandStatus::Cancelled
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_pty_echo_smoke() {
        let mut command = spec("sh", &["-c", "printf pty"], 16, Duration::from_secs(1));
        command.requires_pty = true;
        let result = run(command, &CancellationToken::default()).expect("pty");
        assert_eq!(result.status, CommandStatus::Exited(0));
        assert!(result.stdout.text.contains("pty"));
    }
}
