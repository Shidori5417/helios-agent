---
name: security-review
description: Review code changes against concrete trust boundaries and common application security failure modes.
auto_invoke: false
risk: low
---

# Security Review Workflow

1. Map trust boundaries and attacker-controlled inputs relevant to the change.
2. Review authorization before data access or mutation.
3. Check filesystem/process/network boundaries.
4. Check secret handling and logging.
5. Check parsing/deserialization/query construction.
6. Validate any claim with a plausible path through the code.
7. Rank concrete findings by impact and likelihood without exaggeration.
