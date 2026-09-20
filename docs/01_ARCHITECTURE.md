# Architecture

## High-level design

```text
User/TUI
  |
  v
Session Controller
  |
  +--> Task State Machine
  |      |
  |      +--> Context Engine
  |      +--> Agent Loop
  |      +--> Verification Engine
  |
  +--> Provider Router ----> OpenAI / Anthropic / Gemini / Compatible
  |
  +--> Tool Broker
  |      +--> Native Tools
  |      +--> Terminal Runtime
  |      +--> MCP Gateway
  |      +--> Skills
  |      +--> Subagents
  |
  +--> Memory Engine
  |
  +--> SQLite Storage
  |
  +--> Permission/Sandbox Engine
```

## Rust workspace boundaries

### `protocol`
Provider-neutral immutable types: messages, content parts, tool calls, usage, reasoning summaries, patches, events and errors. No HTTP client and no SQLite dependency.

### `core`
The agent state machine. It owns turn lifecycle but not provider wire formats or terminal implementation.

### `providers`
One adapter per provider. Each implements capabilities discovery, streaming, tool mapping, prompt-cache hints, usage normalization and compaction behavior.

### `context`
Builds a `ContextPlan` instead of concatenating strings directly. Segments have category, priority, token estimate, cache-stability and truncation policy.

### `tools`
Typed tool definitions and registry. Core tools are compiled in; plugins/MCP are registered at runtime through capability manifests.

### `terminal`
Process execution, PTY allocation, process groups, bounded output, cancellation and background-job registry.

### `memory`
Durable memories, session retrieval and project facts. Retrieval is local first.

### `sandbox`
Workspace roots, command policy, environment allowlist, network mode and approval workflows.

### `storage`
SQLite migrations, session journal, tool audit, memory store and cached symbol metadata.

## No “god object” agent

The agent loop must not know how SQLite tables work, how Anthropic serializes messages, or how a PTY is created. Dependencies flow inward through traits.

Example traits:

```rust
trait ModelProvider { /* stream, compact, count, capabilities */ }
trait ToolExecutor { /* validate, authorize, execute */ }
trait ContextEngine { /* plan, compact, account */ }
trait MemoryStore { /* retrieve, upsert, invalidate */ }
trait PermissionEngine { /* classify, decide */ }
```

## Event-driven UI

Core emits events through a bounded channel:

- TurnStarted
- ModelDelta
- ReasoningSummaryDelta
- ToolRequested
- ToolStarted
- ToolOutputDelta
- ToolCompleted
- PatchProduced
- VerificationStarted
- VerificationResult
- PermissionRequested
- ContextCompacted
- TurnCompleted

The TUI subscribes; the core never renders terminal escape sequences.

## SQLite schema groups

- sessions / turns / messages
- tool_runs / process_runs
- memories / memory_links
- project_facts
- file_fingerprints / symbol_index
- eval_runs / eval_metrics

Use WAL. Keep large raw command logs in content-addressed spill files rather than a giant DB row when they exceed the configured threshold.

## Extension boundary

Native extensions are trusted code. MCP servers and external plugins are untrusted processes. They cannot bypass permissions. A tool registered dynamically receives the same policy evaluation as a built-in tool.
