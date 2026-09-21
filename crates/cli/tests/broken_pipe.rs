#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

fn exits_cleanly_after_stdout_consumer_closes(args: &[&str]) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_helios"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn helios");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).expect("read first line");
    drop(reader);
    let status = child.wait().expect("wait helios");
    assert!(
        status.success(),
        "{args:?} exited with {status}, after writing {first_line:?}"
    );
}

#[test]
fn help_pipe_closure_is_clean() {
    exits_cleanly_after_stdout_consumer_closes(&["--help"]);
}

#[test]
fn sessions_pipe_closure_is_clean() {
    exits_cleanly_after_stdout_consumer_closes(&["sessions", "list"]);
}

#[test]
fn stream_exec_pipe_closure_is_clean() {
    let workspace = std::env::temp_dir().join(format!(
        "helios-broken-pipe-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir(&workspace).expect("create workspace");
    let workspace_arg = workspace.to_string_lossy().into_owned();
    let db_path = workspace.join("sessions.db");
    let db_path_arg = db_path.to_string_lossy().into_owned();
    let args = [
        "--workspace",
        workspace_arg.as_str(),
        "--db-path",
        db_path_arg.as_str(),
        "exec",
        "--provider",
        "fake",
        "--output-format",
        "stream-json",
        "hello",
    ];
    exits_cleanly_after_stdout_consumer_closes(&args);
    let _ = std::fs::remove_dir_all(workspace);
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos()
}
