#compdef helios agent-cli
_helios() {
  _arguments \
    '1:command:(init config auth doctor completion providers permissions mcp lsp hooks skills subagents tui status run exec resume models review sessions database audit jobs tasks agents eval perf self trust registry search context usage memory plugins verify diff)' \
    '--workspace[workspace root]:path:_files' \
    '--provider[provider adapter]:provider:(fake openai anthropic gemini generic openai-compatible)' \
    '--model[model name]:model:' \
    '--base-url[provider endpoint]:url:' \
    '--db-path[SQLite database]:path:_files' \
    '--config[workspace runtime config]:path:_files' \
    '--registry[plugin registry path or HTTPS URL]:source:' \
    '--trust-store[public key trust store]:path:_files' \
    '--resume[session id or latest]:selector:' \
    '--image[image file inside the workspace]:path:_files' \
    '--session[session id or latest]:selector:' \
    '--context-file[bounded workspace context file]:path:_files' \
    '--memory-query[bounded workspace memory query]:query:' \
    '--json[JSON output]' \
    '--output-format[headless output format]:format:(text json stream-json)' \
    '--query[FTS query]:query:' \
    '--allow-network[allow configured MCP network host]' \
    '--file[LSP workspace file]:path:_files' \
    '--root[skills root]:path:_files' \
    '--objective[subagent objective]:text:' \
    '--scope[subagent scope]:path:_files' \
    '--worktree[request worktree isolation]' \
    '--keep-worktree[keep exact manager-owned worktree]' \
    '--ref[explicit worktree base ref]:ref:' \
    '--concurrency[subagent concurrency]:count:' \
    '--max-iterations[subagent iteration budget]:count:' \
    '--max-tokens[subagent token budget]:count:' \
    '--max-tool-calls[subagent tool budget]:count:' \
    '--wall-clock[subagent wall clock seconds]:count:' \
    '*:prompt:_message'
}
_helios "$@"
