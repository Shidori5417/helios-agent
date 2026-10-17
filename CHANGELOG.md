# Changelog

## Unreleased

- `helios mcp login <id>` and `helios mcp logout <id>`: interactive OAuth login
  (authorization code with PKCE) for HTTP MCP servers. The access token is kept
  in the system keyring, requests carry it as a Bearer token, and an expired or
  missing login fails before any request. Configured with `oauth_authorization_url`.
  Verified against a local fake provider only.
- `helios exec` no longer opens the default state database for session tool
  overrides when `--db-path` is given. Previously text-mode runs could read and
  write the default database regardless of the requested path.
- The workspace sandbox allows `/dev/null` writes, so `2>/dev/null` redirections
  work inside confined commands, and it allows moving directories between
  writable parents. Plugin uninstall previously failed with EXDEV under the sandbox.
- Child processes run with network denied now also refuse IPv4 and IPv6
  socket creation through a seccomp filter. This closes UDP and DNS, which
  Landlock cannot restrict. Covered by a probe test with a network-allowed control.
- Internal: the CLI body moved from `src/main.rs` into a library (`agent_cli`),
  and both `agent-cli` and `helios` are thin wrappers. Behavior is unchanged,
  pinned by a 53-case argv golden corpus (`crates/cli/tests/argv_golden.rs`).
- LSP language servers now run inside the workspace OS sandbox with network
  denied, like MCP stdio servers. Previously they ran unconfined.
- `agent_mcp::oauth_pkce` adds an authorization-code login with PKCE on a
  loopback redirect, tested against a local fake provider. It is library-level
  only: no CLI command uses it and tokens are not stored.
- `helios run|exec --image PATH` attaches PNG, JPEG, GIF or WebP images
  (at most 4, 5 MiB each) to the prompt. Paths must resolve inside the
  workspace and the bytes must match the image signature. Images are sent as
  native parts for Anthropic, OpenAI Responses, chat completions and Gemini.
  The transcript keeps only an `[image attached]` marker, not the bytes.
- HTTP MCP servers can authenticate with OAuth 2.0 client credentials
  (`oauth_token_url`, and environment variable names for the client id and
  secret). Tokens are kept in memory and refreshed before expiry. The
  interactive authorization-code flow is not implemented.
- `helios doctor` reports the OS sandbox state (enforced, disabled, or unavailable).
- Linux: command execution (`shell`, `process`, `verify`, `git diff`) runs
  under a Landlock ruleset in WorkspaceWrite mode: writes and reads are
  confined to the workspace, temp, the tool spill dir and cargo caches, and
  TCP connect/bind is denied. PTY commands are refused in this mode.
  `HELIOS_OS_SANDBOX=off` disables it; unsupported kernels run unrestricted.
- Stdio MCP servers run under the same workspace confinement in every
  permission mode. Their network stays closed unless `allow_network = true`
  is set on the `[[mcp_server]]` entry.
- New `web_fetch` tool: https text pages from hosts listed in
  `permissions.allowed_network_hosts`, with no redirects, a 2 MiB body cap,
  HTML-to-text conversion, approval through the usual broker, and no
  network in read-only mode. Not offered to the model unless the list is set.
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
