# MCP Architecture

## Purpose

MCP is the integration boundary for external systems. It is not the default implementation for functionality the core already does efficiently.

## Supported transports

- stdio
- streamable HTTP

Add additional transports only when interoperability requires them.

## Registry

```text
McpServerConfig
  name
  transport
  command/url
  args
  cwd
  env_allowlist
  enabled
  tool_include
  tool_exclude
  startup_timeout
  tool_timeout
  trust_level
```

## Lazy connection

Do not spawn every configured MCP server at CLI startup. Store manifests and connect when a tool from that server is requested, unless the user marks it eager.

This directly reduces startup time and RAM.

## Tool discovery strategy

1. cache tool metadata by server fingerprint;
2. expose only names/short descriptions to local tool search;
3. load exact schemas for selected tools when the provider supports deferred loading;
4. otherwise use a stable MCP gateway tool or freeze the selected toolset for the session to protect prompt caching.

## Security

MCP is not trusted simply because it is configured.

- scope filesystem servers to project roots;
- prefer allowlists over denylists for write-capable systems;
- do not pass the full parent environment;
- secrets are injected only into the server process that needs them;
- every resulting operation still passes the agent permission engine where possible;
- log tool name, server, duration, result class and approval decision, not secret payloads.

## CLI UX

```text
/mcp list
/mcp add
/mcp remove
/mcp enable
/mcp disable
/mcp tools <server>
/mcp allow <server> <tool>
/mcp deny <server> <tool>
/mcp test <server>
/mcp reconnect <server>
```

## Avoid MCP overuse

If a native operation is common, latency-sensitive and security-critical (file read, patch, terminal), keep it native. MCP is strongest for external services such as issue trackers, databases, observability systems, browsers and organization APIs.
