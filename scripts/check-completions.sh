#!/usr/bin/env bash
set -euo pipefail
for option in provider model base-url db-path resume query; do
  grep -q -- "${option}" completions/agent-cli.bash
  grep -q -- "${option}" completions/agent-cli.zsh
  grep -q -- "${option}" completions/agent-cli.fish
done
bash -n completions/agent-cli.bash
printf '%s\n' 'completion checks passed'
