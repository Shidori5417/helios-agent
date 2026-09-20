# Memory Engine

## Distinguish memory from context

Context is what the model sees now. Memory is durable information that may be retrieved later. Do not inject all memory into every turn.

## Memory classes

### User memory
Stable preferences relevant to development: preferred package manager, output style, recurring constraints. Keep small.

### Project memory
Architecture, commands, conventions, deployment assumptions and decisions that are not easily derived from the repository every time.

### Episodic memory
Useful prior incidents such as “migration X failed because Y”. It should expire or lose weight over time.

### Session history
Raw past messages stored separately and searchable by FTS5. This is not automatically “memory”.

## Storage schema

```text
memory(
  id,
  scope,
  kind,
  key,
  content,
  confidence,
  importance,
  created_at,
  updated_at,
  last_accessed_at,
  access_count,
  expires_at,
  source_session_id,
  status,
  superseded_by
)
```

## Retrieval

V1 uses SQLite FTS5/BM25 + metadata filters. Optional embeddings can be a plugin, not a core dependency.

Ranking can combine:

```text
lexical relevance
+ importance
+ recency
+ scope match
+ access utility
- contradiction penalty
```

## Write policy

Do not call a separate LLM after every message just to extract memory. Prefer:

1. deterministic candidates from explicit project settings and commands;
2. a compact memory side-channel emitted by the main model only when useful;
3. background/cheap review at session end only if enabled.

Memory writes should support approval mode.

## Contradictions

Never blindly append conflicting facts. Track replacement:

```text
old memory -> status=superseded -> superseded_by=new_id
```

## Bounded always-on memory

Keep an optional tiny startup snapshot similar in spirit to Hermes' bounded user/project memory, but do not rely on it for full recall. Typical target: 500-1500 tokens total. Everything else is retrieval-on-demand.

## Pre-compaction checkpoint

Before dropping history, extract only durable decisions and unresolved task state. The full raw transcript remains stored locally for search/resume.
