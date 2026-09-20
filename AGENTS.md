# Agent Development Instructions

## Mission

Build a professional terminal-first coding agent for Arch Linux and portable Linux. Optimize for code correctness, safe execution, low token use, low idle memory, predictable behavior, and debuggability.

## Engineering rules

- Architecture before feature count. Keep the core small and extension points explicit.
- Prefer deterministic local computation over an extra LLM call.
- Treat model context as a scarce resource. Do not inject data merely because it is available.
- Preserve provider prompt-cache prefixes. Stable ordering and stable schemas matter.
- Never read an entire repository when targeted discovery can answer the question.
- Prefer minimal patches over whole-file rewrites.
- Never report success before running the strongest practical validation available for the changed scope.
- Every new tool must define: permission class, input schema, output budget, timeout, cancellation behavior, logging policy and secret policy.
- Every new provider must pass the provider-contract tests and token-accounting tests.
- Every performance claim must have a benchmark.

## Core constraints

- Rust for the production core.
- Async I/O via Tokio.
- SQLite/FTS5 is the default durable store.
- No mandatory vector DB.
- No unbounded in-memory command output.
- No shell command may bypass the permission engine.
- Network is denied by default inside workspace-write sandbox unless explicitly enabled.
- MCP servers are untrusted extensions; their tools require independent policy evaluation.
- Secrets must never be injected into the model unless the requested operation strictly needs them.

## Coding workflow

For non-trivial changes:

1. Inspect the relevant module and tests.
2. State the smallest viable change internally in task state.
3. Implement a focused patch.
4. Run formatter/type/compiler checks for touched crates.
5. Run targeted tests, then broader tests when justified.
6. Inspect the resulting diff.
7. Remove accidental or unrelated changes.
8. Update docs/evals when behavior changes.

## Context discipline

Always-on context must be short. Long reference material belongs in skills or docs loaded on demand. Tool catalogs must not be fully rendered into every request. Large tool output must be summarized locally or spilled to disk and retrieved selectively.

## Compatibility

Do not couple core message types to one provider's wire format. Provider-specific behavior belongs behind adapters and model profiles. Preserve the ability to run OpenAI, Anthropic, Gemini and OpenAI-compatible endpoints.

## Security

Do not weaken sandboxing to make an integration easier. Never auto-approve commands based only on model text. Path canonicalization, symlink handling, environment filtering, subprocess inheritance and MCP side effects must be treated as security boundaries.
