use agent_evals::{check_baseline, load_suite, run_suite, write_report, EvalReport};
use std::{env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut suite_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/tasks.json");
    let mut fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/workspaces");
    let mut report_path = None;
    let mut baseline_path = None;
    let mut max_regressions = 0_usize;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--suite" => suite_path = PathBuf::from(args.next().unwrap_or_default()),
            "--fixtures" => fixture_root = PathBuf::from(args.next().unwrap_or_default()),
            "--report" => report_path = args.next().map(PathBuf::from),
            "--baseline" => baseline_path = args.next().map(PathBuf::from),
            "--max-regressions" => {
                max_regressions = args.next().unwrap_or_default().parse().unwrap_or(0)
            }
            "--help" => {
                println!("agent-eval [--suite PATH] [--fixtures PATH] [--report PATH] [--baseline PATH] [--max-regressions N]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let result = load_suite(&suite_path).and_then(|suite| {
        run_suite(
            &suite,
            &fixture_root,
            env::temp_dir().join("agent-eval-work"),
        )
        .and_then(|report| {
            println!(
                "{}/{} tasks passed; p50={}ms p95={}ms regressions={}",
                report.passed,
                report.task_count,
                report.summary.p50_wall_time_ms,
                report.summary.p95_wall_time_ms,
                report.summary.regression_count
            );
            if let Some(path) = baseline_path {
                let baseline: EvalReport = serde_json::from_str(
                    &std::fs::read_to_string(path).map_err(agent_evals::EvalError::Io)?,
                )?;
                check_baseline(&report, &baseline, max_regressions)?;
            }
            if let Some(path) = report_path {
                write_report(&report, path)?;
            }
            if report.failed == 0 {
                Ok(())
            } else {
                Err(agent_evals::EvalError::Failed(format!(
                    "{} task(s) failed",
                    report.failed
                )))
            }
        })
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("eval failed: {error}");
            ExitCode::from(1)
        }
    }
}
