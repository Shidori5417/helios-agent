# Research Comparison — Codex, Claude Code, Antigravity CLI, Hermes

Research date: 2026-09-20. This file summarizes public documentation and engineering material; proprietary internals are not assumed.

| Area | Codex CLI | Claude Code | Antigravity CLI | Hermes Agent | Proposed CLI |
|---|---|---|---|---|---|
| Core orientation | coding harness | coding agent | unified agent harness terminal surface | general agent | coding-first harness |
| Runtime signal | open-source Rust implementation | product internals not fully public | Go CLI documented by Google | Python | Rust |
| Project instructions | AGENTS.md | CLAUDE.md/rules | AGENTS.md/skills | `.hermes.md`, AGENTS/CLAUDE context | AGENTS.md + scoped rules |
| Tools | shell/patch/plan + MCP/API tools | built-ins + MCP | tools + MCP/plugins | 40+ tools/toolsets + MCP | tiny core + lazy registry |
| Tool-token optimization | stable prefix/cache discipline | deferred tool schemas/tool search | harness/model co-optimization | toolsets/progressive skills | provider-aware deferred/gateway |
| Skills | supported | supported | supported | progressive disclosure | supported, progressive |
| Hooks | supported in current ecosystem | rich lifecycle hooks | supported | plugin hooks | deterministic lifecycle hooks |
| Subagents | supported | subagents/teams | subagents/async workflows | delegation/multi-agent | isolated, opt-in |
| Memory | session/project mechanisms | CLAUDE + auto/subagent memory | agent state/skills; platform features | strong persistent memory/provider architecture | bounded + FTS retrieval |
| Context compaction | first-class/native API options | compact/context management | harness-managed | compressor + pluggable engines | provider-native + local fallback |
| Sandbox | explicit sandbox/approval modes | permissions/sandbox | cross-platform sandboxing documented | local/container/remote backends | Linux workspace sandbox + policy |
| External integrations | MCP/apps/plugins | MCP/plugins/marketplaces | plugins/MCP | broad MCP/plugin ecosystem | MCP/plugin after core |

## Codex lessons

OpenAI's public agent-loop write-up shows why prompt caching is an architectural concern, not merely a billing feature. Exact prompt prefixes matter; tool ordering and mid-session configuration changes can create cache misses. Codex also treats context compaction as a core responsibility.

OpenAI's model guidance recommends harness/tool designs that match the model's training, including patch/shell/plan semantics. This is a strong argument for provider profiles instead of one generic prompt/tool schema for every model.

## Claude Code lessons

Claude Code's customization model clearly separates always-on instructions, skills, MCP, subagents and hooks. Its documentation recommends keeping always-loaded instructions small, while skills and MCP schemas are loaded progressively. Anthropic's API documentation provides explicit tool-search/deferred-loading and programmatic tool-calling mechanisms for controlling token growth.

## Antigravity lessons

Google moved its terminal experience to a Go-based Antigravity CLI that shares the harness with the wider Antigravity ecosystem. Public Google material emphasizes faster execution, asynchronous subagents, Skills, Hooks, plugins, MCP, sandboxing, credential masking and hardened Git policy. The Antigravity harness is described as co-optimized with Gemini models, reinforcing the model/harness co-design idea.

## Hermes lessons

Hermes is broader than a pure coding agent, but its memory/context/plugin architecture is particularly useful. Public docs describe bounded built-in memory, SQLite FTS5 session search, progressive skill disclosure, toolsets, MCP filtering, output spill, multiple terminal backends, pluggable memory providers and pluggable context engines.

## Decision

Do not fork all Hermes functionality into the core. Reuse concepts and, where code is copied, comply with its MIT license. Build a smaller coding-first Rust runtime, then add Hermes-like memory/provider extension points.

## Primary sources

- OpenAI, “Unrolling the Codex agent loop”: https://openai.com/index/unrolling-the-codex-agent-loop/
- OpenAI model guidance / Codex harness guidance: https://developers.openai.com/api/docs/guides/latest-model
- OpenAI Codex source: https://github.com/openai/codex
- Anthropic, tool context management: https://platform.claude.com/docs/en/agents-and-tools/tool-use/manage-tool-context
- Anthropic, tool search: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool
- Anthropic, programmatic tool calling: https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling
- Anthropic, Claude Code feature overview: https://code.claude.com/docs/en/features-overview
- Google, Antigravity CLI transition: https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/
- Google, Antigravity platform: https://developers.googleblog.com/build-with-google-antigravity-our-new-agentic-development-platform/
- Google Gemini caching: https://ai.google.dev/gemini-api/docs/caching
- Google Gemini 3.5 guidance: https://ai.google.dev/gemini-api/docs/whats-new-gemini-3.5
- Hermes tools: https://hermes-agent.nousresearch.com/docs/user-guide/features/tools
- Hermes memory: https://hermes-agent.nousresearch.com/docs/user-guide/features/memory
- Hermes skills: https://hermes-agent.nousresearch.com/docs/user-guide/features/skills
- Hermes context compression: https://hermes-agent.nousresearch.com/docs/developer-guide/context-compression-and-caching
- Hermes plugins: https://hermes-agent.nousresearch.com/docs/user-guide/features/plugins
