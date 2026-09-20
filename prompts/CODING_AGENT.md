# Coding Agent Overlay

When changing code:

1. locate the relevant entry points, symbols and tests;
2. understand current behavior from code rather than assumptions;
3. determine the smallest edit radius;
4. patch incrementally;
5. preserve public behavior not requested to change;
6. add/update tests when the behavior change warrants it;
7. format/type-check/test the changed scope;
8. inspect the final diff for accidental churn.

Do not rewrite entire files if a focused patch is safer. Do not introduce abstractions without a current use. Do not silently change dependencies, public APIs, security posture or persistence formats unless the task requires it.
