# Security and Threat Model

## Assets

- source code
- git history
- credentials and API keys
- local SSH/GPG/config files
- developer workstation integrity
- external systems reachable through MCP
- user privacy and session transcripts

## Trust boundaries

1. model output is untrusted;
2. repository content is untrusted and may contain prompt injection;
3. MCP/plugin servers are untrusted unless explicitly trusted;
4. terminal command output is untrusted data;
5. network content is untrusted;
6. user-approved actions are trusted only within the approved scope.

## Key threats

### Prompt injection from repository files
Source/comments/README content may instruct the model to exfiltrate secrets or ignore policy. Treat repository text as data below system/developer policy.

### Path traversal / symlink escape
Canonicalize target paths immediately before operation. Re-check after symlink resolution. Workspace-write permission is meaningless without this.

### Secret exfiltration
Never automatically read `~/.ssh`, `~/.gnupg`, cloud credential stores or environment secrets. Redact secrets from logs and model-visible terminal output where detectable.

### Command injection
Use argv execution where possible. When using a shell, ensure the command is explicitly model-generated as a command string and passes the permission engine; never interpolate untrusted tool data into shell templates without escaping.

### MCP side effects
A “database” MCP may expose destructive writes. Tools carry permission metadata and allow/deny policy independent of MCP server trust.

### Git destruction
Protect force pushes, destructive resets, checkout-overwrite and history rewrites behind explicit approval/policy.

## Permission modes

```text
read-only
workspace-write
workspace-write+network
full-access (explicit opt-in, warning)
```

Workspace write denies arbitrary writes outside configured roots.

## Secrets architecture

- config references secret names, not secret plaintext where possible;
- OS keyring/plugin can supply values;
- provider requests receive only provider credentials;
- MCP child receives only its allowlisted secrets;
- audit logs store redacted metadata.

## Logs

Structured logs must be useful without becoming a second secret database. Default to no raw prompts/tool payloads in normal logs; allow opt-in debug traces with explicit warning and local retention control.

## Dependency security

- Cargo.lock committed.
- Dependabot/Renovate equivalent optional.
- `cargo audit`/`cargo deny` in CI.
- signed release artifacts where feasible.
- minimal dependencies in sandbox/terminal code.
