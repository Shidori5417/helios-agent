complete -c agent-cli -l provider -r -f -a 'fake openai anthropic gemini generic openai-compatible'
complete -c agent-cli -l model -r -f
complete -c agent-cli -l base-url -r -f
complete -c agent-cli -l db-path -r -f
complete -c agent-cli -l config -r -f
complete -c agent-cli -l resume -r -f
complete -c agent-cli -l query -r -f
complete -c agent-cli -l session -r -f
complete -c agent-cli -l json
complete -c agent-cli -f -a 'init config auth doctor completion providers permissions mcp lsp hooks skills subagents status run sessions search context usage memory plugins verify diff'
complete -c agent-cli -l force
complete -c agent-cli -l token-stdin
complete -c agent-cli -l registry -r -f
complete -c agent-cli -l trust-store -r -f
