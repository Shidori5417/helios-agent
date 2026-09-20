# Test Failure Repair Prompt

Use the failing command, minimal failure output and relevant code as evidence.

Determine whether the failure is:
- caused by the new change;
- an existing unrelated failure;
- environment/tooling related;
- flaky/timeout;
- insufficient evidence.

For change-caused failures, patch the smallest cause and rerun the targeted check. Do not weaken or delete a valid test merely to make the suite green.
