//! Golden corpus for the `helios` argument surface.
//!
//! Each argv is run against the real binary in an isolated root (its own
//! workspace, HOME and no API keys) and the exit code, stdout and stderr are
//! compared with `tests/golden/argv.txt`. This pins the current parser so a
//! later parser change shows up as a reviewable diff instead of a silent shift.
//!
//! Regenerate deliberately with `HELIOS_UPDATE_GOLDEN=1 cargo test -p agent-cli --test argv_golden`.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const GOLDEN: &str = "tests/golden/argv.txt";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Argument vectors. `{WS}` is replaced with the case's workspace directory.
const CORPUS: &[&[&str]] = &[
    &[],
    &["--version"],
    &["-V"],
    &["--help"],
    &["-h"],
    &["help"],
    &["help", "run"],
    &["run", "--help"],
    &["r", "--help"],
    &["exec", "-h"],
    &["--model", "x", "--help"],
    &["models", "--json"],
    &["doctor", "--json"],
    &["init", "--force"],
    &["config", "path"],
    &["config", "get", "--key", "x"],
    &["auth", "status", "openai"],
    &["completion", "generate", "bash"],
    &["completion", "generate", "zsh"],
    &["completion", "generate", "fish"],
    &["completion", "install", "zsh", "--path", "out.zsh"],
    &["search", "--query", "q"],
    &["search", "--query", "--db-path"],
    &["search"],
    &["--workspace", "{WS}", "config", "path"],
    &["config", "path", "--workspace", "{WS}"],
    &["--workspace"],
    &["--workspace", "-x"],
    &["--workspace", "/nonexistent-helios-dir", "config", "path"],
    &["hello", "world"],
    &["frobnicate"],
    &["not-a-command", "--help"],
    &["exec", "--", "prompt"],
    &["exec", "-prompt"],
    &["exec", "-5", "degrees"],
    &["exec", "run", "tests"],
    &["run"],
    &["exec"],
    &["exec", ""],
    &["exec", "--output-format", "json", "p"],
    &["exec", "--output-format", "yaml", "p"],
    &["run", "--output-format", "json", "p"],
    &["exec", "--tui", "p"],
    &["--tui", "exec", "p"],
    &[
        "--db-path",
        "{WS}/s.db",
        "exec",
        "--output-format",
        "json",
        "p",
    ],
    &["--image", "a.png", "exec", "p"],
    &["exec", "--image", "a.png", "--image", "b.png", "p"],
    &["--provider", "-x", "exec", "p"],
    &["--effort", "bogus", "exec", "p"],
    &["--effort", "auto", "exec", "p"],
    &["providers", "list"],
    &["mcp", "list"],
    &["status", "--json"],
];

struct Outcome {
    exit: String,
    stdout: String,
    stderr: String,
}

fn run_case(index: usize, argv: &[&str]) -> Outcome {
    let root = std::env::temp_dir().join(format!("helios-golden-{}-{index}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let workspace = root.join("ws");
    let home = root.join("home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&home).expect("home");
    let workspace_text = workspace.to_string_lossy().into_owned();
    let args: Vec<String> = argv
        .iter()
        .map(|arg| arg.replace("{WS}", &workspace_text))
        .collect();

    let mut command = Command::new(env!("CARGO_BIN_EXE_helios"));
    command
        .args(&args)
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn helios");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll helios") {
            break Some(status);
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("collect output");
    let exit = match status {
        Some(status) => status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
        None => "TIMEOUT".to_owned(),
    };
    let outcome = Outcome {
        exit,
        stdout: normalize(&String::from_utf8_lossy(&output.stdout), &root),
        stderr: normalize(&String::from_utf8_lossy(&output.stderr), &root),
    };
    let _ = fs::remove_dir_all(&root);
    outcome
}

/// Replaces machine-specific paths so the snapshot is stable across runs.
fn normalize(text: &str, root: &Path) -> String {
    let root = root.to_string_lossy();
    let text = text.replace(root.as_ref(), "<ROOT>");
    // Event timestamps are wall-clock values and differ on every run.
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(index) = rest.find("\"timestamp\":") {
        let (head, tail) = rest.split_at(index + "\"timestamp\":".len());
        out.push_str(head);
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str("<TS>");
        rest = &tail[digits..];
    }
    out.push_str(rest);

    // OS sandbox availability depends on kernel features (Landlock ABI) and varies across environments.
    let text = out;
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(index) = rest.find("\"os_sandbox\":") {
        let (head, tail) = rest.split_at(index + "\"os_sandbox\":".len());
        out.push_str(head);
        if let Some(after_quote) = tail.strip_prefix('"') {
            if let Some(end_quote) = after_quote.find('"') {
                out.push_str("\"<OS_SANDBOX>\"");
                rest = &after_quote[end_quote + 1..];
                continue;
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn render(corpus: &[&[&str]]) -> String {
    let mut out = String::new();
    for (index, argv) in corpus.iter().enumerate() {
        let outcome = run_case(index, argv);
        let _ = writeln!(out, "== {argv:?}");
        let _ = writeln!(out, "exit: {}", outcome.exit);
        let _ = writeln!(out, "--- stdout");
        out.push_str(&outcome.stdout);
        if !outcome.stdout.is_empty() && !outcome.stdout.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "--- stderr");
        out.push_str(&outcome.stderr);
        if !outcome.stderr.is_empty() && !outcome.stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[test]
fn argv_surface_matches_golden_snapshot() {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    let actual = render(CORPUS);
    if std::env::var_os("HELIOS_UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(golden_path.parent().expect("golden dir")).expect("create dir");
        fs::write(&golden_path, &actual).expect("write golden");
        return;
    }
    let expected = fs::read_to_string(&golden_path).unwrap_or_else(|_| {
        panic!(
            "missing {}; run with HELIOS_UPDATE_GOLDEN=1 to create it",
            golden_path.display()
        )
    });
    if expected != actual {
        let first_diff = expected
            .lines()
            .zip(actual.lines())
            .position(|(left, right)| left != right)
            .map_or(
                expected.lines().count().min(actual.lines().count()),
                |line| line,
            );
        panic!(
            "argv surface changed (first differing line {}). Review, then regenerate with HELIOS_UPDATE_GOLDEN=1 if intended.",
            first_diff + 1
        );
    }
}
