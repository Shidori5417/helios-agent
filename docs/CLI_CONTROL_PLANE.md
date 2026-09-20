# CLI control plane

The workspace control plane is managed through `helios`; `agent-cli` remains a compatibility alias. No manual runtime config is required for the basic setup.

`helios init` creates `.helios/config.toml` idempotently and refuses to overwrite it unless `--force` is explicit. An existing `.agent/config.toml` is read for compatibility and is never overwritten automatically. `config list|get|set|unset|sources|path|validate` supports a small typed key set (`provider`, `model`, `base_url`) and writes TOML atomically with best-effort Unix mode `0600`; `config sources` reports only the winning source category, never values.

Workspace selection is explicit with global `--workspace PATH`. Configuration precedence is built-in defaults, XDG global config, project config (or legacy `.agent/config.toml`), `HELIOS_*` environment overrides, CLI options, then session-specific options. Durable state defaults to XDG data/state/cache paths under `helios`; an explicit `--db-path` remains supported.

Provider credentials are never accepted as ordinary command arguments. `auth login PROVIDER` uses a hidden TTY prompt; `auth login PROVIDER --token-stdin` accepts exactly one line from non-TTY stdin. Credentials are stored in the OS Secret Service/keyring under service `dev.helios-agent`, scoped by provider. If keyring storage is unavailable, login fails closed and never writes plaintext. `auth status` reports only `keyring`, `env`, `absent`, or `keyring-unavailable`; runtime provider resolution is keyring-first with environment fallback for compatibility. `auth logout` deletes only the keyring entry; it cannot remove environment variables.

Session control is workspace-scoped: `sessions list|show|archive|unarchive|delete|export|import` uses exact IDs, requires `--confirm ID` for deletion, and stores deletion as a recoverable tombstone. Exports are bounded, redacted JSON and imports require schema version and matching canonical workspace. `database backup` uses SQLite `VACUUM INTO`; restore requires `--confirm-workspace`, validates integrity/schema, and keeps a rollback copy during replacement. `audit list|show|export` exposes bounded, already-redacted lifecycle events only.

Persistent background jobs are managed with `jobs start -- COMMAND ARGS...`, `list`, `status`, `logs`, `cancel`, `wait`, and `reap`. Jobs run through a private supervisor process in the XDG state `helios/jobs` registry, use a separate Unix process group, clear the child environment, bound logs through the existing output files, and reject secret-like arguments. Cancellation verifies PID start time and process-group identity before signalling.

Offline evaluation and startup performance checks are available from the same binary: `eval run --suite smoke|benchmark-100`, `eval show`, and `eval baseline create`, plus `perf run`, `perf show`, and `perf compare`. The built-in suites are embedded in the executable so an installed binary does not depend on the source checkout. Reports default to XDG state `helios/reports`, are written atomically with best-effort `0600` permissions, and existing files are never overwritten; baseline regressions return a non-zero status.

Self-management is explicit and user-scoped: `self version`, `self install --prefix ~/.local --name helios`, `self update --from PATH --sha256 HEX`, and `self uninstall --confirm NAME`. Install/update use atomic replacement and `0755`; uninstall verifies the install marker and moves the exact binary to the user trash directory. Remote updates are fail-closed when no verified release is available; no `sudo`, shell, or plaintext token path is used.

Plugin trust is controlled with `trust list|show|add|disable|enable|revoke|remove`; only 32-byte Ed25519 public-key hex is accepted, and the default store is XDG data `helios/trust-store.json` with atomic `0600` writes. `registry status|reset|verify` exposes rollback state, requires exact reset confirmation, and verifies signed envelopes without mutating state. Private-key material is rejected.

Configured extension smoke tests use `mcp test --id ID` and `lsp test --id ID`. MCP tests initialize and list tools without calling them; HTTP MCP additionally requires `--allow-network` and an exact configured host allowlist entry. LSP tests initialize and shut down the configured process, and `--file PATH` enables bounded document-open/symbol probing after workspace permission checks.

Hooks, skills, and subagents use their real runtime boundaries: `hooks list|enable|disable|test`, `skills list|show|validate`, and `subagents show|config|run|test`. Hook enablement is persisted under `[plugins].enabled`; skills are discovered metadata-first from `skills/` and `.helios/skills` (with legacy `.agent/skills` fallback); subagent runs use the selected fake or configured provider, persisted budgets, inherited permissions, optional bounded `--context-file`, `--session`, or `--memory-query` context, and optional manager-owned detached worktrees.

TUI preferences are controlled by `tui show|set|keys`; safe bounds cover tick interval, event-channel capacity, scroll step, and `auto|always|never` color mode.

`doctor` reports binary/config/database/tool/provider-presence diagnostics without printing secret values. `completion generate` writes the committed shell completion to stdout; `completion install SHELL --path ABSOLUTE_PATH` writes it atomically.
