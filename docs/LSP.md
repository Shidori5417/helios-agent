# LSP boundary

LSP servers are configured in the workspace `.agent/config.toml` with `[[lsp_server]]` entries. The command and arguments are direct argv values; shell interpolation is not supported. The workspace cwd is canonicalized and must remain inside the workspace, and child environment values are available only through an explicit variable-name allowlist.

Enabled servers contribute bounded, provider-neutral metadata for `lsp:<server>:symbols`, `definition`, `references`, and `diagnostics`. Server processes are not spawned during discovery. The `agent-lsp` crate provides lazy stdio JSON-RPC framing, initialization/open/query/shutdown methods, request IDs, response limits, cancellation-aware polling, child cleanup, and path validation. CLI calls are authorized as `ReadOnly` against the canonical workspace file before spawning a server. Real language-server indexing and tree-sitter are intentionally outside this boundary.
