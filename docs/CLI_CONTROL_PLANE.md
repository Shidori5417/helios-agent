# CLI control plane

The workspace control plane is managed through `agent-cli`; no manual runtime config is required for the basic setup.

`agent-cli init` creates `.agent/config.toml` idempotently and refuses to overwrite it unless `--force` is explicit. `config list|get|set|unset|path|validate` supports a small typed key set (`provider`, `model`, `base_url`) and writes TOML atomically with best-effort Unix mode `0600`.

Provider credentials are never accepted as ordinary command arguments. `auth login PROVIDER --token-stdin` accepts input only from a non-TTY stdin and attempts Secret Service/keyring storage; if that store is unavailable, the command fails closed and does not write plaintext. `auth status` reports only provider and presence booleans. Runtime provider resolution retains environment fallback for compatibility; keyring-first integration is the next platform-specific adapter boundary.

`doctor` reports binary/config/database/tool/provider-presence diagnostics without printing secret values. `completion generate` writes the committed shell completion to stdout; `completion install SHELL --path ABSOLUTE_PATH` writes it atomically.
