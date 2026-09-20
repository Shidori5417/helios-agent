# Terminal Runtime

## Objective

Provide Claude Code/Codex-class shell interaction without turning every command into a RAM or token leak.

## Execution modes

### Direct process mode
Default. Use `tokio::process::Command`, no PTY. Best for compilers, tests, git, rg and scripts.

### PTY mode
Allocate only for programs that require TTY semantics: interactive prompts, full-screen apps or REPLs.

PTYs are more expensive and harder to parse; they should not be the default.

## Output buffering

Maintain:

- bounded in-memory ring buffer for live UI;
- optional full spill file on disk;
- parser-specific structured summary;
- cursor for incremental process polling.

Never keep unlimited stdout/stderr in RAM.

## ANSI handling

The UI may render ANSI, but model-facing output should strip color/control sequences unless they carry meaning. Avoid returning progress animations.

## Process groups and cancellation

Every spawned command belongs to a controllable process group. `Ctrl+C` in the agent should first cancel the current turn/tool, not necessarily terminate the entire CLI. Escalate signals if a child ignores termination.

## Shell selection

Prefer direct argv. When shell syntax is needed, execute through a configured shell with explicit `-lc`. Record the resolved shell and working directory in the audit event.

## Environment

Default child environment policy:

- inherit safe basics such as PATH, HOME, TERM and locale;
- redact known credential patterns from model-visible logs;
- support include/exclude patterns;
- do not forward secrets into sandbox/MCP children unless explicitly configured.

## Command policy

Commands are classified by effect, not by a fragile fixed list alone.

- read-only workspace
- write workspace
- network
- package/install
- git history mutation
- outside-workspace write
- privileged/system

The permission engine can combine executable/path/network metadata with configured rules.

## Background processes

Support long-running dev servers without polling spam. The process registry emits completion events and allows targeted log reads.
