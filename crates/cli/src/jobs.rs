//! Background jobs, agent tasks and the job supervisor, with their on-disk state.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct JobMeta {
    pub(crate) id: String,
    pub(crate) workspace: String,
    pub(crate) executable: String,
    pub(crate) argv_hash: String,
    pub(crate) pid: Option<u32>,
    pub(crate) pgid: Option<i32>,
    pub(crate) start_ticks: Option<u64>,
    pub(crate) status: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TaskMeta {
    pub(crate) id: String,
    pub(crate) job_id: String,
    pub(crate) workspace: String,
    pub(crate) objective: String,
    #[serde(default)]
    pub(crate) kind: String,
    pub(crate) state: String,
    pub(crate) provider: String,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) scope: String,
    pub(crate) permission: String,
    pub(crate) max_iterations: u32,
    pub(crate) max_tokens: u64,
    pub(crate) max_tool_calls: u32,
    pub(crate) wall_clock_seconds: u64,
    pub(crate) result: Option<String>,
}

pub(crate) fn jobs_dir(workspace: &Path) -> PathBuf {
    AppPaths::discover(workspace)
        .map(|paths| paths.jobs_dir)
        .unwrap_or_else(|_| workspace.join(".helios/jobs"))
}

pub(crate) fn job_path(workspace: &Path, id: &str) -> PathBuf {
    jobs_dir(workspace).join(format!("{id}.json"))
}

pub(crate) fn task_path(workspace: &Path, id: &str) -> PathBuf {
    jobs_dir(workspace).join(format!("{id}.task.json"))
}

pub(crate) fn read_task(workspace: &Path, id: &str) -> Result<TaskMeta, String> {
    if !safe_job_id(id) {
        return Err("invalid task id".into());
    }
    serde_json::from_slice(
        &fs::read(task_path(workspace, id)).map_err(|_| "task not found".to_owned())?,
    )
    .map_err(|error| format!("invalid task metadata: {error}"))
}

pub(crate) fn write_task(workspace: &Path, task: &TaskMeta) -> Result<(), String> {
    atomic_write(
        &task_path(workspace, &task.id),
        &serde_json::to_vec_pretty(task).map_err(|e| e.to_string())?,
    )
}

pub(crate) fn safe_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn job_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

pub(crate) fn read_job(workspace: &Path, id: &str) -> Result<JobMeta, String> {
    if !safe_job_id(id) {
        return Err("invalid job id".into());
    }
    let path = job_path(workspace, id);
    serde_json::from_slice(&fs::read(path).map_err(|_| "job not found".to_owned())?)
        .map_err(|error| format!("invalid job metadata: {error}"))
}

pub(crate) fn write_job(workspace: &Path, meta: &JobMeta) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(|error| error.to_string())?;
    atomic_write(&job_path(workspace, &meta.id), &bytes)
}

pub(crate) fn jobs_command(command: JobCommand) -> Result<(), String> {
    let workspace = current_workspace()?;
    fs::create_dir_all(jobs_dir(&workspace)).map_err(|error| error.to_string())?;
    match command {
        JobCommand::Start { argv } => {
            let entries = fs::read_dir(jobs_dir(&workspace))
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count();
            if entries >= 32 {
                return Err("job limit reached (32)".into());
            }
            let id = job_id();
            let meta = JobMeta {
                id: id.clone(),
                workspace: workspace.display().to_string(),
                executable: argv[0].clone(),
                argv_hash: argv_digest(&argv),
                pid: None,
                pgid: None,
                start_ticks: None,
                status: "starting".into(),
                exit_code: None,
                stdout: format!("{id}.stdout"),
                stderr: format!("{id}.stderr"),
            };
            write_job(&workspace, &meta)?;
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            let mut supervisor = OsCommand::new(executable);
            supervisor.args([
                "__job-supervisor",
                "--workspace",
                &workspace.display().to_string(),
                "--job-id",
                &id,
                "--",
            ]);
            supervisor.args(argv);
            supervisor
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            supervisor.spawn().map_err(|error| error.to_string())?;
            println!("{id}");
            Ok(())
        }
        JobCommand::List => {
            let mut ids = Vec::new();
            for entry in fs::read_dir(jobs_dir(&workspace)).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    if let Some(id) = path.file_stem().and_then(|value| value.to_str()) {
                        ids.push(id.to_owned());
                    }
                }
            }
            ids.sort();
            for id in ids {
                let meta = read_job(&workspace, &id)?;
                println!("{}\t{}\t{}", meta.id, meta.status, meta.executable);
            }
            Ok(())
        }
        JobCommand::Status(id) => {
            let meta = read_job(&workspace, &id)?;
            println!(
                "{}",
                serde_json::to_string(&meta).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        JobCommand::Logs { id, stderr, tail } => {
            let meta = read_job(&workspace, &id)?;
            let path = jobs_dir(&workspace).join(if stderr { meta.stderr } else { meta.stdout });
            let text = fs::read_to_string(path).unwrap_or_default();
            let lines = text.lines().collect::<Vec<_>>();
            println!(
                "{}",
                lines
                    .into_iter()
                    .rev()
                    .take(tail)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            Ok(())
        }
        JobCommand::Cancel(id) => cancel_job(&workspace, &id),
        JobCommand::Wait { id, timeout } => wait_job(&workspace, &id, timeout),
        JobCommand::Reap { id } => {
            let ids = id.into_iter().collect::<Vec<_>>();
            let candidates = if ids.is_empty() {
                fs::read_dir(jobs_dir(&workspace))
                    .map_err(|error| error.to_string())?
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry
                            .path()
                            .file_stem()
                            .and_then(|value| value.to_str())
                            .map(str::to_owned)
                    })
                    .collect()
            } else {
                ids
            };
            for id in candidates {
                let meta = read_job(&workspace, &id)?;
                if matches!(meta.status.as_str(), "completed" | "failed" | "cancelled") {
                    let _ = fs::remove_file(job_path(&workspace, &id));
                    let _ = fs::remove_file(jobs_dir(&workspace).join(meta.stdout));
                    let _ = fs::remove_file(jobs_dir(&workspace).join(meta.stderr));
                    println!("{id}");
                }
            }
            Ok(())
        }
    }
}

pub(crate) fn start_background_task(
    workspace: &Path,
    objective: String,
) -> Result<TaskMeta, String> {
    start_agent_task(workspace, objective, "background", None, None, None)
}

pub(crate) fn start_agent_task(
    workspace: &Path,
    objective: String,
    kind: &str,
    scope: Option<String>,
    max_iterations: Option<u32>,
    max_tokens: Option<u64>,
) -> Result<TaskMeta, String> {
    let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
    let directory = jobs_dir(&workspace);
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let active = fs::read_dir(&directory)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count();
    if active >= 32 {
        return Err("background task limit reached (32)".into());
    }
    let id = job_id();
    let executable = env::current_exe().map_err(|e| e.to_string())?;
    let job = JobMeta {
        id: id.clone(),
        workspace: workspace.display().to_string(),
        executable: "subagents".into(),
        argv_hash: argv_digest(&["subagents".into(), objective.clone()]),
        pid: None,
        pgid: None,
        start_ticks: None,
        status: "starting".into(),
        exit_code: None,
        stdout: format!("{id}.stdout"),
        stderr: format!("{id}.stderr"),
    };
    write_job(&workspace, &job)?;
    let config = AppPaths::discover(&workspace)
        .ok()
        .and_then(|paths| load_user_config(paths.effective_project_config()).ok())
        .unwrap_or_default();
    let provider = config.provider.unwrap_or_else(|| "fake".into());
    let model = config.model.unwrap_or_else(|| "offline-subagent".into());
    let scope_value = scope
        .clone()
        .unwrap_or_else(|| workspace.display().to_string());
    let configured_budgets = AppPaths::discover(&workspace)
        .ok()
        .and_then(|paths| subagent_budgets(paths.effective_project_config()).ok())
        .unwrap_or_default();
    let task = TaskMeta {
        id: id.clone(),
        job_id: id.clone(),
        workspace: workspace.display().to_string(),
        objective: redact_output(&objective).chars().take(4096).collect(),
        kind: kind.into(),
        state: "pending".into(),
        provider: provider.clone(),
        model: model.clone(),
        scope: scope_value.clone(),
        permission: "read-only".into(),
        max_iterations: max_iterations.unwrap_or(configured_budgets.max_iterations),
        max_tokens: max_tokens.unwrap_or(configured_budgets.max_tokens),
        max_tool_calls: configured_budgets.max_tool_calls,
        wall_clock_seconds: configured_budgets.wall_clock.as_secs(),
        result: None,
    };
    write_task(&workspace, &task)?;
    let mut supervisor = OsCommand::new(executable);
    supervisor.args([
        "__job-supervisor",
        "--workspace",
        &workspace.display().to_string(),
        "--job-id",
        &id,
        "--",
    ]);
    let child_executable = env::current_exe().map_err(|e| e.to_string())?;
    let mut child_args = vec![
        child_executable.display().to_string(),
        "subagents".into(),
        "run".into(),
        "--objective".into(),
        objective.clone(),
    ];
    if let Some(scope) = scope {
        child_args.extend(["--scope".into(), scope]);
    }
    if let Some(value) = max_iterations {
        child_args.extend(["--max-iterations".into(), value.to_string()]);
    }
    if let Some(value) = max_tokens {
        child_args.extend(["--max-tokens".into(), value.to_string()]);
    }
    supervisor.args(child_args);
    supervisor
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(error) = supervisor.spawn() {
        let _ = fs::remove_file(task_path(&workspace, &id));
        let _ = fs::remove_file(job_path(&workspace, &id));
        return Err(error.to_string());
    }
    Ok(task)
}

pub(crate) fn task_state_from_job(job: &JobMeta) -> &'static str {
    match job.status.as_str() {
        "completed" => "completed",
        "failed" => "failed",
        "cancelled" | "cancelling" => "cancelled",
        _ => "running",
    }
}

pub(crate) fn ensure_task_workspace(task: &TaskMeta, workspace: &Path) -> Result<(), String> {
    if task.workspace == workspace.display().to_string() {
        Ok(())
    } else {
        Err("task belongs to another workspace".into())
    }
}

pub(crate) fn tasks_command(command: TaskCommand, kind_filter: Option<&str>) -> Result<(), String> {
    let workspace = current_workspace()?;
    fs::create_dir_all(jobs_dir(&workspace)).map_err(|e| e.to_string())?;
    match command {
        TaskCommand::Background(objective) => {
            let task = start_background_task(&workspace, objective)?;
            println!("{}", task.id);
        }
        TaskCommand::Run {
            objective,
            scope,
            max_iterations,
            max_tokens,
        } => {
            let task = start_agent_task(
                &workspace,
                objective,
                "subagent",
                scope,
                max_iterations,
                max_tokens,
            )?;
            println!("{}", task.id);
        }
        TaskCommand::List => {
            let mut tasks = Vec::new();
            for entry in fs::read_dir(jobs_dir(&workspace))
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
            {
                if entry.path().extension().is_some_and(|ext| ext == "json")
                    && entry
                        .path()
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with(".task.json"))
                {
                    if let Some(id) = entry
                        .path()
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_suffix(".task.json"))
                    {
                        tasks.push(id.to_owned());
                    }
                }
            }
            tasks.sort();
            for id in tasks {
                let mut task = read_task(&workspace, &id)?;
                if task.workspace != workspace.display().to_string()
                    || kind_filter.is_some_and(|kind| task.kind != kind)
                {
                    continue;
                }
                if let Ok(job) = read_job(&workspace, &task.job_id) {
                    task.state = task_state_from_job(&job).into();
                }
                println!("{}\t{}\t{}", task.id, task.state, task.objective);
            }
        }
        TaskCommand::Show(id) | TaskCommand::Status(id) => {
            let mut task = read_task(&workspace, &id)?;
            ensure_task_workspace(&task, &workspace)?;
            if kind_filter.is_some_and(|kind| task.kind != kind) {
                return Err("task is not a subagent".into());
            }
            if let Ok(job) = read_job(&workspace, &task.job_id) {
                task.state = task_state_from_job(&job).into();
            }
            println!(
                "{}",
                serde_json::to_string(&task).map_err(|e| e.to_string())?
            );
        }
        TaskCommand::Cancel(id) => {
            let mut task = read_task(&workspace, &id)?;
            ensure_task_workspace(&task, &workspace)?;
            if kind_filter.is_some_and(|kind| task.kind != kind) {
                return Err("task is not a subagent".into());
            }
            cancel_job(&workspace, &task.job_id)?;
            task.state = "cancelling".into();
            write_task(&workspace, &task)?;
            println!("{id}");
        }
        TaskCommand::Adopt(id) => {
            let mut task = read_task(&workspace, &id)?;
            ensure_task_workspace(&task, &workspace)?;
            if kind_filter.is_some_and(|kind| task.kind != kind) {
                return Err("task is not a subagent".into());
            }
            let job = read_job(&workspace, &task.job_id)?;
            if !matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
                return Err("task is still running".into());
            }
            let output_path = jobs_dir(&workspace).join(&job.stdout);
            let output = fs::read_to_string(output_path).unwrap_or_default();
            task.state = task_state_from_job(&job).into();
            task.result = Some(redact_output(&output).chars().take(16 * 1024).collect());
            write_task(&workspace, &task)?;
            println!("{}", task.result.as_deref().unwrap_or_default());
        }
    }
    Ok(())
}

pub(crate) fn proc_start_ticks(pid: u32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = content
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    fields.get(19)?.parse().ok()
}

pub(crate) fn cancel_job(workspace: &Path, id: &str) -> Result<(), String> {
    let mut meta = read_job(workspace, id)?;
    let (Some(pid), Some(pgid), Some(start)) = (meta.pid, meta.pgid, meta.start_ticks) else {
        return Err("job has not started".into());
    };
    if proc_start_ticks(pid) != Some(start) {
        return Err("job PID identity mismatch; refusing cancel".into());
    }
    #[cfg(unix)]
    unsafe {
        if libc::kill(-pgid, libc::SIGTERM) != 0 {
            return Err("failed to cancel job process group".into());
        }
    }
    #[cfg(not(unix))]
    {
        return Err("job cancellation requires Unix".into());
    }
    meta.status = "cancelling".into();
    write_job(workspace, &meta)
}

pub(crate) fn wait_job(workspace: &Path, id: &str, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let meta = read_job(workspace, id)?;
        if matches!(meta.status.as_str(), "completed" | "failed" | "cancelled") {
            println!("{}\t{}", meta.id, meta.status);
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("job wait timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn run_job_supervisor(args: &[String]) -> Result<(), String> {
    let workspace = value_after(args, "--workspace")?
        .map(PathBuf::from)
        .ok_or("supervisor workspace missing")?;
    let id = value_after(args, "--job-id")?.ok_or("supervisor job id missing")?;
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or("supervisor argv missing")?;
    let argv = &args[separator + 1..];
    if argv.is_empty() || !safe_job_id(&id) {
        return Err("invalid supervisor argv".into());
    }
    let mut meta = read_job(&workspace, &id)?;
    let stdout = fs::File::create(jobs_dir(&workspace).join(&meta.stdout))
        .map_err(|error| error.to_string())?;
    let stderr = fs::File::create(jobs_dir(&workspace).join(&meta.stderr))
        .map_err(|error| error.to_string())?;
    let mut child = OsCommand::new(&argv[0]);
    child
        .args(&argv[1..])
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", env::var("PATH").unwrap_or_default())
        .stdout(stdout)
        .stderr(stderr)
        .stdin(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        child.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.spawn().map_err(|error| error.to_string())?;
    let pid = child.id();
    meta.pid = Some(pid);
    meta.pgid = Some(pid as i32);
    meta.start_ticks = proc_start_ticks(pid);
    meta.status = "running".into();
    write_job(&workspace, &meta)?;
    let status = child.wait().map_err(|error| error.to_string())?;
    meta.exit_code = status.code();
    meta.status = if status.success() {
        "completed"
    } else if status.code() == Some(143) {
        "cancelled"
    } else {
        "failed"
    }
    .into();
    write_job(&workspace, &meta)
}
