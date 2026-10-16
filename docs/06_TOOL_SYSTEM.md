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

## Shell

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
