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
- `apply_patch`
- `shell`
- `process`
- `update_plan`
- `tool_search` or provider-equivalent

Optional tools load via capability discovery.

## Read tool

Support line/range reads and file-size guardrails. Never return multi-megabyte files by default.

## Search

Use ripgrep for text and file discovery. Return ranked hits with line numbers and a configurable cap.

## Patch

Use a deterministic patch format. Validate path scope and preimage. Reject fuzzy unsafe writes rather than silently applying to the wrong region.

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
