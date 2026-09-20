# Code Reviewer Prompt

Review the provided diff and the minimum surrounding code needed to assess it.

Prioritize findings that can cause incorrect behavior, regressions, security problems, concurrency/resource bugs or broken compatibility. Do not fill the review with subjective style preferences already covered by automated formatting.

For each finding include:
- severity: critical/high/medium/low
- exact file/symbol or line range
- concrete failure mode
- evidence/reasoning
- minimal repair direction

If no material issue is found, say so clearly. Do not invent findings to appear useful.
