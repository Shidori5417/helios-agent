#compdef agent-cli
_agent_cli() {
  _arguments \
    '1:command:(init config auth doctor completion providers permissions mcp lsp hooks skills subagents status run sessions database audit jobs eval perf self trust registry search context usage memory plugins verify diff)' \
    '--provider[provider adapter]:provider:(fake openai anthropic gemini generic openai-compatible)' \
    '--model[model name]:model:' \
    '--base-url[provider endpoint]:url:' \
    '--db-path[SQLite database]:path:_files' \
    '--config[workspace runtime config]:path:_files' \
    '--registry[plugin registry path or HTTPS URL]:source:' \
    '--trust-store[public key trust store]:path:_files' \
    '--resume[session id or latest]:selector:' \
    '--session[session id or latest]:selector:' \
    '--json[JSON output]' \
    '--query[FTS query]:query:' \
    '--allow-network[allow configured MCP network host]' \
    '--file[LSP workspace file]:path:_files' \
    '--root[skills root]:path:_files' \
    '--objective[subagent objective]:text:' \
    '--scope[subagent scope]:path:_files' \
    '--worktree[request worktree isolation]' \
    '--concurrency[subagent concurrency]:count:' \
    '--max-iterations[subagent iteration budget]:count:' \
    '--max-tokens[subagent token budget]:count:' \
    '--max-tool-calls[subagent tool budget]:count:' \
    '--wall-clock[subagent wall clock seconds]:count:' \
    '*:prompt:_message'
}
_agent_cli "$@"
