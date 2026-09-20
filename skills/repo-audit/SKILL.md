---
name: repo-audit
description: Build a compact architecture and quality map of an unfamiliar repository without reading it wholesale.
auto_invoke: true
risk: low
---

# Repository Audit

1. Detect languages/manifests and top-level structure.
2. Identify build, test, lint and format commands.
3. Search for application entry points and core domain modules.
4. Identify persistence, auth, network and configuration boundaries when present.
5. Use symbol/LSP queries for high-fanout components.
6. Return a concise map with risks and unknowns.

Do not dump file contents or read every file. Follow evidence paths relevant to the user's request.
