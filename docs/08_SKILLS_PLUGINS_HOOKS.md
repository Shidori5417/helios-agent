# Skills, Plugins, Hooks and Subagents

## Skills

A skill is procedural knowledge, not executable trust.

Layout:

```text
skills/<name>/
  SKILL.md
  references/
  templates/
  scripts/
```

Only name + one-line description is indexed by default. Full `SKILL.md` loads on demand. References load only when requested.

Frontmatter example:

```yaml
---
name: bugfix
description: Reproduce, isolate, patch and verify a software defect.
auto_invoke: true
risk: low
---
```

Keep skill descriptions concise because they may be part of discovery context.

## Hooks

Hooks are deterministic lifecycle automation:

- SessionStart
- UserPromptSubmit
- PreToolUse
- PostToolUse
- PreEdit
- PostEdit
- PermissionRequest
- PreCompact
- Stop

Use hooks for rules that must happen every time: formatting, policy enforcement, audit logging, secret scanning. Do not spend model tokens on deterministic enforcement.

## Plugins

Plugin manifest may bundle:

- native tools
- skills
- hooks
- MCP server definitions
- slash commands
- provider adapters

Plugins run outside the trusted core boundary unless signed/installed as trusted native modules. Prefer process/WASM isolation for third-party executable plugins rather than arbitrary in-process dynamic libraries.

## Subagents

Subagents are primarily a context-isolation mechanism.

Good cases:

- explore a large subsystem and return a compact report;
- run independent code/security/test reviews;
- research an external API;
- perform parallel work in isolated git worktrees.

Bad cases:

- simple rename;
- one-file fix;
- work requiring constant back-and-forth with the parent context.

Parent receives a concise structured summary, not the subagent's full transcript.

## Agent teams

Defer full peer-to-peer multi-agent teams until the single-agent benchmark is strong. Multi-agent orchestration can multiply cost and race conditions without improving success rate.
