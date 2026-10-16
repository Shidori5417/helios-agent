# Changelog

## Unreleased

- Security: dangling symlinks can no longer redirect workspace writes outside
  the workspace; the terminal is restored when a release build panics.
- Providers send the session effort (OpenAI `reasoning.effort`, Anthropic
  `output_config.effort`), Anthropic prompt-cache breakpoints and a 16000
  `max_tokens` default; cache read/write usage is reported.
- The agent now sends a stable system prompt with bounded `AGENTS.md`
  project instructions; Anthropic/Gemini no longer drop later system messages.
- Fixed `helios --db-path PATH exec ...` (and other run options placed before
  `run`/`exec`) being parsed as a non-exec run with `exec` as prompt text.
- Fixed `clippy::manual_option_zip` under Rust 1.97 so the CI lint gate passes.
- Made the process-group timeout test treat reaped-later zombies as terminated,
  and isolated the broken-pipe `sessions list` test from the user database.
- Pinned all GitHub Actions to verified commit SHAs and set workflow tokens to
  `contents: read`.

## 0.1.0 - 2026-09-21

- Added the Helios `helios` binary with `agent-cli` compatibility entry point.
- Added headless text, JSON, and stream-JSON execution with durable sessions.
- Added bounded tools, permissions, provider adapters, MCP/LSP boundaries,
  skills, plugins, checkpoints, tasks, subagents, and local control commands.
- Added the offline 100-task evaluation and release performance harness.

This is the first private repository release. No public GitHub release or
published package is implied by this changelog.
