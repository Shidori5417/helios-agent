_helios() {
  local cur="${COMP_WORDS[COMP_CWORD]}"
  local opts="init config auth doctor completion providers permissions mcp lsp hooks skills subagents tui status run exec sessions database audit jobs eval perf self trust registry search context usage memory plugins verify diff --workspace --provider --model --base-url --db-path --resume --config --registry --trust-store --query --key --content --kind --importance --id --limit --session --context-file --memory-query --json --output-format --force --token-stdin --output --input --confirm --confirm-workspace --event --stderr --tail --timeout --suite --baseline --max-regressions --runs --current --prefix --name --from --sha256 --compat-agent-cli --public-key-file --registry-id --source --allow-network --file --root --objective --scope --worktree --keep-worktree --ref --concurrency --max-iterations --max-tokens --max-tool-calls --wall-clock --tick-ms --channel-capacity --scroll-step --color"
  if [[ "${COMP_WORDS[*]}" == *"--provider"* ]]; then
    COMPREPLY=($(compgen -W "fake openai anthropic gemini generic openai-compatible" -- "${cur}"))
  else
    COMPREPLY=($(compgen -W "${opts}" -- "${cur}"))
  fi
}
complete -F _helios helios
complete -F _helios agent-cli
