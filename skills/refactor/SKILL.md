---
name: refactor
description: Perform behavior-preserving structural changes with impact analysis and staged validation.
auto_invoke: true
risk: medium
---

# Refactor Workflow

1. State the invariant behavior that must remain unchanged.
2. Identify callers, implementations, tests and serialized/public interfaces.
3. Stage changes so intermediate failures are understandable.
4. Avoid unrelated formatting or renaming.
5. Run targeted and then broader regression checks.
6. Compare final public behavior/API surface against the starting state.
