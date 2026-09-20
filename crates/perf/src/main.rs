use agent_evals::{load_suite, run_suite};
use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_RUNS: usize = 10;
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct PerfReport {
    schema_version: u32,
    generated_at: String,
    environment: Environment,
    startup: StartupMetrics,
    short_session: SessionMetrics,
    benchmark: Option<BenchmarkMetrics>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Environment {
    os: String,
    arch: String,
    rustc: Option<String>,
    commit: Option<String>,
    warm_filesystem_caveat: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct StartupMetrics {
    runs: usize,
    samples_ms: Vec<u128>,
    p50_ms: u128,
    p95_ms: u128,
    #[serde(default)]
    version_p50_ms: u128,
    #[serde(default)]
    version_p95_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct SessionMetrics {
    wall_time_ms: u128,
    peak_rss_bytes: Option<u64>,
    exit_code: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct BenchmarkMetrics {
    task_count: usize,
    passed: usize,
    failed: usize,
    p50_wall_time_ms: u128,
    p95_wall_time_ms: u128,
    total_token_estimate: u64,
    total_tool_calls: u32,
    total_diff_bytes: usize,
    regression_count: usize,
    peak_rss_bytes: Option<u64>,
    p95_peak_rss_bytes: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
enum PerfError {
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("child failed: {0}")]
    Child(String),
    #[error("performance threshold failed: {0}")]
    Threshold(String),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agent-perf failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), PerfError> {
    let options = Options::parse(env::args().skip(1))?;
    if options.help {
        return Ok(());
    }
    let binary = options.binary.unwrap_or_else(default_agent_binary);
    let startup = measure_startup(&binary, options.runs)?;
    let short_session = measure_session(&binary)?;
    let benchmark = options
        .suite
        .as_ref()
        .map(|suite| run_benchmark(suite, options.fixtures.as_deref()))
        .transpose()?;
    let report = PerfReport {
        schema_version: 1,
        generated_at: now_string(),
        environment: environment(),
        startup,
        short_session,
        benchmark,
    };
    if let Some(max) = options.max_cold_start_ms {
        if report.startup.p95_ms > max {
            return Err(PerfError::Threshold(format!(
                "startup p95 {}ms exceeds {}ms",
                report.startup.p95_ms, max
            )));
        }
    }
    if let Some(max) = options.max_peak_rss_bytes {
        if report
            .short_session
            .peak_rss_bytes
            .is_some_and(|rss| rss > max)
        {
            return Err(PerfError::Threshold(format!(
                "peak RSS exceeds {max} bytes"
            )));
        }
    }
    if let Some(baseline) = options.baseline {
        compare_baseline(&report, &read_report(baseline)?, options.max_regressions)?;
    }
    let output = serde_json::to_vec_pretty(&report)?;
    if let Some(path) = options.output {
        fs::write(path, output)?;
    } else {
        println!("{}", String::from_utf8_lossy(&output));
    }
    Ok(())
}

#[derive(Default)]
struct Options {
    help: bool,
    binary: Option<PathBuf>,
    runs: usize,
    suite: Option<PathBuf>,
    fixtures: Option<PathBuf>,
    output: Option<PathBuf>,
    baseline: Option<PathBuf>,
    max_cold_start_ms: Option<u128>,
    max_peak_rss_bytes: Option<u64>,
    max_regressions: usize,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, PerfError> {
        let mut options = Self {
            help: false,
            runs: DEFAULT_RUNS,
            ..Self::default()
        };
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--binary" => options.binary = Some(PathBuf::from(next_value(&mut args, &arg)?)),
                "--runs" => options.runs = parse_positive(&next_value(&mut args, &arg)?)?,
                "--suite" => options.suite = Some(PathBuf::from(next_value(&mut args, &arg)?)),
                "--fixtures" => {
                    options.fixtures = Some(PathBuf::from(next_value(&mut args, &arg)?))
                }
                "--output" => options.output = Some(PathBuf::from(next_value(&mut args, &arg)?)),
                "--baseline" => {
                    options.baseline = Some(PathBuf::from(next_value(&mut args, &arg)?))
                }
                "--max-cold-start-ms" => {
                    options.max_cold_start_ms = Some(
                        next_value(&mut args, &arg)?
                            .parse()
                            .map_err(|_| PerfError::Child("invalid cold-start threshold".into()))?,
                    )
                }
                "--max-peak-rss-bytes" => {
                    options.max_peak_rss_bytes = Some(
                        next_value(&mut args, &arg)?
                            .parse()
                            .map_err(|_| PerfError::Child("invalid RSS threshold".into()))?,
                    )
                }
                "--max-regressions" => {
                    options.max_regressions = next_value(&mut args, &arg)?
                        .parse()
                        .map_err(|_| PerfError::Child("invalid regression threshold".into()))?
                }
                "--help" => {
                    println!("agent-perf [--binary PATH] [--runs N] [--suite PATH --fixtures PATH] [--output PATH] [--baseline PATH] [--max-cold-start-ms N] [--max-peak-rss-bytes N]");
                    options.help = true;
                    return Ok(options);
                }
                other => return Err(PerfError::Child(format!("unknown argument: {other}"))),
            }
        }
        Ok(options)
    }
}

fn next_value<I: Iterator<Item = String>>(args: &mut I, name: &str) -> Result<String, PerfError> {
    args.next()
        .ok_or_else(|| PerfError::Child(format!("missing value for {name}")))
}

fn parse_positive(value: &str) -> Result<usize, PerfError> {
    let number = value
        .parse()
        .map_err(|_| PerfError::Child("runs must be a positive integer".into()))?;
    if number == 0 {
        Err(PerfError::Child("runs must be positive".into()))
    } else {
        Ok(number)
    }
}

fn default_agent_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/helios")
}

fn measure_startup(binary: &Path, runs: usize) -> Result<StartupMetrics, PerfError> {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let status = Command::new(binary)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(PerfError::Child("helios --help failed".into()));
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
            .status()?;
        if !status.success() {
            return Err(PerfError::Child("helios --version failed".into()));
        }
        version_samples.push(started.elapsed().as_millis());
    }
    let p50 = percentile(&samples, 1, 2);
    let p95 = percentile(&samples, 19, 20);
    let version_p50 = percentile(&version_samples, 1, 2);
    let version_p95 = percentile(&version_samples, 19, 20);
    Ok(StartupMetrics {
        runs,
        samples_ms: samples,
        p50_ms: p50,
        p95_ms: p95,
        version_p50_ms: version_p50,
        version_p95_ms: version_p95,
    })
}

fn measure_session(binary: &Path) -> Result<SessionMetrics, PerfError> {
    let started = Instant::now();
    let mut child = Command::new(binary)
        .args(["--provider", "fake", "performance smoke"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut peak = None;
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        peak = peak_rss(child.id()).or(peak);
        if let Some(status) = child.try_wait()? {
            return Ok(SessionMetrics {
                wall_time_ms: started.elapsed().as_millis(),
                peak_rss_bytes: peak,
                exit_code: status.code(),
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PerfError::Child("short fake session timed out".into()));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn peak_rss(pid: u32) -> Option<u64> {
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

fn run_benchmark(
    suite_path: &Path,
    fixture_root: Option<&Path>,
) -> Result<BenchmarkMetrics, PerfError> {
    let suite = load_suite(suite_path).map_err(|error| PerfError::Child(error.to_string()))?;
    let fixtures = fixture_root.map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../evals/fixtures/workspaces")
    });
    let work = env::temp_dir().join(format!("agent-perf-work-{}", std::process::id()));
    let report =
        run_suite(&suite, fixtures, work).map_err(|error| PerfError::Child(error.to_string()))?;
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
}

fn compare_baseline(
    report: &PerfReport,
    baseline: &PerfReport,
    max_regressions: usize,
) -> Result<(), PerfError> {
    let startup_regression =
        report.startup.p50_ms > baseline.startup.p50_ms.saturating_mul(110) / 100;
    let rss_regression = match (
        report.short_session.peak_rss_bytes,
        baseline.short_session.peak_rss_bytes,
    ) {
        (Some(now), Some(old)) => now > old.saturating_add(15 * 1024 * 1024),
        _ => false,
    };
    let benchmark_regression = report
        .benchmark
        .as_ref()
        .map_or(0, |benchmark| benchmark.regression_count);
    let count =
        usize::from(startup_regression) + usize::from(rss_regression) + benchmark_regression;
    if count > max_regressions {
        return Err(PerfError::Threshold(format!(
            "{count} regressions exceed {max_regressions}"
        )));
    }
    Ok(())
}

fn read_report(path: PathBuf) -> Result<PerfReport, PerfError> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn percentile(values: &[u128], numerator: usize, denominator: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    values[(values.len() - 1) * numerator / denominator]
}

fn environment() -> Environment {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    Environment {
        os: env::consts::OS.into(),
        arch: env::consts::ARCH.into(),
        rustc,
        commit: env::var("GIT_COMMIT").ok(),
        warm_filesystem_caveat: true,
    }
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_deterministic_and_bounded() {
        assert_eq!(percentile(&[9, 1, 5, 3], 1, 2), 3);
        assert_eq!(percentile(&[], 19, 20), 0);
    }

    #[test]
    fn options_reject_zero_runs() {
        assert!(Options::parse(["--runs".into(), "0".into()].into_iter()).is_err());
    }

    #[test]
    fn baseline_thresholds_are_tolerant_without_rss_support() {
        let baseline = PerfReport {
            schema_version: 1,
            generated_at: "old".into(),
            environment: environment(),
            startup: StartupMetrics {
                runs: 1,
                samples_ms: vec![100],
                p50_ms: 100,
                p95_ms: 100,
                version_p50_ms: 1,
                version_p95_ms: 1,
            },
            short_session: SessionMetrics {
                wall_time_ms: 1,
                peak_rss_bytes: None,
                exit_code: Some(0),
            },
            benchmark: None,
        };
        let current = PerfReport {
            startup: StartupMetrics {
                runs: 1,
                samples_ms: vec![110],
                p50_ms: 110,
                p95_ms: 110,
                version_p50_ms: 1,
                version_p95_ms: 1,
            },
            ..baseline.clone()
        };
        assert!(compare_baseline(&current, &baseline, 0).is_ok());
    }
}
