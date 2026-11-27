## What and why

<!-- What does this change, and why? Link related issues. -->

## Checklist

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `bash scripts/check-completions.sh` (if CLI options changed)
- [ ] CHANGELOG `Unreleased` updated for user-visible changes
- [ ] No secrets or credentials included
