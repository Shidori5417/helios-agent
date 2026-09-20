# Roadmap

## Phase 0 — Benchmarks first

- create Rust workspace
- event protocol
- fake model provider
- eval runner
- fixed benchmark repos/tasks

## Phase 1 — Single-agent core

- TUI + exec mode
- session persistence
- OpenAI provider
- read/search/patch/shell/process tools
- permission modes
- verification loop
- git diff UX

Exit: solves core benchmark with reproducible metrics.

## Phase 2 — Context and memory

- context planner
- stable-prefix fingerprints
- compaction
- SQLite FTS session search
- project memory
- `/context` and `/usage`

## Phase 3 — Provider specialization

- Anthropic adapter/profile
- Gemini adapter/profile
- generic OpenAI-compatible adapter
- provider-specific tool/caching strategy

## Phase 4 — Extensions

- MCP stdio + HTTP
- skills
- hooks
- plugin manifests
- LSP/code intelligence plugin

## Phase 5 — Isolated subagents

- context-fork subagents
- optional git worktree isolation
- parallel read/review tasks
- cost/iteration budgets

## Phase 6 — Ecosystem

- plugin registry format
- signed releases
- Arch packaging
- shell completion
- optional browser plugin

## Explicitly postponed

Messaging gateway, full desktop app, voice, image generation, scheduled personal automation and generalized assistant features are postponed until coding benchmarks are excellent.
