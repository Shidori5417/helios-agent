# Performance evidence

`agent-perf` measures release-binary process wall time and Linux peak RSS without making model or network calls. It also runs the deterministic offline benchmark when a suite is supplied.

Run the bounded manual harness with:

```bash
scripts/run-perf.sh
```

Useful overrides are `AGENT_PERF_RUNS`, `AGENT_CLI_BINARY`, and `AGENT_PERF_REPORT`. The JSON report contains environment/toolchain metadata, all startup samples, p50/p95 startup time, short fake-session wall time and optional peak RSS. Benchmark reports additionally include task-level validation-child peak RSS and aggregate maximum/p95 RSS, plus benchmark p50/p95, token estimates, tool calls, diff bytes and regressions. Unsupported RSS platforms use `null`; they are never reported as zero.

## Reference smoke measurement

The following is a local release measurement, not a portable performance guarantee. It was collected on 2026-09-20 with 10 startup samples and the checked offline benchmark suite:

```json
{
  "command": "scripts/run-perf.sh",
  "runs": 10,
  "rustc": "rustc 1.98.1 (48a229cea 2026-09-01)",
  "startup_ms": {"p50": 1, "p95": 1},
  "short_session_wall_ms": 70,
  "peak_rss_bytes": 7192576,
  "benchmark": {"tasks": 100, "passed": 100, "p50_ms": 3, "p95_ms": 5, "tokens": 1436, "tool_calls": 120, "diff_bytes": 104},
  "network": false,
  "model": false,
  "warm_filesystem_caveat": true,
  "report": "/tmp/agent-perf.json"
}
```

Run the script on the target machine to obtain the numeric values; process scheduling, filesystem cache state and compiler/toolchain change them. Thresholds are opt-in (`--max-cold-start-ms`, `--max-peak-rss-bytes`) and baseline comparison treats a startup p50 increase above 10% or peak RSS increase above 15 MiB as a regression. Normal workspace tests do not enforce timing/RSS thresholds.
