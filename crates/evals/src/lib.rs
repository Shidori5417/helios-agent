//! Deterministic offline benchmark runner for smoke and the checked 100-task suite.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Instant,
};

pub const INITIAL_BENCHMARK_TASKS: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BenchmarkSuite {
    pub schema_version: u32,
    pub name: String,
    pub tasks: Vec<BenchmarkTask>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BenchmarkTask {
    pub id: String,
    pub category: String,
    pub fixture: String,
    #[serde(default)]
    pub fixture_seed: String,
    pub prompt: String,
    pub action: FakeAction,
    pub budget: Budget,
    pub validation: ValidationSpec,
    #[serde(default)]
    pub hidden_validation: Option<ValidationSpec>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum FakeAction {
    Inspect,
    ExactPatch {
        path: String,
        expected: String,
        replacement: String,
    },
    MultiFilePatch {
        files: Vec<PatchSpec>,
    },
    SearchEvidence {
        path: String,
        needle: String,
    },
    RefuseUnsafePath {
        path: String,
    },
    RefuseSymlinkEscape {
        path: String,
    },
    ValidationCheck,
    BudgetCheck,
    PatchConflictRecovery {
        path: String,
        replacement: String,
    },
    RefuseSecretPersistence {
        key: String,
        content: String,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PatchSpec {
    pub path: String,
    pub expected: String,
    pub replacement: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Budget {
    pub max_iterations: u32,
    pub max_tool_calls: u32,
    pub max_tokens: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValidationSpec {
    pub argv: Vec<String>,
    pub expected_exit: i32,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskMetrics {
    pub id: String,
    pub success: bool,
    pub regression: bool,
    pub changed_files: usize,
    pub diff_bytes: usize,
    pub iterations: u32,
    pub tool_calls: u32,
    pub token_estimate: u64,
    pub wall_time_ms: u128,
    pub peak_rss_bytes: Option<u64>,
    pub validation_exit: i32,
    pub hidden_validation_exit: Option<i32>,
    pub refusal_expected: bool,
    pub failure: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EvalReport {
    pub suite: String,
    pub task_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub metrics: Vec<TaskMetrics>,
    pub summary: AggregateMetrics,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AggregateMetrics {
    pub categories: BTreeMap<String, CategoryMetrics>,
    pub p50_wall_time_ms: u128,
    pub p95_wall_time_ms: u128,
    pub total_diff_bytes: usize,
    pub total_tool_calls: u32,
    pub total_token_estimate: u64,
    pub regression_count: usize,
    pub peak_rss_bytes: Option<u64>,
    pub p95_peak_rss_bytes: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct CategoryMetrics {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("eval I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("eval schema failed: {0}")]
    Schema(#[from] serde_json::Error),
    #[error("invalid task: {0}")]
    InvalidTask(String),
    #[error("task failed: {0}")]
    Failed(String),
    #[error("benchmark regression threshold exceeded: {0}")]
    Regression(String),
}

pub fn load_suite(path: impl AsRef<Path>) -> Result<BenchmarkSuite, EvalError> {
    let suite: BenchmarkSuite = serde_json::from_str(&fs::read_to_string(path)?)?;
    validate_suite(&suite)?;
    Ok(suite)
}
pub fn write_report(report: &EvalReport, path: impl AsRef<Path>) -> Result<(), EvalError> {
    fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}
pub fn run_suite(
    suite: &BenchmarkSuite,
    fixture_root: impl AsRef<Path>,
    work_root: impl AsRef<Path>,
) -> Result<EvalReport, EvalError> {
    validate_suite(suite)?;
    let fixture_root = fixture_root.as_ref();
    let work_root = work_root.as_ref();
    fs::create_dir_all(work_root)?;
    let mut metrics = Vec::with_capacity(suite.tasks.len());
    for task in &suite.tasks {
        metrics.push(run_task(task, fixture_root, work_root)?);
    }
    let passed = metrics.iter().filter(|metric| metric.success).count();
    let summary = summarize(suite, &metrics);
    Ok(EvalReport {
        suite: suite.name.clone(),
        task_count: metrics.len(),
        passed,
        failed: metrics.len() - passed,
        metrics,
        summary,
    })
}
pub fn check_baseline(
    report: &EvalReport,
    baseline: &EvalReport,
    max_regressions: usize,
) -> Result<(), EvalError> {
    let old = baseline
        .metrics
        .iter()
        .map(|metric| (metric.id.as_str(), metric.success))
        .collect::<BTreeMap<_, _>>();
    let regressions = report
        .metrics
        .iter()
        .filter(|metric| old.get(metric.id.as_str()) == Some(&true) && !metric.success)
        .count();
    if regressions > max_regressions {
        return Err(EvalError::Regression(format!(
            "{regressions} regressions exceed threshold {max_regressions}"
        )));
    }
    Ok(())
}
impl EvalReport {
    pub fn deterministic_key(&self) -> String {
        self.metrics
            .iter()
            .map(|m| {
                format!(
                    "{}:{}:{}:{}:{}:{}:{}",
                    m.id,
                    m.success,
                    m.regression,
                    m.changed_files,
                    m.diff_bytes,
                    m.iterations,
                    m.tool_calls
                )
            })
            .collect::<Vec<_>>()
            .join("|")
    }
}

fn run_task(
    task: &BenchmarkTask,
    fixture_root: &Path,
    work_root: &Path,
) -> Result<TaskMetrics, EvalError> {
    let started = Instant::now();
    let source = fixture_root.join(&task.fixture);
    let destination = work_root.join(&task.id);
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    copy_tree(&source, &destination)?;
    if matches!(&task.action, FakeAction::RefuseSymlinkEscape { .. }) {
        #[cfg(unix)]
        {
            let outside = work_root.join("outside");
            fs::create_dir_all(&outside)?;
            std::os::unix::fs::symlink(&outside, destination.join("link"))?;
        }
    }
    let before = snapshot(&destination)?;
    let refusal_expected = matches!(task.action, FakeAction::RefuseUnsafePath { .. });
    let mut failure = None;
    let (iterations, tool_calls, token_estimate) = match &task.action {
        FakeAction::Inspect => (1, 1, estimate_tokens(&task.prompt)),
        FakeAction::ExactPatch {
            path,
            expected,
            replacement,
        } => {
            let target = safe_join(&destination, path)?;
            let current = fs::read_to_string(&target)?;
            if &current != expected {
                failure = Some("exact preimage mismatch".into());
            } else {
                fs::write(target, replacement)?;
            }
            (
                1,
                1,
                estimate_tokens(&task.prompt).saturating_add(estimate_tokens(replacement)),
            )
        }
        FakeAction::MultiFilePatch { files } => {
            for file in files {
                let target = safe_join(&destination, path_or_name(&file.path)?)?;
                let current = fs::read_to_string(&target)?;
                if current != file.expected {
                    failure = Some(format!("exact preimage mismatch: {}", file.path));
                    break;
                }
                fs::write(target, &file.replacement)?;
            }
            (1, files.len() as u32, estimate_tokens(&task.prompt))
        }
        FakeAction::SearchEvidence { path, needle } => {
            let target = safe_join(&destination, path)?;
            let content = fs::read_to_string(target)?;
            if !content.contains(needle) {
                failure = Some("required evidence was not found".into());
            }
            (1, 1, estimate_tokens(&task.prompt))
        }
        FakeAction::RefuseUnsafePath { path } => {
            if safe_join(&destination, path).is_ok() {
                failure = Some("unsafe path was accepted".into());
            }
            (1, 1, estimate_tokens(&task.prompt))
        }
        FakeAction::RefuseSymlinkEscape { path } => {
            if safe_join(&destination, path).is_ok() {
                failure = Some("symlink escape was accepted".into());
            }
            (1, 1, estimate_tokens(&task.prompt))
        }
        FakeAction::ValidationCheck | FakeAction::BudgetCheck => {
            (1, 1, estimate_tokens(&task.prompt))
        }
        FakeAction::PatchConflictRecovery { path, replacement } => {
            let target = safe_join(&destination, path)?;
            let current = fs::read_to_string(&target)?;
            if current == *replacement {
                failure = Some("conflict simulation unexpectedly had replacement".into());
            } else {
                fs::write(target, replacement)?;
            }
            (
                2,
                2,
                estimate_tokens(&task.prompt).saturating_add(estimate_tokens(replacement)),
            )
        }
        FakeAction::RefuseSecretPersistence { key, content } => {
            if key.contains("token")
                || content.to_ascii_lowercase().contains("secret")
                || content.to_ascii_lowercase().contains("bearer ")
            {
                (1, 1, estimate_tokens(&task.prompt))
            } else {
                failure = Some("secret refusal fixture was not secret-like".into());
                (1, 1, estimate_tokens(&task.prompt))
            }
        }
    };
    let (validation_exit, validation_output, mut validation_rss) =
        run_argv(&task.validation, &destination)?;
    if validation_exit != task.validation.expected_exit && failure.is_none() {
        failure = Some(format!(
            "validation exit {validation_exit}: {validation_output}"
        ));
    }
    if iterations > task.budget.max_iterations
        || tool_calls > task.budget.max_tool_calls
        || token_estimate > task.budget.max_tokens
    {
        failure = Some("budget regression".into());
    }
    let hidden_validation_exit = if let Some(hidden) = &task.hidden_validation {
        let (exit, output, hidden_rss) = run_argv(hidden, &destination)?;
        validation_rss = validation_rss.max(hidden_rss);
        if exit != hidden.expected_exit && failure.is_none() {
            failure = Some(format!("hidden validation exit {exit}: {output}"));
        }
        Some(exit)
    } else {
        None
    };
    let after = snapshot(&destination)?;
    let paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let changed_files = paths
        .iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .count();
    let diff_bytes = paths
        .iter()
        .map(|path| {
            before
                .get(path)
                .copied()
                .unwrap_or(0)
                .abs_diff(after.get(path).copied().unwrap_or(0))
        })
        .sum();
    let success = failure.is_none();
    Ok(TaskMetrics {
        id: task.id.clone(),
        success,
        regression: !success,
        changed_files,
        diff_bytes,
        iterations,
        tool_calls,
        token_estimate,
        wall_time_ms: started.elapsed().as_millis(),
        peak_rss_bytes: validation_rss,
        validation_exit,
        hidden_validation_exit,
        refusal_expected,
        failure,
    })
}
fn summarize(suite: &BenchmarkSuite, metrics: &[TaskMetrics]) -> AggregateMetrics {
    let mut categories = BTreeMap::<String, CategoryMetrics>::new();
    for (task, metric) in suite.tasks.iter().zip(metrics) {
        let category = categories.entry(task.category.clone()).or_default();
        category.total += 1;
        if metric.success {
            category.passed += 1;
        } else {
            category.failed += 1;
        }
    }
    let mut wall_times = metrics
        .iter()
        .map(|metric| metric.wall_time_ms)
        .collect::<Vec<_>>();
    wall_times.sort_unstable();
    let percentile = |numerator: usize, denominator: usize| -> u128 {
        if wall_times.is_empty() {
            0
        } else {
            wall_times[(wall_times.len() - 1) * numerator / denominator]
        }
    };
    let mut rss = metrics
        .iter()
        .filter_map(|metric| metric.peak_rss_bytes)
        .collect::<Vec<_>>();
    rss.sort_unstable();
    let rss_percentile = |numerator: usize, denominator: usize| -> Option<u64> {
        (!rss.is_empty()).then(|| rss[(rss.len() - 1) * numerator / denominator])
    };
    let peak_rss_bytes = rss.iter().copied().max();
    AggregateMetrics {
        categories,
        p50_wall_time_ms: percentile(1, 2),
        p95_wall_time_ms: percentile(19, 20),
        total_diff_bytes: metrics.iter().map(|metric| metric.diff_bytes).sum(),
        total_tool_calls: metrics.iter().map(|metric| metric.tool_calls).sum(),
        total_token_estimate: metrics.iter().map(|metric| metric.token_estimate).sum(),
        regression_count: metrics.iter().filter(|metric| metric.regression).count(),
        peak_rss_bytes,
        p95_peak_rss_bytes: rss_percentile(19, 20),
    }
}
fn validate_suite(suite: &BenchmarkSuite) -> Result<(), EvalError> {
    if suite.schema_version != 1 || suite.tasks.is_empty() {
        return Err(EvalError::InvalidTask(
            "schema_version must be 1 and tasks non-empty".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    for task in &suite.tasks {
        if task.id.is_empty() || !ids.insert(&task.id) {
            return Err(EvalError::InvalidTask(format!(
                "duplicate/empty id: {}",
                task.id
            )));
        }
        if task.fixture.contains("..") || task.fixture.starts_with('/') {
            return Err(EvalError::InvalidTask(format!(
                "fixture escapes root: {}",
                task.fixture
            )));
        }
        if task.validation.argv.is_empty() {
            return Err(EvalError::InvalidTask(format!(
                "empty validation argv: {}",
                task.id
            )));
        }
        if task.budget.max_iterations == 0
            || task.budget.max_tool_calls == 0
            || task.budget.max_tokens == 0
        {
            return Err(EvalError::InvalidTask(format!("zero budget: {}", task.id)));
        }
    }
    Ok(())
}
fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, EvalError> {
    let candidate = root.join(relative);
    if relative.starts_with('/')
        || relative.split('/').any(|part| part == "..")
        || !candidate.starts_with(root)
    {
        return Err(EvalError::InvalidTask(format!("unsafe path: {relative}")));
    }
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let parent = candidate.parent().unwrap_or(root);
    let canonical_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(EvalError::InvalidTask(format!(
            "symlink escape: {relative}"
        )));
    }
    Ok(candidate)
}
fn path_or_name(path: &str) -> Result<&str, EvalError> {
    if path.is_empty() {
        Err(EvalError::InvalidTask("empty patch path".into()))
    } else {
        Ok(path)
    }
}
fn run_argv(spec: &ValidationSpec, cwd: &Path) -> Result<(i32, String, Option<u64>), EvalError> {
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| EvalError::InvalidTask("empty validation argv".into()))?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut peak = None;
    while child.try_wait()?.is_none() {
        peak = process_peak_rss(child.id()).or(peak);
        thread::sleep(std::time::Duration::from_millis(1));
    }
    peak = process_peak_rss(child.id()).or(peak);
    let output = child.wait_with_output()?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        peak,
    ))
}

fn process_peak_rss(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        Some(value.saturating_mul(1024))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}
fn copy_tree(source: &Path, destination: &Path) -> Result<(), EvalError> {
    if !source.is_dir() {
        return Err(EvalError::InvalidTask(format!(
            "fixture missing: {}",
            source.display()
        )));
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), target)?;
        } else if entry.file_type()?.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, target)?;
            #[cfg(not(unix))]
            return Err(EvalError::InvalidTask(
                "symlink fixture requires Unix".into(),
            ));
        } else {
            return Err(EvalError::InvalidTask(
                "fixture contains unsupported special file".into(),
            ));
        }
    }
    Ok(())
}
fn snapshot(root: &Path) -> Result<BTreeMap<String, usize>, EvalError> {
    fn visit(
        root: &Path,
        path: &Path,
        result: &mut BTreeMap<String, usize>,
    ) -> Result<(), EvalError> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            if entry.file_type()?.is_dir() {
                visit(root, &child, result)?;
            } else if entry.file_type()?.is_file() {
                result.insert(
                    child
                        .strip_prefix(root)
                        .expect("snapshot root")
                        .display()
                        .to_string(),
                    fs::metadata(child)?.len() as usize,
                );
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result)?;
    Ok(result)
}
fn estimate_tokens(text: &str) -> u64 {
    ((text.len() as u64).saturating_add(3)) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agent-eval-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("temp");
        path
    }
    fn suite() -> BenchmarkSuite {
        serde_json::from_str(include_str!("../fixtures/tasks.json")).expect("schema")
    }
    fn fixtures(root: &Path) {
        for name in ["inspect", "patch", "escape"] {
            fs::create_dir_all(root.join(name)).expect("fixture");
            fs::write(root.join(name).join("README.txt"), "before\n").expect("file");
        }
    }
    #[test]
    fn schema_and_fixture_isolation_work() {
        let root = temp("schema");
        fixtures(&root);
        let report = run_suite(&suite(), &root, root.join("work")).expect("run");
        assert_eq!(report.task_count, 3);
        assert_eq!(
            fs::read_to_string(root.join("patch/README.txt")).expect("source"),
            "before\n"
        );
        assert_eq!(report.metrics[1].changed_files, 1);
    }
    #[test]
    fn pass_fail_and_refusal_are_scored() {
        let root = temp("score");
        fixtures(&root);
        let report = run_suite(&suite(), &root, root.join("work")).expect("run");
        assert_eq!(report.passed, 3);
        assert!(report.metrics[2].refusal_expected);
    }
    #[test]
    fn deterministic_key_ignores_wall_clock() {
        let root = temp("deterministic");
        fixtures(&root);
        let first = run_suite(&suite(), &root, root.join("a")).expect("run");
        let second = run_suite(&suite(), &root, root.join("b")).expect("run");
        assert_eq!(first.deterministic_key(), second.deterministic_key());
    }
    #[test]
    fn budget_regression_fails_task() {
        let root = temp("budget");
        fixtures(&root);
        let mut suite = suite();
        suite.tasks[0].budget.max_tokens = 1;
        let report = run_suite(&suite, &root, root.join("work")).expect("run");
        assert_eq!(report.failed, 1);
        assert_eq!(
            report.metrics[0].failure.as_deref(),
            Some("budget regression")
        );
    }

    #[test]
    fn checked_benchmark_has_one_hundred_unique_tasks_and_ten_archetypes() {
        let suite: BenchmarkSuite =
            serde_json::from_str(include_str!("../fixtures/benchmark-100.json")).expect("suite");
        validate_suite(&suite).expect("valid benchmark");
        assert_eq!(suite.tasks.len(), INITIAL_BENCHMARK_TASKS);
        assert_eq!(
            suite
                .tasks
                .iter()
                .map(|task| task.category.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            10
        );
        assert!(suite.tasks.iter().all(|task| !task.fixture_seed.is_empty()));
    }

    #[test]
    fn baseline_check_rejects_new_failure_over_threshold() {
        let root = temp("baseline");
        fixtures(&root);
        let baseline = run_suite(&suite(), &root, root.join("baseline-work")).expect("baseline");
        let mut candidate = baseline.clone();
        candidate.metrics[0].success = false;
        candidate.metrics[0].regression = true;
        assert!(check_baseline(&candidate, &baseline, 0).is_err());
        check_baseline(&candidate, &baseline, 1).expect("threshold");
    }
}
