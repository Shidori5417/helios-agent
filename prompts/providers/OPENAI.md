# OpenAI/Codex Provider Overlay

Use model-native coding tool shapes when supported. Keep the system/tool prefix byte-stable across turns where possible. Do not reorder tools dynamically without reason. Represent environment changes as appended context rather than rewriting earlier stable segments when the provider semantics support that pattern.

Prefer native conversation compaction when supported by the selected route/model. Keep reasoning effort low/medium for ordinary edits and raise it only for tasks that need it according to evals.

The harness, not the prompt, enforces permissions.
