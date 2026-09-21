# Performance evidence

`agent-perf` measures the `helios` release-binary process wall time and Linux peak RSS without making model or network calls. It also runs the deterministic offline benchmark when a suite is supplied.

Run the bounded manual harness with:

```bash
scripts/run-perf.sh
```

Useful overrides are `AGENT_PERF_RUNS`, `HELIOS_BINARY`, and `AGENT_PERF_REPORT`. The JSON report contains environment/toolchain metadata, all startup samples, p50/p95 startup time, short fake-session wall time and optional peak RSS. Benchmark reports additionally include task-level validation-child peak RSS and aggregate maximum/p95 RSS, plus benchmark p50/p95, token estimates, tool calls, diff bytes and regressions. Unsupported RSS platforms use `null`; they are never reported as zero.

## Reference smoke measurement

The following is a local release measurement, not a portable performance guarantee. It was collected on 2026-09-21 with 10 startup and version samples and the checked offline benchmark suite:

```json
{
  "command": "scripts/run-perf.sh",
  "runs": 10,
  "rustc": "rustc 1.98.1 (48a229cea 2026-09-01)",
  "startup_ms": {"help_p50": 1, "help_p95": 2, "version_p50": 2, "version_p95": 2},
  "short_session_wall_ms": 26,
  "peak_rss_bytes": 9437184,
  "binary_size_bytes": 9352578,
  "agent_perf_size_bytes": 673737,
  "benchmark": {"tasks": 100, "passed": 100, "p50_ms": 5, "p95_ms": 5, "tokens": 1436, "tool_calls": 120, "diff_bytes": 104, "peak_rss_bytes": 2469888},
  "advanced": {"pty_first_frame_ms": 56, "idle_rss_bytes": 9859072, "idle_cpu_percent": 0, "active_rss_bytes": 9437184, "palette_filter_us": 1920, "session_10k_list_ms": 0, "session_10k_restore_ms": 0, "context_build_ms": 5, "tool_schema_bytes": 5744, "tool_schema_tokens_estimate": 1436, "large_output_spill_bytes": 10485760, "skill_metadata_bytes": 19, "skill_body_bytes": 27, "mcp_lazy_child_count": 0, "mcp_discovery_ms": 61, "cache_cleanup_deleted_bytes": 5, "cache_cleanup_remaining_bytes": 6},
  "network": false,
  "model": false,
  "warm_filesystem_caveat": true,
  "report": "/tmp/agent-perf.json"
}
```

The measurement host is Linux x86_64 with `rustc 1.98.1 (48a229cea 2026-09-01)`. PTY first-frame uses a real 120x36 PTY and detects the first rendered byte; idle RSS/CPU are sampled during a two-second idle interval. The 10k session measurement seeds 10,000 SQLite sessions and queries only a 50-row page plus the latest-session restore path. The offline fixture also emits 10 MiB through the bounded terminal collector (10 MiB spill, 8 KiB preview), discovers skill metadata before loading its 27-byte body, performs a local stdio MCP handshake/discovery, and removes a 5-byte stale cache reference while preserving a 6-byte active reference. `cargo-bloat` was not installed, so `size(1)` is reported for the release artifacts instead. RSS is Linux `/proc` data and is not a portable memory guarantee; filesystem cache and process scheduling can change all wall-time values.

Run the script on the target machine to obtain the numeric values; process scheduling, filesystem cache state and compiler/toolchain change them. Thresholds are opt-in (`--max-cold-start-ms`, `--max-peak-rss-bytes`) and baseline comparison treats a startup p50 increase above 10% or peak RSS increase above 15 MiB as a regression. Normal workspace tests do not enforce timing/RSS thresholds.
