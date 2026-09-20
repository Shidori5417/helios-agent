# Evals and Benchmarks

## Why this is mandatory

A coding agent can feel impressive while silently consuming twice the tokens or introducing regressions. The harness must be developed against repeatable tasks.

## Offline harness tests

No model required:

- context segment ordering is deterministic;
- identical stable inputs produce identical cache prefix bytes;
- tool schemas have stable canonical JSON;
- path/symlink escape tests;
- output spill thresholds;
- process cancellation and timeout;
- SQLite migrations/recovery;
- FTS retrieval ranking;
- plugin/MCP allowlist enforcement.

## Agent benchmark suite

Start with 100 curated repo tasks:

- 20 bug fixes
- 20 feature implementations
- 15 refactors
- 10 test additions/fixes
- 10 security fixes
- 10 frontend tasks
- 5 dependency/configuration tasks
- 5 repository understanding tasks
- 5 documentation/API consistency tasks

Each task includes a clean repository snapshot, hidden validation and allowed runtime budget.

## Metrics

The repository includes a checked, fully offline `crates/evals/fixtures/benchmark-100.json` suite. It contains exactly 100 unique tasks across ten archetypes: inspect/no-change, exact patch, multi-file patch, search evidence, unsafe path refusal, symlink escape refusal, validation checks, budget checks, patch-conflict recovery, and secret persistence refusal. Run it with:

```bash
cargo run -p agent-evals --bin agent-eval -- \
  --suite crates/evals/fixtures/benchmark-100.json \
  --fixtures crates/evals/fixtures/workspaces \
  --report /tmp/agent-eval-100.json
```

The runner exits nonzero on task failure or a baseline regression beyond `--max-regressions`. Its report includes category pass/fail totals, p50/p95 wall time, aggregate token/tool/diff counts, and regression count. The suite is deterministic in task identity and scoring; wall-clock fields are measurements and are not used by the deterministic report key.

### Quality
- hidden tests passed
- build/type/lint result
- requirement coverage
- regressions
- diff minimality

### Efficiency
- uncached input tokens
- cached input tokens
- output/reasoning tokens where reported
- model calls
- tool calls
- compactions
- wall time
- peak RSS

### Safety
- workspace escape attempts
- denied unsafe commands
- secret file reads
- network attempts

## A/B testing

Every optimization is tested as:

```text
baseline harness + same model
vs
candidate harness + same model
```

Then separately test model changes. Do not change model and harness simultaneously and attribute the gain to one variable.

## Provider profiles

Run the same task set with each provider profile. A tool schema that helps one model may hurt another. Provider-specific adaptations are accepted only when the benchmark demonstrates the gain.

## Token-quality frontier

Plot success rate against median token cost. The target is not minimum tokens; it is the best quality at a sustainable token budget.
