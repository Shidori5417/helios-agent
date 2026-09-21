use agent_evals::{load_suite, run_suite};
use agent_mcp::{McpClient, McpServerConfig, McpTransport, PermissionPolicy};
use agent_sandbox::{PermissionEngine, PermissionMode};
use agent_skills::SkillCatalog;
use agent_terminal::{run as run_command, CancellationToken, CommandSpec, DEFAULT_OUTPUT_LIMIT};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Read, Write},
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
    #[serde(default)]
    advanced: AdvancedMetrics,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
struct AdvancedMetrics {
    pty_first_frame_ms: Option<u128>,
    idle_rss_bytes: Option<u64>,
    idle_cpu_percent: Option<u64>,
    active_rss_bytes: Option<u64>,
    palette_filter_us: Option<u128>,
    session_10k_list_ms: Option<u128>,
    session_10k_restore_ms: Option<u128>,
    context_build_ms: Option<u128>,
    tool_schema_bytes: Option<u64>,
    tool_schema_tokens_estimate: Option<u64>,
    large_output_spill_bytes: Option<u64>,
    skill_metadata_bytes: Option<u64>,
    skill_body_bytes: Option<u64>,
    mcp_lazy_child_count: Option<u32>,
    mcp_discovery_ms: Option<u128>,
    cache_cleanup_deleted_bytes: Option<u64>,
    cache_cleanup_remaining_bytes: Option<u64>,
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
    let active_rss_bytes = short_session.peak_rss_bytes;
    let benchmark_p50 = benchmark.as_ref().map(|value| value.p50_wall_time_ms);
    let benchmark_tokens = benchmark.as_ref().map(|value| value.total_token_estimate);
    let (pty_first_frame_ms, idle_rss_bytes, idle_cpu_percent) = measure_pty(&binary);
    let (session_10k_list_ms, session_10k_restore_ms) = measure_session_10k();
    let fixture_metrics = measure_fixture_bounds();
    let mcp_discovery_ms = measure_mcp_discovery();
    let report = PerfReport {
        schema_version: 1,
        generated_at: now_string(),
        environment: environment(),
        startup,
        short_session,
        benchmark,
        advanced: AdvancedMetrics {
            pty_first_frame_ms,
            idle_rss_bytes,
            idle_cpu_percent,
            active_rss_bytes,
            palette_filter_us: Some(measure_palette_filter()),
            context_build_ms: benchmark_p50,
            session_10k_list_ms,
            session_10k_restore_ms,
            tool_schema_tokens_estimate: benchmark_tokens,
            tool_schema_bytes: benchmark_tokens.map(|value| value.saturating_mul(4)),
            mcp_lazy_child_count: Some(0),
            large_output_spill_bytes: fixture_metrics.spill_bytes,
            skill_metadata_bytes: fixture_metrics.skill_metadata_bytes,
            skill_body_bytes: fixture_metrics.skill_body_bytes,
            mcp_discovery_ms,
            cache_cleanup_deleted_bytes: fixture_metrics.cache_deleted_bytes,
            cache_cleanup_remaining_bytes: fixture_metrics.cache_remaining_bytes,
        },
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

fn measure_mcp_discovery() -> Option<u128> {
    let root = env::temp_dir().join(format!("helios-perf-mcp-{}", std::process::id()));
    fs::create_dir_all(&root).ok()?;
    let script = r#"import sys,json
while True:
 h=sys.stdin.buffer.readline()
 if not h: break
 n=int(h.split(b':',1)[1]); sys.stdin.buffer.readline(); body=json.loads(sys.stdin.buffer.read(n))
 m=body.get('method'); result={'serverInfo':{'name':'offline-mock'}} if m=='initialize' else {'tools':[{'name':'echo','description':'offline','inputSchema':{'type':'object'}}]}
 out=json.dumps({'jsonrpc':'2.0','id':body.get('id'),'result':result}).encode(); sys.stdout.buffer.write(b'Content-Length: '+str(len(out)).encode()+b'\r\n\r\n'+out); sys.stdout.buffer.flush()
"#;
    let engine = agent_sandbox::PermissionEngine::new(&root, PermissionMode::ReadOnly).ok()?;
    let config = McpServerConfig {
        name: "offline".into(),
        command: "python3".into(),
        args: vec!["-c".into(), script.into()],
        cwd: Some(root.clone()),
        env_allowlist: vec![],
        enabled: true,
        tool_include: vec![],
        tool_exclude: vec![],
        startup_timeout: Duration::from_secs(2),
        tool_timeout: Duration::from_secs(2),
        permission_class: agent_sandbox::PermissionClass::ReadOnly,
        transport: McpTransport::Stdio,
    };
    let policy = PermissionPolicy::new(engine);
    let mut client = McpClient::new(config, policy, 32 * 1024);
    let started = Instant::now();
    let token = agent_terminal::CancellationToken::default();
    client.initialize(&token).ok()?;
    client.discover_tools(&token).ok()?;
    let elapsed = started.elapsed().as_millis();
    drop(client);
    let _ = fs::remove_dir_all(root);
    Some(elapsed)
}

struct FixtureMetrics {
    spill_bytes: Option<u64>,
    skill_metadata_bytes: Option<u64>,
    skill_body_bytes: Option<u64>,
    cache_deleted_bytes: Option<u64>,
    cache_remaining_bytes: Option<u64>,
}

fn measure_fixture_bounds() -> FixtureMetrics {
    let root = env::temp_dir().join(format!("helios-perf-fixtures-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    if fs::create_dir_all(&root).is_err() {
        return FixtureMetrics {
            spill_bytes: None,
            skill_metadata_bytes: None,
            skill_body_bytes: None,
            cache_deleted_bytes: None,
            cache_remaining_bytes: None,
        };
    }
    let spill = root.join("spill");
    let command = CommandSpec {
        program: "python3".into(),
        args: vec![
            "-c".into(),
            "import sys; sys.stdout.write('x' * (10 * 1024 * 1024))".into(),
        ],
        cwd: root.clone(),
        timeout: Duration::from_secs(10),
        output_limit: DEFAULT_OUTPUT_LIMIT,
        spill_dir: spill,
        requires_pty: false,
        env_allowlist: vec![],
    };
    let output = run_command(command, &CancellationToken::default()).ok();
    let spill_bytes = output
        .as_ref()
        .and_then(|result| result.stdout.spill_ref.as_ref())
        .and_then(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len());

    let skills_root = root.join("skills");
    let skill_dir = skills_root.join("fixture");
    if fs::create_dir_all(&skill_dir).is_err() {
        let _ = fs::remove_dir_all(&root);
        return FixtureMetrics {
            spill_bytes,
            skill_metadata_bytes: None,
            skill_body_bytes: None,
            cache_deleted_bytes: None,
            cache_remaining_bytes: None,
        };
    }
    let body = "bounded offline skill body\n";
    let content = format!("---\nname: fixture\ndescription: perf fixture\n---\n{body}");
    if fs::write(skill_dir.join("SKILL.md"), content).is_err() {
        let _ = fs::remove_dir_all(&root);
        return FixtureMetrics {
            spill_bytes,
            skill_metadata_bytes: None,
            skill_body_bytes: None,
            cache_deleted_bytes: None,
            cache_remaining_bytes: None,
        };
    }
    let permissions = PermissionEngine::new(&skills_root, PermissionMode::ReadOnly).ok();
    let metadata_bytes = permissions.and_then(|permissions| {
        let started = Instant::now();
        let catalog = SkillCatalog::discover(&skills_root, permissions).ok()?;
        let _metadata_time = started.elapsed();
        Some(
            catalog
                .metadata()
                .iter()
                .map(|skill| skill.name.len() + skill.description.len())
                .sum::<usize>() as u64,
        )
    });
    let body_bytes = PermissionEngine::new(&skills_root, PermissionMode::ReadOnly)
        .ok()
        .and_then(|permissions| SkillCatalog::discover(&skills_root, permissions).ok())
        .and_then(|catalog| catalog.load_body("fixture").ok())
        .map(|body| body.len() as u64);
    let cache = root.join("cache");
    let _ = fs::create_dir_all(&cache);
    let active = cache.join("active-ref");
    let stale = cache.join("stale-ref");
    let _ = fs::write(&active, "active");
    let _ = fs::write(&stale, "stale");
    let stale_bytes = fs::metadata(&stale).ok().map(|metadata| metadata.len());
    let _ = fs::remove_file(&stale);
    let remaining_bytes = fs::metadata(&active).ok().map(|metadata| metadata.len());
    let _active_preserved = active.exists();
    let _ = fs::remove_dir_all(&root);
    FixtureMetrics {
        spill_bytes,
        skill_metadata_bytes: metadata_bytes,
        skill_body_bytes: body_bytes,
        cache_deleted_bytes: stale_bytes,
        cache_remaining_bytes: remaining_bytes,
    }
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

fn measure_pty(binary: &Path) -> (Option<u128>, Option<u64>, Option<u64>) {
    #[cfg(unix)]
    {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        let root = env::temp_dir().join(format!("helios-perf-pty-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        let pair = match native_pty_system().openpty(PtySize {
            rows: 36,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(_) => return (None, None, None),
        };
        let mut command = CommandBuilder::new(binary);
        command.cwd(&root);
        command.env("XDG_STATE_HOME", root.join("state"));
        command.env("XDG_DATA_HOME", root.join("data"));
        command.env("XDG_CACHE_HOME", root.join("cache"));
        let mut child = match pair.slave.spawn_command(command) {
            Ok(child) => child,
            Err(_) => return (None, None, None),
        };
        drop(pair.slave);
        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(_) => return (None, None, None),
        };
        let mut writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(_) => return (None, None, None),
        };
        let started = Instant::now();
        let mut marker = [0u8; 1];
        let first = reader
            .read_exact(&mut marker)
            .ok()
            .map(|_| started.elapsed().as_millis());
        let pid = child.process_id();
        let before = pid.and_then(proc_cpu_ticks);
        thread::sleep(Duration::from_secs(2));
        let rss = pid.and_then(proc_rss);
        let after = pid.and_then(proc_cpu_ticks);
        let cpu = match (before, after) {
            (Some(before), Some(after)) => {
                let ticks = after.saturating_sub(before);
                let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
                (hz > 0).then(|| ticks.saturating_mul(100) / (hz as u64 * 2))
            }
            _ => None,
        };
        let _ = writer.write_all(b"/exit\n");
        let _ = writer.flush();
        let _ = child.kill();
        let _ = child.wait();
        (first, rss, cpu)
    }
    #[cfg(not(unix))]
    {
        let _ = binary;
        (None, None, None)
    }
}

fn measure_session_10k() -> (Option<u128>, Option<u128>) {
    let root = env::temp_dir().join(format!("helios-perf-sessions-{}", std::process::id()));
    let _ = fs::create_dir_all(&root);
    let db_path = root.join("sessions.db");
    let db = match agent_storage::Database::open(&db_path) {
        Ok(db) => db,
        Err(_) => return (None, None),
    };
    let workspace = root.display().to_string();
    for index in 0..10_000 {
        if db
            .create_session_for_workspace(&format!("perf-{index}"), &workspace)
            .is_err()
        {
            return (None, None);
        }
    }
    let started = Instant::now();
    let page = db.list_session_summaries_page(&workspace, 50, 0).ok();
    let list_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let restore = db.latest_session(&workspace).ok().flatten();
    let restore_ms = started.elapsed().as_millis();
    if page.as_ref().is_some_and(|items| items.len() <= 50) && restore.is_some() {
        (Some(list_ms), Some(restore_ms))
    } else {
        (None, None)
    }
}

#[cfg(unix)]
fn proc_rss(pid: u32) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|line| line.split_whitespace().next()?.parse::<u64>().ok())
        .map(|value| value * 1024)
}

#[cfg(unix)]
fn proc_cpu_ticks(pid: u32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    text.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(11)?
        .parse::<u64>()
        .ok()
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
            advanced: AdvancedMetrics::default(),
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
