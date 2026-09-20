# Runtime control commands

Runtime extension settings are managed with typed CLI commands: `providers`, `permissions`, `mcp`, `lsp`, `hooks`, `skills`, `subagents`, and consolidated `status`. Mutations use canonical ids, reject secret literals and shell command strings, validate MCP HTTP URLs as HTTPS, and write the workspace TOML atomically. `mcp test` and `lsp test` perform bounded configured local handshakes; HTTP MCP remains denied unless explicit host and network policy are supplied.

MCP authentication accepts only an environment-variable name (`--auth-env`), never a token value. LSP and MCP command arguments are stored as argv-oriented fields. Existing runtime loaders continue to consume `.agent/config.toml`; the control plane is additive and preserves compatibility with explicit flags.
