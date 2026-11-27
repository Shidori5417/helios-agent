# Security policy

Helios Agent runs model-driven tools (shell, file edits, MCP/LSP servers, plugins) on your machine and stores provider credentials in the system keyring, so we treat security reports seriously. The design assumptions are documented in [docs/11_SECURITY_THREAT_MODEL.md](docs/11_SECURITY_THREAT_MODEL.md).

## Reporting a vulnerability

Please report security issues **privately**. Do not open a public issue or pull request.

1. Preferred: GitHub private vulnerability reporting. On the repository page open **Security**, then **Report a vulnerability**.
2. If that button is not available, open a public issue titled "Security contact request" **without any technical details**, and a private channel will be arranged.

Include what you found, how to reproduce it, the affected commit or version, and the impact you expect. Never include real API keys, keyring values or private keys in a report.

You can expect an acknowledgement within a few days and a status update as the issue is investigated. Reporters are credited in the advisory unless they prefer to stay anonymous. Please allow reasonable time for a fix before public disclosure.

## Supported versions

There is no tagged release yet. Only the current `main` branch receives security fixes.

## Scope

In scope: escapes from the workspace sandbox (Landlock/seccomp), bypasses of tool permission and approval prompts, path traversal in file tools, plugin signature or capability-allowlist bypasses, leaks of credentials or tokens into logs, sessions or tool output, and unsafe handling of MCP/OAuth tokens.

Out of scope: issues that require an attacker who already controls the user account or the configured provider endpoint, and vulnerabilities in third-party model providers or MCP servers themselves (report those upstream).
