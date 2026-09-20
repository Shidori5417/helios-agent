# Context Engine

## Principle

A large context window is capacity, not a target. Good coding agents retrieve the minimum sufficient evidence.

## Context segments

Each segment has:

- `kind`: system, project_rules, skill, memory, conversation, source, diff, tool_result
- `priority`: required / high / normal / disposable
- `stability`: static / session-stable / volatile
- `estimated_tokens`
- `truncate_policy`: none / head-tail / semantic chunks / summarize / spill-reference
- `source_hash`

The engine generates a `ContextPlan`, then provider adapters render it.

## Stable prefix strategy

OpenAI publicly documents that exact prefix matches are important for prompt caching. Therefore order content like:

1. stable base instructions
2. stable tool definitions
3. stable project instructions
4. session-stable skill metadata
5. conversation/history
6. retrieved volatile context
7. current user request

Do not reorder tool schemas nondeterministically. Sort by canonical stable IDs.

Provider-specific caching rules may differ, so the logical plan remains provider-neutral while rendering is provider-aware.

## Repository retrieval pipeline

```text
request
 -> path hints
 -> manifest/language detection
 -> rg filename/content search
 -> symbol search / LSP when enabled
 -> candidate files
 -> rank by lexical relevance + graph proximity + recency
 -> load exact ranges
```

Do not embed the entire repository by default.

## Tool output hygiene

Tool output is one of the biggest sources of context bloat.

- <= 8 KiB: return normally
- 8-64 KiB: return selected head/tail + structured summary
- > 64 KiB: spill to `.agent/spill/<hash>` and return metadata + relevant excerpts

Thresholds should be configurable and tested.

For test commands, parse failures. For `git diff`, prefer touched hunks. For package installers, return errors/warnings and final status rather than progress noise.

## Compaction

Use provider-native compaction when it preserves model state safely. Otherwise use a local checkpoint format that preserves:

- user objective
- accepted constraints
- architectural decisions
- files changed
- unresolved failures
- verification results
- relevant memory IDs

Do not summarize code that remains directly available on disk; keep references and re-read if needed.

## Compaction trigger

Use actual provider-reported token usage when available. Estimate only until a real usage anchor exists. Trigger before quality degrades, with per-model thresholds from the model profile.

## Context observability

Expose `/context` with categories:

```text
system          1.4k
project rules   0.8k
skills          0.3k
memory          0.7k
tools           1.9k
conversation    6.4k
working set     9.8k
free            ...
```

Users should be able to identify what is consuming context.
