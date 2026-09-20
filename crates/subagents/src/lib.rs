use agent_context::{ContextPlan, ContextSegment};
use agent_sandbox::{PermissionEngine, PermissionMode};
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IsolationMode {
    Shared,
    Worktree {
        temp_root: PathBuf,
        base_ref: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("source is not a git work tree: {0}")]
    NotGit(String),
    #[error("invalid worktree path: {0}")]
    InvalidPath(String),
    #[error("worktree already exists: {0}")]
    Collision(PathBuf),
    #[error("git command failed: {0}")]
    Git(String),
    #[error("task scope is outside source repository: {0}")]
    ScopeOutside(PathBuf),
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub base_ref: String,
}

pub struct WorktreeManager {
    source_repo: PathBuf,
    temp_root: PathBuf,
    owned: BTreeSet<PathBuf>,
    next_id: u64,
}

impl WorktreeManager {
    pub fn new(
        source_repo: impl AsRef<std::path::Path>,
        temp_root: impl AsRef<std::path::Path>,
    ) -> Result<Self, WorktreeError> {
        let source_repo = source_repo.as_ref().canonicalize()?;
        let temp_root = temp_root.as_ref().canonicalize().or_else(|_| {
            fs::create_dir_all(temp_root.as_ref())?;
            temp_root.as_ref().canonicalize()
        })?;
        if source_repo == temp_root || temp_root.starts_with(&source_repo) {
            return Err(WorktreeError::InvalidPath(
                "temporary root must not be the source repository".into(),
            ));
        }
        let detected = git_output(&source_repo, &["rev-parse", "--show-toplevel"])
            .map_err(|_| WorktreeError::NotGit(source_repo.display().to_string()))?;
        let detected = PathBuf::from(detected.trim()).canonicalize()?;
        if detected != source_repo {
            return Err(WorktreeError::NotGit(source_repo.display().to_string()));
        }
        Ok(Self {
            source_repo,
            temp_root,
            owned: BTreeSet::new(),
            next_id: 1,
        })
    }

    pub fn create(&mut self, base_ref: Option<&str>) -> Result<WorktreeHandle, WorktreeError> {
        let base_ref = base_ref.unwrap_or("HEAD").to_owned();
        let resolved = git_output(&self.source_repo, &["rev-parse", "--verify", &base_ref])?;
        let id = format!("agent-worktree-{:04}", self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        let path = self.temp_root.join(id);
        if !path.starts_with(&self.temp_root) || path == self.source_repo || path.exists() {
            return Err(WorktreeError::Collision(path));
        }
        git_status(
            &self.source_repo,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str()
                    .ok_or_else(|| WorktreeError::InvalidPath("non-UTF8 path".into()))?,
                resolved.trim(),
            ],
        )?;
        self.owned.insert(path.clone());
        Ok(WorktreeHandle { path, base_ref })
    }

    pub fn cleanup(&mut self, handle: WorktreeHandle) -> Result<(), WorktreeError> {
        if !self.owned.remove(&handle.path)
            || !handle.path.starts_with(&self.temp_root)
            || handle.path == self.source_repo
        {
            return Err(WorktreeError::InvalidPath(
                handle.path.display().to_string(),
            ));
        }
        git_status(
            &self.source_repo,
            &[
                "worktree",
                "remove",
                "--force",
                handle
                    .path
                    .to_str()
                    .ok_or_else(|| WorktreeError::InvalidPath("non-UTF8 path".into()))?,
            ],
        )
    }

    pub fn reroot_task(
        &self,
        task: &TaskSpec,
        handle: &WorktreeHandle,
    ) -> Result<TaskSpec, WorktreeError> {
        let mut rerooted = task.clone();
        rerooted.scope = task
            .scope
            .iter()
            .map(|scope| {
                let relative = scope
                    .strip_prefix(&self.source_repo)
                    .map_err(|_| WorktreeError::ScopeOutside(scope.clone()))?;
                Ok(handle.path.join(relative))
            })
            .collect::<Result<Vec<_>, WorktreeError>>()?;
        Ok(rerooted)
    }
}

fn git_output(cwd: &std::path::Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(WorktreeError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn git_status(cwd: &std::path::Path, args: &[&str]) -> Result<(), WorktreeError> {
    git_output(cwd, args).map(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budgets {
    pub max_iterations: u32,
    pub max_tokens: u64,
    pub max_tool_calls: u32,
    pub wall_clock: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            max_tokens: 16_000,
            max_tool_calls: 32,
            wall_clock: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub id: String,
    pub objective: String,
    pub context: Vec<ContextSegment>,
    pub scope: Vec<PathBuf>,
    pub permission_mode: PermissionMode,
    pub budgets: Budgets,
}

impl TaskSpec {
    pub fn new(id: impl Into<String>, objective: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            objective: objective.into(),
            context: Vec::new(),
            scope: Vec::new(),
            permission_mode: PermissionMode::ReadOnly,
            budgets: Budgets::default(),
        }
    }

    pub fn with_scope(mut self, scope: Vec<PathBuf>) -> Self {
        self.scope = scope;
        self
    }
    pub fn with_context(mut self, context: Vec<ContextSegment>) -> Self {
        self.context = context;
        self
    }
    pub fn with_context_plan(mut self, plan: &ContextPlan) -> Self {
        self.context = plan
            .segments
            .iter()
            .map(|planned| planned.segment.clone())
            .collect();
        self
    }
    pub fn with_permission(mut self, permission_mode: PermissionMode) -> Self {
        self.permission_mode = permission_mode;
        self
    }
    pub fn with_budgets(mut self, budgets: Budgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Completed,
    IterationBudget,
    TokenBudget,
    ToolCallBudget,
    WallClock,
    Cancelled,
    PermissionDenied,
    ScopeConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskResult {
    pub id: String,
    pub summary: String,
    pub iterations: u32,
    pub tokens: u64,
    pub tool_calls: u32,
    pub stop_reason: StopReason,
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("task {task} requests permission mode above parent")]
    PermissionDenied { task: String },
    #[error("task {task} scope is invalid: {reason}")]
    InvalidScope { task: String, reason: String },
    #[error("duplicate task id: {0}")]
    DuplicateTask(String),
}

pub trait Executor: Send + Sync {
    fn execute(
        &self,
        task: &TaskSpec,
        cancellation: &CancellationToken,
        active: &AtomicUsize,
    ) -> TaskResult;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FakeExecutor;
impl Executor for FakeExecutor {
    fn execute(
        &self,
        task: &TaskSpec,
        cancellation: &CancellationToken,
        _active: &AtomicUsize,
    ) -> TaskResult {
        let started = Instant::now();
        let mut iterations = 0;
        let mut tokens = 0;
        let mut calls = 0;
        loop {
            if cancellation.is_cancelled() {
                return result(task, iterations, tokens, calls, StopReason::Cancelled);
            }
            if started.elapsed() >= task.budgets.wall_clock {
                return result(task, iterations, tokens, calls, StopReason::WallClock);
            }
            if iterations >= task.budgets.max_iterations {
                return result(task, iterations, tokens, calls, StopReason::IterationBudget);
            }
            if tokens.saturating_add(4) > task.budgets.max_tokens {
                return result(task, iterations, tokens, calls, StopReason::TokenBudget);
            }
            if calls >= task.budgets.max_tool_calls {
                return result(task, iterations, tokens, calls, StopReason::ToolCallBudget);
            }
            iterations += 1;
            tokens = tokens.saturating_add(4);
            calls += 1;
            if task.objective.starts_with("complete") {
                return result(task, iterations, tokens, calls, StopReason::Completed);
            }
        }
    }
}

fn result(
    task: &TaskSpec,
    iterations: u32,
    tokens: u64,
    tool_calls: u32,
    stop_reason: StopReason,
) -> TaskResult {
    TaskResult {
        id: task.id.clone(),
        summary: format!(
            "{} ({} context segments)",
            task.objective,
            task.context.len()
        ),
        iterations,
        tokens,
        tool_calls,
        stop_reason,
    }
}

pub struct Scheduler {
    concurrency_cap: usize,
}
impl Scheduler {
    pub fn new(concurrency_cap: usize) -> Self {
        Self {
            concurrency_cap: concurrency_cap.max(1),
        }
    }

    pub fn run<E: Executor>(
        &self,
        root: &PermissionEngine,
        parent_mode: PermissionMode,
        tasks: Vec<TaskSpec>,
        executor: &E,
        cancellation: CancellationToken,
    ) -> Result<Vec<TaskResult>, SchedulerError> {
        validate_tasks(root, parent_mode, &tasks)?;
        let mut batches: Vec<Vec<usize>> = Vec::new();
        for index in 0..tasks.len() {
            let mut placed = false;
            for batch in &mut batches {
                if batch.len() < self.concurrency_cap
                    && batch
                        .iter()
                        .all(|other| !overlaps(&tasks[index].scope, &tasks[*other].scope))
                {
                    batch.push(index);
                    placed = true;
                    break;
                }
            }
            if !placed {
                batches.push(vec![index]);
            }
        }
        let active = Arc::new(AtomicUsize::new(0));
        let mut results = Vec::with_capacity(tasks.len());
        for batch in batches {
            let mut batch_results = thread::scope(|scope| {
                let handles = batch
                    .iter()
                    .map(|&index| {
                        let token = cancellation.clone();
                        let active = Arc::clone(&active);
                        let task = &tasks[index];
                        scope.spawn(move || {
                            active.fetch_add(1, Ordering::AcqRel);
                            let result = executor.execute(task, &token, &active);
                            active.fetch_sub(1, Ordering::AcqRel);
                            (index, result)
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("subagent panicked"))
                    .collect::<Vec<_>>()
            });
            batch_results.sort_by_key(|(index, _)| *index);
            results.extend(batch_results.into_iter().map(|(_, result)| result));
        }
        Ok(results)
    }
}

fn validate_tasks(
    root: &PermissionEngine,
    parent_mode: PermissionMode,
    tasks: &[TaskSpec],
) -> Result<(), SchedulerError> {
    let mut ids = std::collections::BTreeSet::new();
    for task in tasks {
        if !ids.insert(task.id.clone()) {
            return Err(SchedulerError::DuplicateTask(task.id.clone()));
        }
        if mode_rank(task.permission_mode) > mode_rank(parent_mode) {
            return Err(SchedulerError::PermissionDenied {
                task: task.id.clone(),
            });
        }
        for path in &task.scope {
            root.resolve_path(path)
                .map_err(|error| SchedulerError::InvalidScope {
                    task: task.id.clone(),
                    reason: error.to_string(),
                })?;
        }
    }
    Ok(())
}
fn mode_rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::ReadOnly => 0,
        PermissionMode::WorkspaceWrite => 1,
    }
}
fn overlaps(left: &[PathBuf], right: &[PathBuf]) -> bool {
    left.iter().any(|a| {
        right
            .iter()
            .any(|b| a == b || a.starts_with(b) || b.starts_with(a))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command, sync::atomic::AtomicUsize};

    fn root(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("agent-subagents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("root");
        path
    }
    fn task(id: &str, path: &std::path::Path) -> TaskSpec {
        TaskSpec::new(id, "complete task").with_scope(vec![path.to_path_buf()])
    }

    #[test]
    fn context_scope_and_permission_are_inherited() {
        let root = root("inherit");
        let engine = PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine");
        let task = task("write", &root).with_permission(PermissionMode::WorkspaceWrite);
        let error = Scheduler::new(1)
            .run(
                &engine,
                PermissionMode::ReadOnly,
                vec![task],
                &FakeExecutor,
                CancellationToken::default(),
            )
            .expect_err("deny");
        assert!(matches!(error, SchedulerError::PermissionDenied { .. }));
    }

    #[test]
    fn overlapping_scopes_are_serialized_and_results_are_deterministic() {
        let root = root("overlap");
        let a = root.join("a");
        let b = a.join("b");
        fs::create_dir_all(&b).expect("scope");
        let engine = PermissionEngine::new(&root, PermissionMode::WorkspaceWrite).expect("engine");
        let tasks = vec![task("a", &a), task("b", &b)];
        let results = Scheduler::new(2)
            .run(
                &engine,
                PermissionMode::WorkspaceWrite,
                tasks,
                &FakeExecutor,
                CancellationToken::default(),
            )
            .expect("run");
        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn budgets_and_cancellation_stop_fake_execution() {
        let root = root("budget");
        let engine = PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine");
        let limited = TaskSpec::new("limited", "work")
            .with_scope(vec![root.clone()])
            .with_budgets(Budgets {
                max_iterations: 1,
                ..Budgets::default()
            });
        let result = Scheduler::new(1)
            .run(
                &engine,
                PermissionMode::ReadOnly,
                vec![limited],
                &FakeExecutor,
                CancellationToken::default(),
            )
            .expect("run");
        assert_eq!(result[0].stop_reason, StopReason::IterationBudget);
        let token = CancellationToken::default();
        token.cancel();
        let cancelled = Scheduler::new(1)
            .run(
                &engine,
                PermissionMode::ReadOnly,
                vec![task("cancelled", &root)],
                &FakeExecutor,
                token,
            )
            .expect("run");
        assert_eq!(cancelled[0].stop_reason, StopReason::Cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn worktree_isolated_from_dirty_source_and_cleanup_is_scoped() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let source = root("worktree-source");
        let temp = root("worktree-temp");
        run_git(&source, &["init", "--quiet"]);
        run_git(&source, &["config", "user.email", "agent@example.invalid"]);
        run_git(&source, &["config", "user.name", "Agent Test"]);
        fs::write(source.join("tracked.txt"), "clean\n").expect("file");
        run_git(&source, &["add", "tracked.txt"]);
        run_git(&source, &["commit", "--quiet", "-m", "initial"]);
        fs::write(source.join("tracked.txt"), "dirty\n").expect("dirty");
        fs::write(source.join("untracked.txt"), "must not copy\n").expect("untracked");
        let mut manager = WorktreeManager::new(&source, &temp).expect("manager");
        let handle = manager.create(None).expect("create");
        assert_eq!(
            fs::read_to_string(handle.path.join("tracked.txt")).expect("tracked"),
            "clean\n"
        );
        assert!(!handle.path.join("untracked.txt").exists());
        let task =
            TaskSpec::new("isolated", "complete").with_scope(vec![source.join("tracked.txt")]);
        let rerooted = manager.reroot_task(&task, &handle).expect("reroot");
        assert_eq!(rerooted.scope, vec![handle.path.join("tracked.txt")]);
        let path = handle.path.clone();
        manager.cleanup(handle).expect("cleanup");
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(source.join("tracked.txt")).expect("source"),
            "dirty\n"
        );
        assert!(source.join("untracked.txt").exists());
    }

    #[test]
    fn worktree_rejects_non_git_and_invalid_scope() {
        let source = root("not-git");
        let temp = root("not-git-temp");
        assert!(matches!(
            WorktreeManager::new(&source, &temp),
            Err(WorktreeError::NotGit(_))
        ));
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    struct ObservingExecutor;
    impl Executor for ObservingExecutor {
        fn execute(
            &self,
            task: &TaskSpec,
            _cancel: &CancellationToken,
            active: &AtomicUsize,
        ) -> TaskResult {
            assert!(active.load(Ordering::Acquire) <= 2);
            result(task, 1, 1, 0, StopReason::Completed)
        }
    }
    #[test]
    fn concurrency_cap_is_enforced() {
        let root = root("cap");
        let engine = PermissionEngine::new(&root, PermissionMode::ReadOnly).expect("engine");
        let tasks = (0..5)
            .map(|index| task(&format!("{index}"), &root.join(format!("{index}"))))
            .collect::<Vec<_>>();
        for task in &tasks {
            fs::create_dir_all(&task.scope[0]).expect("scope");
        }
        let results = Scheduler::new(2)
            .run(
                &engine,
                PermissionMode::ReadOnly,
                tasks,
                &ObservingExecutor,
                CancellationToken::default(),
            )
            .expect("run");
        assert_eq!(results.len(), 5);
    }
}
