//! Runs a program under the workspace sandbox that the agent applies to its own
//! subprocesses, so the whole test suite can be checked under the same rules.
//!
//! Build first, then run the binary directly:
//! `cargo build -p agent-sandbox --example run_confined`
//! `target/debug/examples/run_confined <root> <program> [args...]`

use std::{os::unix::process::CommandExt, path::PathBuf, process::Command};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .expect("usage: run_confined <root> <program> [args...]"),
    );
    let program = args
        .next()
        .expect("usage: run_confined <root> <program> [args...]");
    let rest: Vec<String> = args.collect();
    let sandbox = agent_sandbox::landlock::workspace_sandbox(&root, &[], false)
        .expect("build ruleset")
        .expect("kernel provides Landlock");
    let mut command = Command::new(program);
    command.args(rest).current_dir(&root);
    // SAFETY: restrict_current performs only prctl and landlock syscalls.
    unsafe {
        command.pre_exec(move || sandbox.restrict_current());
    }
    // exec only returns on failure.
    let error = command.exec();
    eprintln!("exec failed: {error}");
    std::process::exit(127);
}
