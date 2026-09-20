# Anthropic/Claude Provider Overlay

Use Anthropic's trained-in standard development tool schemas when available. For large tool catalogs, prefer tool search/deferred loading. Use programmatic tool calling only for workloads where batching/filtering intermediate results reduces round trips; do not force it for simple sequential calls.

Place stable tool/system content under prompt caching where supported. Keep always-on project instructions short. Use isolated subagents for noisy exploration whose details are not needed in the main context.
