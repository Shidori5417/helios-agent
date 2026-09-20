_agent_cli() {
  local cur="${COMP_WORDS[COMP_CWORD]}"
  local opts="run sessions search context usage memory plugins verify diff --provider --model --base-url --db-path --resume --config --registry --trust-store --query --key --content --kind --importance --id --limit --session --json"
  if [[ "${COMP_WORDS[*]}" == *"--provider"* ]]; then
    COMPREPLY=($(compgen -W "fake openai anthropic gemini generic openai-compatible" -- "${cur}"))
  else
    COMPREPLY=($(compgen -W "${opts}" -- "${cur}"))
  fi
}
complete -F _agent_cli agent-cli
