# Performance: RAM, Terminal and Token Efficiency

## The optimization hierarchy

For a coding agent, model inference usually dominates CPU execution time. Optimize in this order:

1. avoid unnecessary model calls;
2. reduce uncached prompt tokens;
3. reduce irrelevant tool results;
4. preserve prompt-cache prefixes;
5. parallelize independent I/O;
6. reduce process/runtime overhead;
7. micro-optimize Rust only after profiling.

## Token strategy

### 1. Stable prefix
Never put timestamps, random IDs or volatile stats in the early system prefix. Canonically order tools and instructions.

### 2. Progressive disclosure
Always-on: base rules + tiny project instructions + compact tool discovery metadata.
On demand: skill content, MCP schemas, reference docs, large memory and source files.

### 3. Local retrieval first
FTS/BM25, ripgrep, tree-sitter and LSP are cheaper than asking an LLM to search blindly.

### 4. Tool output reduction
Filter at source. Ten thousand lines of successful test output are usually worth less than the exit code plus failing cases.

### 5. Batch deterministic work
When safe, one local script can perform filtering/aggregation that would otherwise require multiple tool-result round trips.

### 6. Avoid model hopping inside long sessions
Changing models can destroy cache reuse. If a different model is needed for an isolated task, use a subagent/new context rather than constantly changing the main session.

### 7. Reasoning effort ladder
Use the minimum effort that achieves the eval target. Escalate only after signals such as repeated failure, broad architectural change or complex debugging.

### 8. Compaction
Use native compaction where supported. Fallback compaction stores task state and durable decisions, not verbose prose.

## What major providers publicly expose

### OpenAI
Codex engineering guidance stresses exact prefix stability and prompt caching; changing tools/model/sandbox/cwd can cause cache misses. Conversation compaction is a first-class technique for long sessions.

### Anthropic
Anthropic documents tool search/deferred loading, programmatic tool calling, prompt caching and context editing. Their published examples show very large tool catalogs can have major schema-token overhead and that programmatic tool calling can significantly reduce input tokens in suitable multi-tool workloads.

### Google
Gemini supports implicit context caching on newer models and recommends shared prompt prefixes to improve cache hits. Gemini 3.x guidance also recommends concise prompts and lower thinking levels to reduce unnecessary tool usage when possible.

### Hermes
Hermes uses bounded memory files, progressive skill disclosure, FTS5 session search, output spill and configurable compression/context engines.

## RAM strategy

- Rust single binary for core.
- Lazy-start MCP servers.
- Lazy-start LSP and shut down after configurable idle timeout.
- No browser process until requested.
- SQLite connection pool size 1-4, not dozens.
- SQLite cache size bounded.
- Command output ring buffers.
- Memory-mapped or streamed large files instead of full copies.
- File/symbol cache keyed by mtime/hash and invalidated incrementally.
- Avoid retaining duplicate prompt strings: build segments and stream serialization where practical.
- Keep session transcript durable on disk; only active working set in RAM.

## Performance budgets

Define a reference machine and track:

```text
cold_start_ms
warm_start_ms
idle_rss_mb
active_rss_mb
peak_rss_mb
prompt_build_ms
tool_discovery_ms
sqlite_query_ms
first_token_ms
tokens_uncached
tokens_cached
tokens_output
model_calls
tool_calls
```

Regression gate examples:

- >10% cold-start regression: investigate
- >15 MB idle RSS regression: investigate
- >10% median uncached-token regression on fixed eval suite: block merge unless justified
- task success must not drop to achieve token savings

Optimization without quality measurement is not accepted.
