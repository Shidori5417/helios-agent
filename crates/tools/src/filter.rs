//! Deterministic condensing of noisy build/test output. A model needs the
//! failing lines and the summary, not thousands of "Compiling ..." or passing
//! test lines. No LLM call and no network: the rules are plain text matching,
//! and the full stream always stays in the spill file.

/// Largest output this module will scan. Anything beyond is ignored, so a
/// runaway command cannot make condensing itself expensive.
pub const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Tool families with a known failure/summary shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Cargo,
    Pytest,
    Go,
    Node,
}

/// Recognises a build or test invocation by program basename and arguments.
/// Returns `None` for everything else, which keeps its plain bounded output.
pub fn family_of(program: &str, args: &[String]) -> Option<Family> {
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let first = args.first().map(String::as_str).unwrap_or("");
    match name {
        "cargo" if matches!(first, "test" | "build" | "check" | "clippy" | "nextest") => {
            Some(Family::Cargo)
        }
        "pytest" => Some(Family::Pytest),
        "python" | "python3" if args.iter().any(|arg| arg == "pytest") => Some(Family::Pytest),
        "go" if first == "test" || first == "build" || first == "vet" => Some(Family::Go),
        "npm" | "pnpm" | "yarn" | "bun" if matches!(first, "test" | "run" | "build" | "lint") => {
            Some(Family::Node)
        }
        _ => None,
    }
}

/// Lines worth keeping for a family: failures, locations, and summaries.
fn is_signal(family: Family, line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    let common = [
        "error",
        "Error",
        "ERROR",
        "FAIL",
        "Fail",
        "failed",
        "FAILED",
        "panicked",
        "assert",
        "Assert",
        "thread '",
        "Traceback",
        "exception",
        "Exception",
    ];
    if common.iter().any(|marker| trimmed.contains(marker)) {
        return true;
    }
    match family {
        Family::Cargo => {
            trimmed.starts_with("test result:")
                || trimmed.starts_with("Finished")
                || trimmed.starts_with("-->")
                || trimmed.starts_with("warning:")
                || trimmed.starts_with("note:")
                || trimmed.starts_with("help:")
        }
        Family::Pytest => {
            trimmed.starts_with("=") || trimmed.starts_with("E ") || trimmed.starts_with("FAILED")
        }
        Family::Go => {
            trimmed.starts_with("ok ") || trimmed.starts_with("---") || trimmed.starts_with("FAIL")
        }
        Family::Node => trimmed.starts_with("Tests:") || trimmed.starts_with("Test Suites:"),
    }
}

/// Result of condensing: the kept text plus whether anything was dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condensed {
    pub text: String,
    pub kept_lines: usize,
    pub total_lines: usize,
}

/// Keeps only signal lines from `text` (already read by the caller, at most
/// [`MAX_SCAN_BYTES`]). Consecutive dropped lines become one marker, so the
/// reader can see where the gaps are.
pub fn condense(family: Family, text: &str) -> Condensed {
    let scanned = &text[..floor_char_boundary(text, MAX_SCAN_BYTES)];
    let mut out = String::new();
    let mut total = 0usize;
    let mut kept = 0usize;
    let mut gap = 0usize;
    for line in scanned.lines() {
        total += 1;
        if is_signal(family, line) {
            if gap > 0 {
                out.push_str(&format!("… {gap} line(s) omitted\n"));
                gap = 0;
            }
            out.push_str(line);
            out.push('\n');
            kept += 1;
        } else {
            gap += 1;
        }
    }
    if gap > 0 {
        out.push_str(&format!("… {gap} line(s) omitted\n"));
    }
    Condensed {
        text: out,
        kept_lines: kept,
        total_lines: total,
    }
}

fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn recognises_build_and_test_invocations_only() {
        assert_eq!(
            family_of("cargo", &args(&["test", "-p", "x"])),
            Some(Family::Cargo)
        );
        assert_eq!(
            family_of("/usr/bin/cargo", &args(&["clippy"])),
            Some(Family::Cargo)
        );
        assert_eq!(family_of("cargo", &args(&["fmt"])), None);
        assert_eq!(family_of("pytest", &args(&["-q"])), Some(Family::Pytest));
        assert_eq!(
            family_of("python3", &args(&["-m", "pytest"])),
            Some(Family::Pytest)
        );
        assert_eq!(family_of("python3", &args(&["script.py"])), None);
        assert_eq!(family_of("go", &args(&["test", "./..."])), Some(Family::Go));
        assert_eq!(family_of("npm", &args(&["test"])), Some(Family::Node));
        assert_eq!(family_of("git", &args(&["status"])), None);
    }

    #[test]
    fn cargo_failure_keeps_location_message_and_summary_only() {
        let raw = "   Compiling dep v1\n   Compiling app v0.1\n\
error[E0308]: mismatched types\n  --> src/lib.rs:4:9\n   |\n4  |     1\n   |     ^ expected `u8`\n\
     Compiling other v1\n\
test tests::a ... ok\ntest tests::b ... FAILED\n\
thread 'tests::b' panicked at src/lib.rs:9\n\
test result: FAILED. 1 passed; 1 failed\n";
        let condensed = condense(Family::Cargo, raw);
        assert!(condensed.text.contains("error[E0308]: mismatched types"));
        assert!(condensed.text.contains("--> src/lib.rs:4:9"));
        assert!(condensed.text.contains("FAILED"));
        assert!(condensed.text.contains("test result: FAILED"));
        assert!(!condensed.text.contains("Compiling dep"));
        assert!(!condensed.text.contains("tests::a ... ok"));
        assert!(condensed.text.contains("line(s) omitted"));
        assert!(condensed.kept_lines < condensed.total_lines);
    }

    #[test]
    fn pytest_keeps_assertions_and_summary() {
        let raw = "collected 3 items\ntest_a.py .\ntest_a.py F\n\
E   AssertionError: assert 1 == 2\n=== 1 failed, 2 passed in 0.10s ===\n";
        let condensed = condense(Family::Pytest, raw);
        assert!(condensed.text.contains("E   AssertionError"));
        assert!(condensed.text.contains("1 failed, 2 passed"));
        assert!(!condensed.text.contains("collected 3 items"));
    }

    #[test]
    fn all_passing_output_still_yields_its_summary() {
        let raw = "running 2 tests\ntest a ... ok\ntest b ... ok\ntest result: ok. 2 passed\n";
        let condensed = condense(Family::Cargo, raw);
        assert!(condensed.text.contains("test result: ok. 2 passed"));
    }

    #[test]
    fn condensing_is_bounded_and_char_safe() {
        let raw = "error ".repeat(MAX_SCAN_BYTES / 4);
        let condensed = condense(Family::Cargo, &raw);
        assert!(condensed.text.len() <= MAX_SCAN_BYTES + 64);
        let multibyte = "é".repeat(MAX_SCAN_BYTES);
        let _ = condense(Family::Go, &multibyte);
    }
}
