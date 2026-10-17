//! Operator commands: eval, perf, self-update, trust, registry, verify, diff, doctor and shell completion.

use super::*;

pub(crate) fn eval_command(command: EvalCommand) -> Result<(), String> {
    let workspace = current_workspace()?;
    match command {
        EvalCommand::Run {
            suite,
            report,
            baseline,
            max_regressions,
        } => {
            let suite = embedded_suite(&suite)?;
            let fixture_root =
                std::env::temp_dir().join(format!("helios-eval-fixtures-{}", std::process::id()));
            let work_root =
                std::env::temp_dir().join(format!("helios-eval-work-{}", std::process::id()));
            write_embedded_fixtures(&fixture_root)?;
            let result = agent_evals::run_suite(&suite, &fixture_root, &work_root)
                .map_err(|error| error.to_string());
            let _ = fs::remove_dir_all(&fixture_root);
            let _ = fs::remove_dir_all(&work_root);
            let report_data = result?;
            let path = report_path(&workspace, "eval", report)?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&report_data).map_err(|error| error.to_string())?,
            )?;
            println!(
                "suite={} passed={} failed={} report={}",
                report_data.suite,
                report_data.passed,
                report_data.failed,
                path.display()
            );
            if let Some(baseline) = baseline {
                let old: agent_evals::EvalReport =
                    serde_json::from_slice(&fs::read(baseline).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                agent_evals::check_baseline(&report_data, &old, max_regressions)
                    .map_err(|error| error.to_string())?;
            }
            if report_data.failed > 0 {
                return Err(format!("eval failed: {} task(s)", report_data.failed));
            }
            Ok(())
        }
        EvalCommand::Show(path) => {
            let report: agent_evals::EvalReport =
                serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        EvalCommand::Baseline { input, output } => {
            let report: agent_evals::EvalReport =
                serde_json::from_slice(&fs::read(input).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            let path = report_path(&workspace, "baseline", Some(output))?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
            )?;
            println!("{}", path.display());
            Ok(())
        }
    }
}

pub(crate) fn perf_command(command: PerfCommand) -> Result<(), String> {
    let workspace = current_workspace()?;
    match command {
        PerfCommand::Run {
            runs,
            suite,
            fixtures,
            output,
            baseline,
            max_cold_start_ms,
            max_peak_rss_bytes,
            max_regressions,
        } => {
            let binary = env::current_exe().map_err(|error| error.to_string())?;
            let report = agent_perf::run_full_report(
                &binary,
                runs,
                suite.as_deref(),
                fixtures.as_deref(),
                max_cold_start_ms,
                max_peak_rss_bytes,
            )
            .map_err(|error| error.to_string())?;
            let path = report_path(&workspace, "perf", output)?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
            )?;
            if let Some(baseline) = baseline {
                let old: agent_perf::FullPerfReport =
                    serde_json::from_slice(&fs::read(baseline).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                let regressions = usize::from(report.startup.p95_ms > old.startup.p95_ms)
                    + usize::from(
                        report
                            .short_session
                            .peak_rss_bytes
                            .zip(old.short_session.peak_rss_bytes)
                            .is_some_and(|(now, previous)| now > previous),
                    );
                if regressions > max_regressions {
                    return Err(format!(
                        "{regressions} performance regressions exceed {max_regressions}"
                    ));
                }
            }
            println!(
                "runs={} p50_ms={} p95_ms={} peak_rss_bytes={:?} benchmark_tasks={:?} report={}",
                report.startup.runs,
                report.startup.p50_ms,
                report.startup.p95_ms,
                report.short_session.peak_rss_bytes,
                report
                    .benchmark
                    .as_ref()
                    .map(|benchmark| benchmark.task_count),
                path.display()
            );
            Ok(())
        }
        PerfCommand::Show(path) => {
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        PerfCommand::Compare { current, baseline } => {
            let current: serde_json::Value =
                serde_json::from_slice(&fs::read(current).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            let baseline: serde_json::Value =
                serde_json::from_slice(&fs::read(baseline).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            let current_p95 = current
                .pointer("/startup/p95_ms")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| current.get("p95_ms").and_then(serde_json::Value::as_u64))
                .unwrap_or(0);
            let baseline_p95 = baseline
                .pointer("/startup/p95_ms")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| baseline.get("p95_ms").and_then(serde_json::Value::as_u64))
                .unwrap_or(0);
            if current_p95 > baseline_p95 {
                return Err(format!(
                    "p95 {current_p95}ms exceeds baseline {baseline_p95}ms"
                ));
            }
            println!("performance baseline passed");
            Ok(())
        }
    }
}

pub(crate) fn safe_install_prefix(prefix: &Path) -> Result<PathBuf, String> {
    let prefix = expand_user_path(prefix.to_path_buf())?
        .canonicalize()
        .unwrap_or_else(|_| prefix.to_path_buf());
    if prefix.parent().is_none() || prefix == Path::new("/") {
        return Err("refusing unsafe install prefix".into());
    }
    let text = prefix.to_string_lossy();
    if ["/usr", "/bin", "/sbin", "/etc", "/var"]
        .iter()
        .any(|item| text == *item)
    {
        return Err("refusing system install prefix; use a user-owned prefix".into());
    }
    Ok(prefix)
}

pub(crate) fn install_one(source: &Path, target: &Path) -> Result<String, String> {
    let digest = sha256_file(source)?;
    let parent = target.parent().ok_or("invalid install target")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    fs::copy(source, &temp).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    fs::rename(&temp, target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })?;
    atomic_write(&target.with_extension("install.sha256"), digest.as_bytes())?;
    Ok(digest)
}

pub(crate) fn self_command(command: SelfCommand) -> Result<(), String> {
    match command {
        SelfCommand::Version => {
            println!("helios {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        SelfCommand::Install {
            prefix,
            name,
            compat,
        } => {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err("invalid installation name".into());
            }
            let prefix = safe_install_prefix(&prefix)?;
            let source = env::current_exe()
                .map_err(|error| error.to_string())?
                .canonicalize()
                .map_err(|error| error.to_string())?;
            let target = prefix.join("bin").join(&name);
            let digest = install_one(&source, &target)?;
            if compat && name != "agent-cli" {
                install_one(&source, &prefix.join("bin/agent-cli"))?;
            }
            println!("installed {} ({})", target.display(), digest);
            if !env::var_os("PATH").is_some_and(|path| {
                env::split_paths(&path).any(|entry| entry == prefix.join("bin"))
            }) {
                println!("add {} to PATH", prefix.join("bin").display());
            }
            Ok(())
        }
        SelfCommand::Uninstall { name, confirm: _ } => {
            let home = env::var_os("HOME").ok_or("HOME is not set")?;
            let mut candidates = env::var_os("PATH")
                .map(|path| {
                    env::split_paths(&path)
                        .map(|dir| dir.join(&name))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            candidates.push(PathBuf::from(home.clone()).join(".local/bin").join(&name));
            let target = candidates
                .into_iter()
                .find(|path| path.is_file())
                .ok_or("installed binary not found")?;
            let marker = target.with_extension("install.sha256");
            let expected =
                fs::read_to_string(&marker).map_err(|_| "refusing to remove untracked binary")?;
            if sha256_file(&target)? != expected.trim() {
                return Err("installed binary identity does not match its marker".into());
            }
            let trash = PathBuf::from(home).join(".local/share/helios/trash");
            fs::create_dir_all(&trash).map_err(|error| error.to_string())?;
            let stamp = job_id();
            fs::rename(&target, trash.join(format!("{name}-{stamp}")))
                .map_err(|error| error.to_string())?;
            let _ = fs::rename(
                &marker,
                trash.join(format!("{name}-{stamp}.install.sha256")),
            );
            println!("moved {} to {}", target.display(), trash.display());
            Ok(())
        }
        SelfCommand::Update {
            source: Some(source),
            sha256: Some(expected),
        } => {
            if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("--sha256 must be a 64-character hexadecimal digest".into());
            }
            let source = source.canonicalize().map_err(|error| error.to_string())?;
            let actual = sha256_file(&source)?;
            if !actual.eq_ignore_ascii_case(&expected) {
                return Err("local update checksum mismatch; no mutation performed".into());
            }
            let target = env::current_exe().map_err(|error| error.to_string())?;
            let digest = install_one(&source, &target)?;
            println!("updated {} ({})", target.display(), digest);
            Ok(())
        }
        SelfCommand::Update {
            source: None,
            sha256: None,
        } => remote_self_update(),
        SelfCommand::Update { .. } => {
            Err("self update requires both --from PATH and --sha256 HEX".into())
        }
    }
}

pub(crate) fn remote_self_update() -> Result<(), String> {
    #[derive(Deserialize)]
    struct Release {
        assets: Vec<ReleaseAsset>,
    }
    #[derive(Deserialize)]
    struct ReleaseAsset {
        name: String,
        browser_download_url: String,
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent("helios-agent-self-update")
        .build()
        .map_err(|error| error.to_string())?;
    let mut request =
        client.get("https://api.github.com/repos/Shidori5417/helios-agent/releases/latest");
    if let Some(token) = env::var_os("GITHUB_TOKEN") {
        request = request.bearer_auth(token.to_string_lossy());
    }
    let response = request.send().map_err(|error| error.to_string())?;
    if response.status().as_u16() == 404 {
        return Err("no release available".into());
    }
    if !response.status().is_success() {
        return Err(format!(
            "release lookup failed with HTTP {}",
            response.status()
        ));
    }
    let release: Release = response.json().map_err(|error| error.to_string())?;
    let artifact = release
        .assets
        .iter()
        .find(|asset| {
            asset.name.contains("x86_64")
                && asset.name.contains("linux")
                && !asset.name.ends_with(".bundle")
        })
        .ok_or("release has no x86_64 Linux artifact")?;
    let checksums = release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .ok_or("release has no SHA256SUMS")?;
    let bundle = release
        .assets
        .iter()
        .find(|asset| asset.name.ends_with(".bundle"))
        .ok_or("release has no cosign bundle")?;
    let download = |url: &str| -> Result<Vec<u8>, String> {
        let mut request = client.get(url);
        if let Some(token) = env::var_os("GITHUB_TOKEN") {
            request = request.bearer_auth(token.to_string_lossy());
        }
        let response = request.send().map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "asset download failed with HTTP {}",
                response.status()
            ));
        }
        let bytes = response.bytes().map_err(|error| error.to_string())?;
        if bytes.len() > 100 * 1024 * 1024 {
            return Err("release asset exceeds 100 MiB bound".into());
        }
        Ok(bytes.to_vec())
    };
    let artifact_bytes = download(&artifact.browser_download_url)?;
    let checksum_text = String::from_utf8(download(&checksums.browser_download_url)?)
        .map_err(|_| "invalid checksum file".to_string())?;
    let expected = checksum_text
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == artifact.name).then(|| digest.to_string())
        })
        .ok_or("artifact is absent from SHA256SUMS")?;
    if !format!("{:x}", Sha256::digest(&artifact_bytes)).eq_ignore_ascii_case(&expected) {
        return Err("release checksum mismatch; no mutation performed".into());
    }
    let root = env::temp_dir().join(format!("helios-update-{}", job_id()));
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let artifact_path = root.join(&artifact.name);
    let bundle_path = root.join(&bundle.name);
    fs::write(&artifact_path, &artifact_bytes).map_err(|error| error.to_string())?;
    fs::write(&bundle_path, download(&bundle.browser_download_url)?)
        .map_err(|error| error.to_string())?;
    let verified = OsCommand::new("cosign").args(["verify-blob", "--bundle"]).arg(&bundle_path).args(["--certificate-identity", "https://github.com/Shidori5417/helios-agent/.github/workflows/release.yml@refs/heads/main", "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com"]).arg(&artifact_path).stdout(Stdio::null()).stderr(Stdio::null()).status();
    if !verified.is_ok_and(|status| status.success()) {
        let _ = fs::remove_dir_all(&root);
        return Err(
            "cosign verification failed or cosign is unavailable; no mutation performed".into(),
        );
    }
    let target = env::current_exe().map_err(|error| error.to_string())?;
    let result = install_one(&artifact_path, &target);
    let _ = fs::remove_dir_all(&root);
    result.map(|digest| println!("updated {} ({})", target.display(), digest))
}

pub(crate) fn read_trust_store_or_empty(path: &Path) -> Result<TrustStore, String> {
    if !path.exists() {
        return Ok(TrustStore {
            keys: Default::default(),
            revoked: Default::default(),
            disabled: Default::default(),
        });
    }
    load_trust_store(path).map_err(|error| error.to_string())
}

pub(crate) fn write_trust_store(path: &Path, store: &TrustStore) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(store).map_err(|error| error.to_string())?;
    atomic_write(path, &bytes)
}

pub(crate) fn trust_command(command: TrustCommand) -> Result<(), String> {
    match command {
        TrustCommand::List { path } => {
            let store = read_trust_store_or_empty(&path)?;
            for id in store.keys.keys() {
                let state = if store.revoked.contains(id) {
                    "revoked"
                } else if store.disabled.contains(id) {
                    "disabled"
                } else {
                    "enabled"
                };
                println!("{id}\t{state}");
            }
            Ok(())
        }
        TrustCommand::Show { path, id } => {
            let store = read_trust_store_or_empty(&path)?;
            let key = store.keys.get(&id).ok_or("trust key not found")?;
            let state = if store.revoked.contains(&id) {
                "revoked"
            } else if store.disabled.contains(&id) {
                "disabled"
            } else {
                "enabled"
            };
            println!("id={id} state={state} public_key={key}");
            Ok(())
        }
        TrustCommand::Add { path, id, key_file } => {
            if !valid_key_id(&id) {
                return Err("invalid trust key id".into());
            }
            let key = read_public_key_file(&key_file)?;
            let mut store = read_trust_store_or_empty(&path)?;
            if store.keys.contains_key(&id) || store.revoked.contains(&id) {
                return Err("trust key id already exists or is revoked".into());
            }
            store.keys.insert(id, key);
            write_trust_store(&path, &store)
        }
        TrustCommand::Disable { path, id } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.disabled.insert(id);
            write_trust_store(&path, &store)
        }
        TrustCommand::Enable { path, id } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if store.revoked.contains(&id) {
                return Err("revoked trust keys cannot be re-enabled".into());
            }
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.disabled.remove(&id);
            write_trust_store(&path, &store)
        }
        TrustCommand::Revoke { path, id } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.disabled.remove(&id);
            store.revoked.insert(id);
            write_trust_store(&path, &store)
        }
        TrustCommand::Remove {
            path,
            id,
            confirm: _,
        } => {
            let mut store = read_trust_store_or_empty(&path)?;
            if !store.keys.contains_key(&id) {
                return Err("trust key not found".into());
            }
            store.keys.remove(&id);
            store.disabled.remove(&id);
            store.revoked.remove(&id);
            write_trust_store(&path, &store)
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct RegistryStateFile {
    #[serde(default)]
    pub(crate) registries: std::collections::BTreeMap<String, RegistryStateValue>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RegistryStateValue {
    pub(crate) sequence: u64,
    pub(crate) digest: String,
}

pub(crate) fn registry_command(command: RegistryCommand) -> Result<(), String> {
    match command {
        RegistryCommand::Status { path, id } => {
            let state: RegistryStateFile = if path.exists() {
                serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?
            } else {
                RegistryStateFile::default()
            };
            for (name, value) in state.registries {
                if id.as_deref().is_none_or(|wanted| wanted == name) {
                    println!(
                        "{name}\tsequence={}\tdigest={}",
                        value.sequence, value.digest
                    );
                }
            }
            Ok(())
        }
        RegistryCommand::Reset {
            path,
            id,
            confirm: _,
        } => {
            let mut state: RegistryStateFile =
                serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            if state.registries.remove(&id).is_none() {
                return Err("registry id not found".into());
            }
            let home = env::var_os("HOME").ok_or("HOME is not set")?;
            let trash = PathBuf::from(home).join(".local/share/helios/trash");
            fs::create_dir_all(&trash).map_err(|error| error.to_string())?;
            fs::rename(&path, trash.join(format!("registry-state-{}", job_id())))
                .map_err(|error| error.to_string())?;
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?,
            )
        }
        RegistryCommand::Verify { source, trust } => {
            let store = load_trust_store(&trust).map_err(|error| error.to_string())?;
            let verified =
                verify_signed_registry(source, &store).map_err(|error| error.to_string())?;
            println!(
                "verified registry={} signer={} sequence={} expires_at={}",
                verified.registry_id, verified.key_id, verified.sequence, verified.expires_at
            );
            Ok(())
        }
    }
}

pub(crate) fn verify_command(argv: Vec<String>) -> Result<(), String> {
    let workspace = workspace_root()?;
    let registry = ToolRegistry::new(workspace, PermissionMode::WorkspaceWrite)
        .map_err(|error| error.to_string())?;
    let result = registry
        .verify_argv(&argv)
        .map_err(|error| error.to_string())?;
    print!("{}", result.output);
    if result.status == ToolStatus::Ok {
        Ok(())
    } else {
        Err(format!("verification status: {:?}", result.status))
    }
}

pub(crate) fn diff_command() -> Result<(), String> {
    let workspace = workspace_root()?;
    let registry = ToolRegistry::new(workspace, PermissionMode::ReadOnly)
        .map_err(|error| error.to_string())?;
    let result = registry.git_diff().map_err(|error| error.to_string())?;
    print!("{}", result.data);
    Ok(())
}

pub(crate) fn doctor_command(json: bool) -> Result<(), String> {
    let workspace = workspace_root()?;
    let paths = app_paths()?;
    let config = config_path()?;
    let config_valid = load_user_config(&config).is_ok();
    let db_present = paths.db_path.is_file();
    let db_integrity = db_present
        && Database::open(&paths.db_path)
            .and_then(|db| {
                db.connection()
                    .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            })
            .map(|value| value == "ok")
            .unwrap_or(false);
    let keyring_available = KeyringSecretStore.get("openai").is_ok();
    let git_available = command_available("git");
    let rg_available = command_available("rg");
    let terminal = env::var("TERM").unwrap_or_else(|_| "unknown".into());
    let provider_credentials = ["openai", "anthropic", "gemini", "generic"]
        .iter()
        .map(|provider| serde_json::json!({"provider": provider, "env_present": env::var_os(provider_env(provider)).is_some(), "keyring_present": KeyringSecretStore.get(provider).is_ok()}))
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "binary": "helios", "version": env!("CARGO_PKG_VERSION"), "workspace": workspace,
        "config": config, "config_valid": config_valid, "db_present": db_present,
        "db_integrity": db_integrity, "keyring_available": keyring_available,
        "git_available": git_available, "rg_available": rg_available, "terminal": terminal,
        "provider_credentials": provider_credentials,
        "mcp_configured": config_value_count(&config, "mcp_server"), "lsp_configured": config_value_count(&config, "lsp_server"),
        "plugins_dir_present": paths.plugins_dir.is_dir(), "skills_dir_present": workspace.join("skills").is_dir(),
        "permission_mode": load_runtime_policy(&config).map(|policy| format!("{:?}", policy.mode)).unwrap_or_else(|_| "invalid".into()),
        "os_sandbox": os_sandbox_status(),
    });
    if json {
        println!("{payload}");
    } else {
        println!("binary=helios version={}", env!("CARGO_PKG_VERSION"));
        for (key, value) in payload
            .as_object()
            .into_iter()
            .flat_map(|object| object.iter())
        {
            if !matches!(key.as_str(), "provider_credentials") {
                println!("{key}={value}");
            }
        }
    }
    Ok(())
}

pub(crate) fn completion_command(command: CompletionCommand) -> Result<(), String> {
    let (shell, source) = match &command {
        CompletionCommand::Generate { shell } | CompletionCommand::Install { shell, .. } => {
            (shell.clone(), completion_source(shell)?)
        }
    };
    match command {
        CompletionCommand::Generate { .. } => print!("{source}"),
        CompletionCommand::Install { path, .. } => {
            let target = path.unwrap_or_else(|| match shell.as_str() {
                "bash" => PathBuf::from("~/.local/share/bash-completion/completions/helios"),
                "zsh" => PathBuf::from("~/.zfunc/_helios"),
                _ => PathBuf::from("~/.config/fish/completions/helios.fish"),
            });
            if target.to_string_lossy().starts_with("~") {
                return Err("completion install requires an explicit absolute --path".into());
            }
            atomic_write(&target, source.as_bytes())?;
            println!("{}", target.display());
        }
    }
    Ok(())
}

pub(crate) fn completion_source(shell: &str) -> Result<String, String> {
    let source = match shell {
        "bash" => include_str!("../../../completions/agent-cli.bash"),
        "zsh" => include_str!("../../../completions/agent-cli.zsh"),
        "fish" => include_str!("../../../completions/agent-cli.fish"),
        other => return Err(format!("unsupported completion shell: {other}")),
    };
    for definition in command_registry() {
        if !source.contains(definition.name) {
            return Err(format!(
                "completion source is missing command {}",
                definition.name
            ));
        }
    }
    Ok(source.into())
}
