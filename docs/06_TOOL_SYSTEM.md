# Native Tool System

## Tool design principles

Tools are APIs for models. Bad schemas waste tokens and reduce reliability.

Every tool definition contains:

```text
id
version
short_description
input_schema
permission_class
context_cost_hint
supports_parallel
output_policy
timeout
capability_tags
```

## Core tools

Keep the always-available set intentionally small:

- `read_file`
- `search_files`
- `glob`
- `apply_patch`
- `shell`
- `process`
- `update_plan`
- `tool_search` or provider-equivalent

Optional tools load via capability discovery.

## Read tool

Support line/range reads and file-size guardrails. Never return multi-megabyte files by default.

Implemented limits: files over 32 MiB are refused, at most 1 MiB is collected per request, and output is cut at a line boundary with a `[truncated after line N; use start_line=N+1 to continue]` hint. A missing path, directory or non-UTF-8 file is a recoverable tool error that names the path, so the model can correct itself instead of ending the turn.

## Search

Use ripgrep for text and file discovery. Return ranked hits with line numbers and a configurable cap.

The built-in `search_files` is a literal-string fallback with the same contract: workspace-relative `path:line:text` hits, `glob` / `ignore_case` / `max_results` options (default 200, maximum 1000), a 300-character line cap, and a footer when a limit stops the scan. `glob` lists files (`**`, `*`, `?`; a pattern without `/` matches any depth). Both use one deterministic, sorted walker that never follows symlinks, skips `.git`, `target`, `node_modules` and similar vendor directories, stops after 20,000 files, and ignores binary or over-1 MiB files.

## Patch

Use a deterministic patch format. Validate path scope and preimage. Reject fuzzy unsafe writes rather than silently applying to the wrong region.

`apply_patch` has two exact forms; a call must use exactly one. The snippet form (`old_text` / `new_text`, optional `replace_all`) replaces one unique occurrence and costs only the changed region in output tokens; zero matches is a patch conflict, several matches without `replace_all` is an invalid-argument error that asks for more context, and an empty `old_text` creates a missing file (parents are created inside the workspace). The whole-file form (`expected_content` / `replacement`) is kept for compatibility and requires the exact current content.

## OS sandbox (Linux)

Child processes started by `shell`, `process`, `verify` and `git diff` run under a Landlock ruleset when the registry is in WorkspaceWrite mode and the kernel supports Landlock (ABI 1 or later; ABI 4+ also denies TCP connect and bind). The rules are built in the parent before fork and applied in the child with `prctl(PR_SET_NO_NEW_PRIVS)` and `landlock_restrict_self`, which allocate nothing.

- Writable: the workspace, the temp directory, the tool spill directory, and cargo's `registry` and `git` caches. `~/.cargo` as a whole is not writable because it holds `credentials.toml`.
- Readable: system roots (`/usr`, `/etc`, `/proc`, `/dev`, and so on), PATH entries, `~/.rustup`, `~/.cargo/bin`, `~/.cargo/config.toml`, and git configuration. Other home-directory data, such as `~/.ssh`, is denied.
- Network: TCP connect and bind are denied for every child process. UDP is not restricted, so DNS lookups still happen.
- PTY commands cannot take the restriction, so they are refused in this mode rather than run unrestricted.
- Opt out with `HELIOS_OS_SANDBOX=off`. On kernels without Landlock the commands run unrestricted; the registry reports this through `os_sandbox_active()`.

Stdio MCP servers get the same confinement, in every permission mode, because they are untrusted. Their TCP access is closed unless the `[[mcp_server]]` entry sets `allow_network = true`. Limitation: a server that needs its own package cache outside the workspace (for example `uv`) must be given a path through configuration, which is not supported yet, so such servers may fail to start.

Tests cover writes outside the workspace, reads of a credential-like directory outside the allowed roots, and TCP connections, with a control run showing the probe succeeds without the sandbox. Not yet covered: a full `cargo test` under the sandbox, and MCP servers and hooks, which spawn processes outside this path.

## MCP OAuth (client credentials)

HTTP MCP servers can authenticate with OAuth 2.0 client credentials. The `[[mcp_server]]` entry names three things: `oauth_token_url` (https only), `oauth_client_id_env` and `oauth_client_secret_env` (environment variable names; values are never accepted in the config), and optionally `oauth_scope`. Literal values are rejected. The access token is held in memory only, refreshed before it expires (expiry defaults to 300 s when the server omits `expires_in`), and dropped on a 401 so the next request fetches a new one. The token endpoint is network access, so it passes the server's host allowlist. Errors name the HTTP status but never the response body or the credentials.

Scope limit: only client credentials is implemented. Interactive authorization-code flows with PKCE, which need a browser redirect, are not implemented and cannot be verified without a live authorization server.

## Web fetch

`web_fetch` is the only tool with `PermissionClass::Network`. The engine's class policy still denies network; instead the tool is granted per host from `permissions.allowed_network_hosts` (managed with `permissions add-network-host`). It is not offered to the model unless that list is non-empty, and it is refused in read-only (plan) mode.

Policy and limits: https only, default port only, no credentials, no IP literals, a DNS name must be on the list (case-insensitive, exact match). Redirects are never followed, so a redirect cannot reach an unlisted host. Bodies are read up to 2 MiB; only text media types are accepted, and HTML is reduced to text (scripts, styles and comments dropped). Each request goes through the normal approval broker when one is attached. Logs and errors name only the host, never the path or query. Output is bounded and spills like other tools. Requests carry only a `User-Agent`; no cookies are stored.

## Shell

When a recognised build or test command (`cargo test|build|check|clippy`, `pytest`, `python -m pytest`, `go test|build|vet`, `npm|pnpm|yarn|bun test|run|build|lint`) produces more output than the budget, the result is condensed deterministically: only error, failure, location and summary lines are kept, with `… N line(s) omitted` markers. The full stream is always available through `spill_ref`. No model call is involved, and the rules are plain text matches, so they cannot hide a failure that matches a marker.


Prefer direct argv execution when no shell syntax is required. Use a shell only for pipes/redirection/globbing/compound commands. This improves quoting safety and observability.

## Process

Background processes need stable IDs, process groups, output cursors, wait/poll/log/kill/write actions and cleanup on session exit.

## Tool search

Do not expose 100 MCP tools as full JSON schemas to every model request.

Local index fields:

- tool name
- short description
- server/plugin
- tags
- argument names

Search with BM25/FTS locally. Provider-native deferred tool mechanisms should be used when supported.

## Output normalization

Every result should be structured:

```json
{
  "status": "ok|error|timeout|denied",
  "summary": "...",
  "data": {},
  "truncated": false,
  "spill_ref": null
}
```

Do not send ANSI progress bars or repeated spinner frames to the model.
