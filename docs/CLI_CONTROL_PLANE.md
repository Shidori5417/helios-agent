# CLI control plane

The workspace control plane is managed through `agent-cli`; no manual runtime config is required for the basic setup.

`agent-cli init` creates `.agent/config.toml` idempotently and refuses to overwrite it unless `--force` is explicit. `config list|get|set|unset|path|validate` supports a small typed key set (`provider`, `model`, `base_url`) and writes TOML atomically with best-effort Unix mode `0600`.

Provider credentials are never accepted as ordinary command arguments. `auth login PROVIDER` uses a hidden TTY prompt; `auth login PROVIDER --token-stdin` accepts exactly one line from non-TTY stdin. Credentials are stored in the OS Secret Service/keyring under service `dev.helios-agent`, scoped by provider. If keyring storage is unavailable, login fails closed and never writes plaintext. `auth status` reports only `keyring`, `env`, `absent`, or `keyring-unavailable`; runtime provider resolution is keyring-first with environment fallback for compatibility. `auth logout` deletes only the keyring entry; it cannot remove environment variables.

`doctor` reports binary/config/database/tool/provider-presence diagnostics without printing secret values. `completion generate` writes the committed shell completion to stdout; `completion install SHELL --path ABSOLUTE_PATH` writes it atomically.
