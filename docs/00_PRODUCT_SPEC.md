# Product Specification

## Product identity

A terminal-native software engineering agent: fast enough for daily use, capable enough for repository-scale tasks, and transparent enough that a developer can understand what it changed and why.

## Primary user journeys

### Understand a repository
The agent maps manifests, languages, build/test commands, architectural boundaries and important symbols without dumping the entire repository into context.

### Fix a bug
The agent reproduces or locates the failure, identifies the smallest cause, edits minimally, runs targeted validation and summarizes the evidence.

### Implement a feature
The agent discovers relevant interfaces, follows local conventions, writes tests when appropriate, builds/validates and presents the final diff summary.

### Refactor
The agent builds a dependency/symbol impact map, performs staged edits, validates behavior and avoids unrelated formatting churn.

### Review
The agent reviews diffs with references to concrete lines/symbols and separates correctness, security, performance and maintainability findings.

## Product principles derived from leading agents

### From Codex
- Harness quality is as important as raw model quality.
- Preserve exact prompt prefixes to maximize caching.
- Use model-trained tool shapes where available (`shell`, patch, plan semantics).
- Context compaction is a first-class runtime concern.

### From Claude Code
- CLAUDE-style persistent project instructions should be concise and hierarchical.
- Skills and large tool schemas should load only when required.
- Subagents are valuable primarily for context isolation, not because “more agents” is always better.
- Hooks enforce deterministic actions that should not rely on model compliance.
- Code intelligence/LSP can reduce broad file reads.

### From Antigravity
- A native lightweight CLI can share one harness with other surfaces.
- Skills, hooks, subagents and plugins are core extension primitives.
- Multi-agent work should be asynchronous and isolated.
- Sandboxing, credential masking and Git protections belong in the harness.

### From Hermes
- Memory, context engines and model providers should be pluggable.
- Skills should use progressive disclosure.
- Session search can be local and cheap using SQLite FTS5.
- Toolsets and MCP filtering prevent irrelevant tools from polluting context.

## Functional requirements

- Interactive TUI and non-interactive `exec` mode.
- Streaming model output and streaming command output.
- Resumeable sessions.
- Git-aware diff display.
- Workspace read/write policy modes.
- Native shell/process tools and deterministic patching.
- MCP client support: stdio and streamable HTTP.
- Skill discovery and progressive loading.
- Hooks: session start/end, pre/post tool, pre/post edit, pre-compact, permission request.
- Plugin manifests that can bundle skills, hooks and MCP definitions.
- Provider adapters.
- Context budgeting and compaction.
- Persistent project/user memory.
- Subagent execution with isolated context.
- Eval/benchmark runner.

## Non-functional targets

Targets must be measured on a defined reference machine; they are goals, not guarantees.

- Cold startup target: under 150 ms before network/auth work.
- Idle RSS target: under 60 MB with no LSP/MCP/browser processes.
- Standard coding session target: under 120 MB core RSS excluding external LSP/MCP child processes.
- Terminal output memory: bounded, with disk spill for large output.
- Crash safety: SQLite WAL + transactional state writes.
- No silent destructive command execution.
- Context accounting error should stay below a configured tolerance once provider usage data is available.

## Success metrics

- task success rate
- tests/build pass rate
- regression rate
- median and p95 tokens per solved task
- cache-hit input tokens
- model calls per task
- tool calls per task
- wall-clock latency
- max RSS and child-process RSS
- diff size and unrelated-line ratio
- permission denials / unsafe attempts
