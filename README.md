# Production-Grade Coding Agent CLI Blueprint

Research date: 2026-09-20
Target platform: Arch Linux first, Linux portable by design
Reference products: OpenAI Codex CLI, Anthropic Claude Code, Google Antigravity CLI, Hermes Agent

This repository is a design-and-prompt starter pack for a professional coding-agent CLI. The target is not to clone another product. The target is to combine the strongest public design ideas from leading coding agents while keeping the runtime small, safe, provider-neutral, measurable, and token-efficient.

## Product goals

1. High coding quality: inspect before editing, minimal diffs, verify changes, recover from failures.
2. Native terminal experience: fast startup, streaming, PTY support, background jobs, diff review, resume.
3. Low RAM: Rust core, lazy subsystems, bounded buffers, no mandatory vector database, no always-on browser stack.
4. Low token consumption: stable cached prefixes, on-demand tools/skills, local retrieval, output spilling, compaction.
5. Provider-aware behavior: OpenAI, Anthropic, Gemini, OpenAI-compatible/local models each receive a tuned adapter profile.
6. Extension system: MCP, native tools, skills, hooks, plugins, subagents and model providers without coupling them to core.
7. Security by default: workspace isolation, least privilege, secret redaction, explicit network policy, auditable tool calls.
8. Measurable quality: every architectural optimization must be tested with evals, not judged by feel.

## Recommended implementation stack

- Rust stable + Tokio
- ratatui + crossterm for TUI
- reqwest + rustls for HTTP
- serde / serde_json / toml
- SQLite with FTS5 (rusqlite or sqlx)
- tree-sitter for lazy symbol extraction
- ripgrep as a process dependency for fast text search
- portable-pty only for commands that require a real terminal
- tracing for structured logs
- secrecy / zeroize for in-memory secret handling where practical

The project should remain a single process for V1. Do not introduce Redis, Kubernetes, a daemon mesh, or a vector database until an eval proves they solve a measured bottleneck.

## Start here

Read in this order:

1. `AGENTS.md`
2. `docs/00_PRODUCT_SPEC.md`
3. `docs/01_ARCHITECTURE.md`
4. `docs/02_AGENT_LOOP.md`
5. `docs/03_CONTEXT_ENGINE.md`
6. `docs/04_MEMORY_ENGINE.md`
7. `docs/05_MODEL_ROUTING.md`
8. `docs/10_PERFORMANCE_TOKEN_RAM.md`
9. `docs/11_SECURITY_THREAT_MODEL.md`
10. `docs/12_EVALS_BENCHMARKS.md`
11. `IMPLEMENTATION_PROMPT.md`

## Current vertical slice

The workspace now includes a bounded coding loop with fake, OpenAI Responses, Anthropic Messages, Gemini, and generic OpenAI-compatible provider selection, permission-checked native tools, exact-preimage patches, safe direct-argv verification, bounded terminal output with spill files, Unix process-group cleanup, bounded background jobs, and read-only git diff display. Direct argv execution is the default; PTY allocation is explicit (`requires_pty`) and is never inferred from a command string.

```bash
cargo run -p agent-cli -- "inspect this repository"
cargo run -p agent-cli -- --provider openai --model gpt-5 "fix the failing test"
```

Provider adapters read credentials only from their separate environment variables: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, or `GENERIC_API_KEY`. Their `*_BASE_URL` variables (or CLI `--base-url`) select a test/local endpoint. Secret values are never put in errors, logs, or model context. Provider profiles expose typed tool-shape, caching, compaction, reasoning, and streaming capabilities while keeping wire formats outside core.

For example: `cargo run -p agent-cli -- --provider anthropic --model claude-3-5-sonnet "inspect the tests"`; network calls are opt-in through provider selection and are not made by the test suite.

Context planning is provider-neutral: `ContextSegment`/`ContextPlan` provide deterministic ordering, bounded selection, stable-prefix fingerprints, saturating usage accounting, and a compaction boundary.

Milestone 5 adds a bundled SQLite/WAL durable store with idempotent migrations, FTS5 session/message search, bounded project memory, transactional writes, and rejection of secret-like memory keys/content.

Memory can be managed with `memory list`, `memory search --query TEXT`, `memory add --key KEY --content TEXT`, and exact-id `memory supersede`/`memory delete`; all operations are scoped to the canonical current workspace. Agent turns retrieve at most eight matching entries, never write model output back automatically, and invoke the bounded compactor when the context budget is exceeded. Streaming usage events include a saturating normalized total.

Run `usage [--session ID|latest] [--json]` for durable normalized token totals, or `context [--session ID|latest] [--json]` for bounded-plan budget/fingerprint/selection summaries. Lifecycle events are stored with deterministic turn sequence, bounded payloads, and secret-like redaction; raw model/tool deltas are not persisted.

Optional workspace runtime configuration lives at `.agent/config.toml` (or an explicit `--config PATH` inside the workspace). `[[mcp_server]]` entries provide typed stdio/streamable-HTTP settings, tool include/exclude catalogues, and environment-variable names for secrets; literal secret values are not accepted. HTTP calls require an explicit `allowed_hosts` entry. Enabled plugin hook names are listed under `[plugins].enabled`; hooks run through the shared permission/process boundary and fail-closed hooks stop the turn while fail-open results remain observable.

Plugin trust is explicit: `plugins verify/install/update` require a signed, fresh, monotonic registry envelope and public-key-only trust store, verify SHA-256 plus Ed25519 before atomic installation, and accept local files or HTTPS only. Accepted sequence/digest state is persisted atomically in `.agent/registry-state.json`; rollback and same-sequence equivocation are rejected. `plugins uninstall --id ID` moves the exact plugin to `.agent/plugins/.trash` for recovery. The package format is intentionally a single signed `plugin.json` manifest; arbitrary archives and private signing keys are not handled.

Performance evidence is collected offline with `scripts/run-perf.sh` (release startup samples, short fake-session wall/RSS, and the checked 100-task benchmark). It records unsupported RSS as `null`, keeps warm-filesystem caveats explicit, and only applies thresholds when requested.

The LSP extension is a lazy stdio JSON-RPC boundary configured by `[[lsp_server]]` entries in `.agent/config.toml`. It exposes bounded provider-neutral symbol/definition/reference/diagnostic metadata without always-on indexing; direct argv, workspace-canonical cwd and environment allowlists are enforced.

Phase 4 adds isolated extension boundaries: lazy MCP stdio and streamable-HTTP JSON-RPC clients with bounded responses and permission policies, metadata-first/lazy-body skill discovery, and deterministic plugin manifests with explicit lifecycle and typed hook policies. Plugin execution remains limited to declared hook argv through the shared permission/process boundary; arbitrary in-process plugin code is not enabled.

Phase 5 adds deterministic isolated subagent scheduling. `TaskSpec` accepts only a selected context-plan subset, explicit workspace scopes, inherited permission mode, and hard iteration/token/tool-call/wall-clock budgets. The scheduler propagates cancellation, caps concurrency, serializes overlapping scopes, and returns results in stable task order; it does not spawn arbitrary model processes or mutate Git worktrees.

Phase 6 adds Linux distribution scaffolding: a checksum-explicit Arch `PKGBUILD`, measured Cargo release profile, Bash/Zsh/Fish completions, Linux CI, an x86_64 artifact/SHA256 release workflow, SPDX JSON SBOM generation, keyless Sigstore signing/self-verification, installation/uninstallation guidance, and a validated example plugin manifest. No release is published by this repository; action commit pinning remains documented supply-chain debt.

The V1 eval boundary has a fast deterministic offline smoke suite (inspect/no-change, exact patch plus argv validation, and unsafe path refusal):

```bash
cargo run -p agent-evals --bin agent-eval -- --report /tmp/agent-eval-report.json
```

It runs in isolated fixture copies, emits JSON metrics, and exits nonzero on failure. The checked 100-task suite is available with `--suite crates/evals/fixtures/benchmark-100.json`; it covers ten archetypes and reports category totals, p50/p95 wall time, token/tool/diff totals, and baseline regressions. Use `--baseline PATH --max-regressions N` to enforce a configurable regression threshold.

Session prompts and model/tool messages are intentionally persisted in the selected SQLite database so `--resume`, `sessions`, and `search` can work. API keys are provider environment inputs only and are never journaled. The CLI records the canonical workspace path with each session, rejects resume from another workspace, and applies best-effort `0600` permissions to the database on Unix.

The CLI remains non-interactive by default; `--tui` enters the ratatui/crossterm UI only on an interactive TTY and restores the terminal on exit. TUI bounds and key display are controlled with `tui show|set|keys`. The producer and renderer communicate through a bounded event channel; q/Esc/Ctrl-C cancellation propagates to provider, native process and MCP execution. Resize and bounded scroll state are reducer-tested. Provider adapters implement bounded chunked SSE/JSON streaming with assembled tool calls and usage events; malformed, oversized, timed-out, or cancelled streams fail with typed errors. The offline fake provider retains the complete-response fallback path.

The CLI control plane includes `init`, typed atomic `config`, OS-keyring-backed `auth`, `doctor`, and shell `completion` commands. Secret status exposes source only; login never accepts secret arguments or falls back to plaintext when Secret Service/keyring storage is unavailable. See `docs/CLI_CONTROL_PLANE.md`.

Runtime extensions are also controlled through typed `providers`, `permissions`, `mcp`, `lsp`, `hooks`, `skills`, `subagents`, and `status` commands. Hooks, skills, and subagents use their real runtime boundaries; MCP/LSP test commands perform bounded configured handshakes and require explicit network policy for HTTP.

Terminal cancellation and timeouts kill the Unix process group, so descendants do not remain running. Background jobs have deterministic IDs, bounded concurrency, cancellation, wait/reap, and bounded output spill. PTY mode is intentionally opt-in and remains a small non-interactive boundary; full interactive terminal UX is postponed.

MCP supports both lazy stdio and streamable HTTP JSON-RPC transports. HTTP is denied by default and requires an explicit policy host approval; authentication accepts only an environment-variable name. Plugin hooks execute explicit argv through the same permission, timeout, cancellation, bounded-output, and workspace-canonicalization boundary. Fail-closed hooks block on failure; fail-open hooks record a bounded failure and continue. In-process arbitrary plugin code is not supported.

Subagents can optionally use manager-owned detached Git worktrees. The manager validates a canonical Git source and explicit temporary root, resolves an explicit ref (default `HEAD`), never copies uncommitted changes, re-roots task scopes, and removes only exact manager-owned worktree paths during explicit cleanup. The manager never performs destructive cleanup from `Drop`.

Recoverable tool failures such as patch conflicts are bounded and fed back to the provider as tool error messages. Permission, path-escape, and other fail-closed errors are not retried. Each CLI turn is journaled transactionally as `running`, `complete`, or `failed` with a bounded error message, so resume can safely observe an incomplete/failed turn.

## Proposed repository layout

```text
agent-cli/
├── Cargo.toml
├── AGENTS.md
├── crates/
│   ├── cli/                # CLI entry point, command parsing
│   ├── tui/                # ratatui UI, rendering, key handling
│   ├── core/               # agent loop and state machine
│   ├── protocol/           # provider-neutral message/tool/event types
│   ├── providers/          # OpenAI, Anthropic, Gemini, OpenAI-compatible
│   ├── context/            # prompt assembly, budgeting, compaction
│   ├── memory/             # persistent memory + retrieval
│   ├── tools/              # native tool registry
│   ├── terminal/           # process, PTY, output spill, process groups
│   ├── mcp/                # MCP clients and server registry
│   ├── skills/             # skill discovery / progressive disclosure
│   ├── plugins/            # plugin manifests and lifecycle hooks
│   ├── subagents/          # bounded deterministic task scheduler
│   ├── sandbox/            # permissions / execution policy
│   ├── storage/            # SQLite schema and migrations
│   └── evals/              # benchmark runner and scoring
├── prompts/
├── skills/
└── docs/
```

## Non-goals for V1

Do not initially ship a messaging gateway, WhatsApp/Telegram integration, image generation, full browser automation, cron platform, cloud orchestration, or a generalized personal assistant. Those can exist as plugins later. The core product is a coding agent.

## Definition of done for V1

A release is not complete because it can edit files. It is complete when it can repeatedly solve a fixed benchmark suite with bounded token usage, recover from failed edits, run project validation, preserve a stable prompt cache, and refuse unsafe workspace escapes without corrupting the repository.
