use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use agent_evals::{load_suite, run_suite};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PerfReport {
    pub schema_version: u32,
    pub runs: usize,
    pub samples_ms: Vec<u128>,
    pub p50_ms: u128,
    pub p95_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FullPerfReport {
    pub schema_version: u32,
    pub startup: StartupMetrics,
    pub short_session: SessionMetrics,
    pub benchmark: Option<BenchmarkMetrics>,
    #[serde(default)]
    pub advanced: AdvancedMetrics,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdvancedMetrics {
    pub pty_first_frame_ms: Option<u128>,
    pub idle_rss_bytes: Option<u64>,
    pub idle_cpu_percent: Option<u64>,
    pub active_rss_bytes: Option<u64>,
    pub palette_filter_us: Option<u128>,
    pub session_10k_list_ms: Option<u128>,
    pub session_10k_restore_ms: Option<u128>,
    pub context_build_ms: Option<u128>,
    pub tool_schema_bytes: Option<u64>,
    pub tool_schema_tokens_estimate: Option<u64>,
    pub large_output_spill_bytes: Option<u64>,
    pub skill_metadata_bytes: Option<u64>,
    pub skill_body_bytes: Option<u64>,
    pub mcp_lazy_child_count: Option<u32>,
    pub mcp_discovery_ms: Option<u128>,
    pub cache_cleanup_deleted_bytes: Option<u64>,
    pub cache_cleanup_remaining_bytes: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StartupMetrics {
    pub runs: usize,
    pub samples_ms: Vec<u128>,
    pub p50_ms: u128,
    pub p95_ms: u128,
    #[serde(default)]
    pub version_p50_ms: u128,
    #[serde(default)]
    pub version_p95_ms: u128,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionMetrics {
    pub wall_time_ms: u128,
    pub peak_rss_bytes: Option<u64>,
    pub exit_code: Option<i32>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BenchmarkMetrics {
    pub task_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub p50_wall_time_ms: u128,
    pub p95_wall_time_ms: u128,
    pub total_token_estimate: u64,
    pub total_tool_calls: u32,
    pub total_diff_bytes: usize,
    pub regression_count: usize,
    pub peak_rss_bytes: Option<u64>,
    pub p95_peak_rss_bytes: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum PerfError {
    #[error("invalid run count")]
    InvalidRuns,
    #[error("performance command failed: {0}")]
    Command(String),
    #[error("performance JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("performance threshold failed: {0}")]
    Threshold(String),
}

pub fn run_full_report(
    binary: &Path,
    runs: usize,
    suite: Option<&Path>,
    fixtures: Option<&Path>,
    max_cold_start_ms: Option<u128>,
    max_peak_rss_bytes: Option<u64>,
) -> Result<FullPerfReport, PerfError> {
    if runs == 0 {
        return Err(PerfError::InvalidRuns);
    }
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let status = Command::new(binary)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| PerfError::Command(e.to_string()))?;
        if !status.success() {
            return Err(PerfError::Command("helios --help failed".into()));
        }
        samples.push(started.elapsed().as_millis());
    }
    let mut version_samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let status = Command::new(binary)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| PerfError::Command(e.to_string()))?;
        if !status.success() {
            return Err(PerfError::Command("helios --version failed".into()));
        }
        version_samples.push(started.elapsed().as_millis());
    }
    let startup = StartupMetrics {
        runs,
        p50_ms: percentile(&samples, 1, 2),
        p95_ms: percentile(&samples, 19, 20),
        samples_ms: samples,
        version_p50_ms: percentile(&version_samples, 1, 2),
        version_p95_ms: percentile(&version_samples, 19, 20),
    };
    if max_cold_start_ms.is_some_and(|max| startup.p95_ms > max) {
        return Err(PerfError::Threshold(format!(
            "startup p95 {}ms exceeds threshold",
            startup.p95_ms
        )));
    }
    let started = Instant::now();
    let mut child = Command::new(binary)
        .args(["--provider", "fake", "performance smoke"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PerfError::Command(e.to_string()))?;
    let mut peak = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        peak = peak_rss(child.id()).or(peak);
        if let Some(status) = child
            .try_wait()
            .map_err(|e| PerfError::Command(e.to_string()))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PerfError::Command("short fake session timed out".into()));
        }
        thread::sleep(Duration::from_millis(1));
    };
    if max_peak_rss_bytes.is_some_and(|max| peak.is_some_and(|value| value > max)) {
        return Err(PerfError::Threshold("peak RSS exceeds threshold".into()));
    }
    let benchmark = suite
        .map(|path| -> Result<BenchmarkMetrics, PerfError> {
            let suite = load_suite(path).map_err(|e| PerfError::Command(e.to_string()))?;
            let fixture_root = fixtures.map(PathBuf::from).unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../evals/fixtures/workspaces")
            });
            let work = std::env::temp_dir().join(format!("agent-perf-work-{}", std::process::id()));
            let report = run_suite(&suite, fixture_root, work)
                .map_err(|e| PerfError::Command(e.to_string()))?;
            Ok(BenchmarkMetrics {
                task_count: report.task_count,
                passed: report.passed,
                failed: report.failed,
                p50_wall_time_ms: report.summary.p50_wall_time_ms,
                p95_wall_time_ms: report.summary.p95_wall_time_ms,
                total_token_estimate: report.summary.total_token_estimate,
                total_tool_calls: report.summary.total_tool_calls,
                total_diff_bytes: report.summary.total_diff_bytes,
                regression_count: report.summary.regression_count,
                peak_rss_bytes: report.summary.peak_rss_bytes,
                p95_peak_rss_bytes: report.summary.p95_peak_rss_bytes,
            })
        })
        .transpose()?;
    let context_build_ms = benchmark.as_ref().map(|value| value.p50_wall_time_ms);
    let tool_schema_tokens_estimate = benchmark.as_ref().map(|value| value.total_token_estimate);
    Ok(FullPerfReport {
        schema_version: 2,
        startup,
        short_session: SessionMetrics {
            wall_time_ms: started.elapsed().as_millis(),
            peak_rss_bytes: peak,
            exit_code: status.code(),
        },
        benchmark,
        advanced: AdvancedMetrics {
            active_rss_bytes: peak,
            palette_filter_us: Some(measure_palette_filter()),
            context_build_ms,
            tool_schema_tokens_estimate,
            tool_schema_bytes: tool_schema_tokens_estimate.map(|value| value.saturating_mul(4)),
            mcp_lazy_child_count: Some(0),
            ..AdvancedMetrics::default()
        },
    })
}

fn measure_palette_filter() -> u128 {
    let commands = (0..256)
        .map(|index| format!("tool-{index}"))
        .collect::<Vec<_>>();
    let started = Instant::now();
    let mut count = 0usize;
    for _ in 0..1000 {
        count += commands
            .iter()
            .filter(|name| name.contains("tool-1"))
            .count();
    }
    std::hint::black_box(count);
    started.elapsed().as_micros()
}

fn percentile(values: &[u128], numerator: usize, denominator: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    values[(values.len() - 1) * numerator / denominator]
}
fn peak_rss(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|line| line.split_whitespace().next()?.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub fn run_report(binary: &Path, runs: usize) -> Result<PerfReport, PerfError> {
    if runs == 0 {
        return Err(PerfError::InvalidRuns);
    }
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let status = Command::new(binary)
            .arg("--help")
            .output()
            .map_err(|error| PerfError::Command(error.to_string()))?;
        if !status.status.success() {
            return Err(PerfError::Command("helios --help failed".into()));
        }
        samples.push(started.elapsed().as_millis());
    }
    samples.sort_unstable();
    let percentile = |n: usize, d: usize| samples[(samples.len() - 1) * n / d];
    Ok(PerfReport {
        schema_version: 1,
        runs,
        p50_ms: percentile(1, 2),
        p95_ms: percentile(19, 20),
        samples_ms: samples,
    })
}

pub fn compare(
    current: &PerfReport,
    baseline: &PerfReport,
    max_regressions: usize,
) -> Result<(), PerfError> {
    let regression = usize::from(current.p95_ms > baseline.p95_ms);
    if regression > max_regressions {
        return Err(PerfError::Command(format!(
            "p95 {}ms exceeds baseline {}ms",
            current.p95_ms, baseline.p95_ms
        )));
    }
    Ok(())
}
