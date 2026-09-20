use serde::{Deserialize, Serialize};
use std::{path::Path, process::Command, time::Instant};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PerfReport {
    pub schema_version: u32,
    pub runs: usize,
    pub samples_ms: Vec<u128>,
    pub p50_ms: u128,
    pub p95_ms: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum PerfError {
    #[error("invalid run count")]
    InvalidRuns,
    #[error("performance command failed: {0}")]
    Command(String),
    #[error("performance JSON failed: {0}")]
    Json(#[from] serde_json::Error),
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
            return Err(PerfError::Command("agent-cli --help failed".into()));
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
