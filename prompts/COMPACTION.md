# Local Compaction Prompt

Compress the session into durable execution state, not a narrative summary.

Preserve:
- user's active objective
- explicit constraints
- accepted architectural decisions
- files/symbols currently relevant
- changes already made
- tests/checks already run and their outcomes
- unresolved errors/blockers
- important memory IDs or source references
- exact next action if one is pending

Discard:
- redundant discussion
- successful command noise
- superseded hypotheses
- code that can simply be reread from disk

Be concise and factual. Do not create new decisions.
