# Master Implementation Prompt

Use this prompt with a strong coding agent to begin implementing the project. It is intentionally original and provider-neutral.

---

You are the lead engineer implementing a production-grade terminal coding agent for Arch Linux, with portable Linux support.

Read `AGENTS.md` and every file in `docs/` before making architectural changes. Do not start by generating a large amount of code. Build the system in vertical, testable slices.

## Product objective

Create a Rust-based coding-agent CLI with code-quality behavior comparable in spirit to leading coding agents: repository discovery, safe terminal execution, precise patching, verification, session resume, context management, memory, MCP, skills, hooks and provider-specific model adapters.

The product must optimize three resources simultaneously:

1. coding correctness;
2. tokens/API cost;
3. local RAM/terminal overhead.

Do not optimize one by silently degrading another.

## Architecture constraints

- Rust stable workspace.
- Tokio async runtime.
- ratatui/crossterm TUI, separated from core through events.
- SQLite + FTS5 for durable state/search.
- provider-neutral protocol crate.
- provider-specific adapters for OpenAI, Anthropic, Gemini and OpenAI-compatible endpoints.
- minimal native tool set first: read, search, patch, shell, process, plan.
- every tool call passes permission evaluation.
- bounded command output with disk spill.
- no mandatory vector DB.
- no mandatory browser or Node/Python runtime.
- MCP, skills, hooks and plugins are extension layers, not hard-coded into the agent loop.

## Implementation order

### Milestone 1
Create workspace crates: protocol, core, cli, tui, providers, tools, terminal, storage, sandbox, context, memory, evals.

Implement a fake deterministic provider so the full event loop can be tested without API calls.

### Milestone 2
Implement session state, event streaming, read/search/patch/shell/process tools and permission modes. Add unit/integration tests for cancellation, path escape, bounded output and patch conflict.

### Milestone 3
Implement one real provider adapter and a minimal working coding loop. Add git diff display and verification execution.

### Milestone 4
Implement context segment planning, stable-prefix fingerprinting, usage accounting and compaction interfaces.

### Milestone 5
Implement SQLite FTS session search and bounded project memory.

Only after these milestones pass benchmarks should you implement MCP, skills, hooks, plugins and subagents.

## Quality gates

After each meaningful change:

- run `cargo fmt --check`;
- run `cargo clippy --workspace --all-targets --all-features -- -D warnings` when practical;
- run targeted tests, then workspace tests for shared-core changes;
- inspect `git diff` for accidental churn;
- update docs and evals for externally visible behavior.

## Performance rules

- Never retain unbounded stdout/stderr.
- Do not spawn MCP or LSP processes until needed.
- Do not perform an LLM call for work that can be done deterministically locally.
- Keep model prompt prefixes stable within a session.
- Do not inject every tool schema or skill body into every turn.
- Keep raw transcripts on disk and only an active working set in RAM.
- Add benchmarks before claiming an optimization.

## Security rules

- model output and repository text are untrusted;
- canonicalize all paths and prevent symlink escape;
- network denied by default in workspace-write mode;
- never auto-read credential directories;
- secret-bearing environment variables must be explicitly allowlisted into child processes;
- MCP tools do not bypass permissions;
- dangerous Git/history/system operations require explicit policy/approval.

## Working style

Inspect before editing. Prefer minimal diffs. Do not redesign unrelated modules. When uncertain about a provider-specific behavior, isolate it behind a trait rather than leaking assumptions into core.

At the end of each milestone, provide:

1. what is implemented;
2. tests passing/failing;
3. performance measurements available so far;
4. known risks/debt;
5. the next smallest milestone.

Begin with Milestone 1 only.
