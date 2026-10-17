# Changelog

## Unreleased

- Shell output from recognised build/test commands (cargo, pytest, go, npm
  family) is condensed to failures and summaries when it exceeds the output
  budget; the full output stays in the spill file.
- Tool calls are now a first-class part of the conversation: assistant
  messages keep their calls, tool results carry the call id, and each provider
  uses its native protocol (Anthropic `tool_use`/`tool_result` blocks, OpenAI
  `function_call`/`function_call_output`, chat `tool_calls`/`tool_call_id`).
  Orphaned or unpaired tool history is rendered as text instead of being sent
  to strict APIs, and Gemini receives it as text.
- Transcripts store tool calls as `[tool call NAME] {args}` lines.
- Interactive TUI: `@path` file mentions with a fuzzy popup (bounded,
  symlink-safe index built on first use), Ctrl+R reverse history search and
  Shift+Tab to toggle plan (read-only) and workspace-write. The header now
  follows permission-mode changes (`PermissionModeChanged` event), and each
  turn opens its own assistant block instead of appending to the last one.
- Providers: MCP, LSP and plugin tool names such as `mcp.git.status` are sent
  as wire-safe names (`mcp_git_status`) and mapped back on the response;
  OpenAI and Anthropic reject dots and colons in function names.
- Tools: added `glob`; `search_files` is now bounded (relative paths, result
  and line caps, `glob`/`ignore_case`/`max_results`, no symlink following, skips
  vendor and binary files) and no longer recurses through symlink loops;
  `read_file` streams, refuses huge files and tells the model which
  `start_line` continues a truncated read; `apply_patch` accepts a unique
  `old_text`/`new_text` snippet (the whole-file form still works); every
  tool now publishes a full JSON schema with parameter descriptions.
- Fixed a panic when tool output was truncated inside a multi-byte character.
- A wrong path, unreadable file or missing program is now a recoverable tool
  error instead of ending the turn, and the recovery budget counts
  consecutive failures (3) instead of 2 per turn.
- Fixed prompts containing FTS5 operators (e.g. `**bold**`, quotes, `*`)
  failing the whole turn with an SQLite syntax error during memory/session
  search; free text is now converted to quoted terms.
- Fixed `--workspace` being ignored by interactive turns (worker threads)
  and by `search`, `context`, `usage`, `memory`, `plugins`, `verify` and
  `diff`, which fell back to the process working directory.
- Interactive TUI: styled markdown and diffs with Dark/Light/Monochrome
  palettes, readline-style composer keys, large-paste chips, slash-command
  popup, follow-bottom scrolling with mouse wheel, spinner with elapsed time
  and tokens, status line, collapsible tool cards (Ctrl+O), and a bell after
  long turns.
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
