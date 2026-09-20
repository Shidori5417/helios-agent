# Model Routing and Provider-Specific Profiles

## Core rule

Do not pretend all models want the same harness. Share a provider-neutral protocol, but tune prompts, tools, reasoning level, caching and compaction per model family.

## OpenAI / Codex-family profile

Public OpenAI guidance for Codex-style models emphasizes the harness, exact/stable prompt prefixes, compaction, and tool interfaces such as shell/patch/plan.

Recommended approach:

- keep the initial instruction/tool prefix stable;
- preserve deterministic tool ordering;
- use the provider's preferred patch/shell shapes when available;
- default reasoning to the lowest level that passes evals, escalate on complex tasks;
- use provider-native compaction where supported;
- avoid switching models mid-session because it can invalidate cache advantages;
- append environment changes instead of rewriting earlier prompt sections where possible.

For large third-party tool catalogs, prefer a stable gateway/search abstraction unless a provider supports deferred tool definitions without destroying cache behavior.

## Anthropic / Claude profile

Anthropic exposes several mechanisms explicitly designed for context/tool efficiency.

Recommended approach:

- use trained-in bash/text-editor tool schemas where applicable;
- for 20+ tools, use tool search and deferred loading when supported;
- use programmatic tool calling for batches of many dependent data/tool operations when it actually reduces round trips;
- use prompt caching for stable tool/system prefixes;
- isolate noisy exploration in subagents;
- keep always-loaded project instructions concise;
- enforce hard rules in hooks, not only prose instructions.

Important: programmatic tool calling is not automatically cheaper for short sequential workflows; benchmark it by workload.

## Google Gemini / Antigravity-like profile

Google's Gemini 3.x guidance favors concise direct prompts and configurable thinking levels. Antigravity's harness is co-optimized with Gemini models.

Recommended approach:

- keep prompts concise; avoid legacy “think step by step” scaffolding;
- lower thinking level for straightforward edits and raise it for hard architecture/debugging;
- use stateful interactions when privacy settings permit and when it improves cache/reuse;
- maximize implicit cache hits with a shared prefix;
- combine/parallelize tool calls where supported;
- place long data context before the final specific question/instruction as recommended by Gemini guidance;
- use explicit/implicit context caching for repeatedly reused large corpora when appropriate.

## Generic OpenAI-compatible/local profile

Assume less tool-shape training.

- use clear, compact schemas;
- avoid dozens of tools at once;
- keep a small core set: read/search/patch/shell/plan;
- use local tool-search + generic MCP gateway for long catalogs;
- give explicit error-recovery instructions;
- use lower context budgets because local models may degrade earlier even if the nominal window is large.

## Router policy

Prefer session-level routing over turn-level model hopping.

```text
fast/simple edit        -> fast coding-capable model
normal feature/bugfix   -> default frontier coding model
hard debugging/refactor -> high reasoning profile
review-only             -> review profile
large independent scan  -> subagent on cheaper model, return summary
```

Use deterministic complexity signals before spending a model call on routing: number of referenced files, repo size, error type, requested operation, expected write radius and whether external tools are required.

## Capability registry

Each model profile should declare:

- context window
- reasoning controls
- prompt-cache behavior
- native compaction
- parallel tool calling
- deferred tool loading/tool search
- trained-in tool schemas
- structured output support
- streaming semantics
- max tool schema budget

Never hard-code provider behavior in the core loop.
