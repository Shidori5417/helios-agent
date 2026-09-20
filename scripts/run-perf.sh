#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

binary="${HELIOS_BINARY:-$root_dir/target/release/helios}"
runs="${AGENT_PERF_RUNS:-10}"
report="${AGENT_PERF_REPORT:-/tmp/agent-perf.json}"

if [[ ! -x "$binary" ]]; then
  cargo build --release -p agent-cli --bin helios
fi

cargo run --release -p agent-perf -- \
  --binary "$binary" \
  --runs "$runs" \
  --suite "$root_dir/crates/evals/fixtures/benchmark-100.json" \
  --fixtures "$root_dir/crates/evals/fixtures/workspaces" \
  --output "$report"

printf 'performance report: %s\n' "$report"
