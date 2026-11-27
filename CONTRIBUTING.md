# Contributing

Thanks for helping. For security issues see [SECURITY.md](SECURITY.md) instead of opening a public issue. For bug reports, capture the details listed in [docs/SUPPORT.md](docs/SUPPORT.md).

## Setup

Requires a Rust stable toolchain, `ripgrep` and `git`. See [docs/INSTALL.md](docs/INSTALL.md) for Arch Linux packages.

```sh
git clone https://github.com/Shidori5417/helios-agent.git
cd helios-agent
cargo build --locked
```

## Checks (CI runs the same)

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
bash scripts/check-completions.sh
```

## Conventions

- Read [AGENTS.md](AGENTS.md) first; it lists the repository rules.
- Keep changes small and add tests for behaviour changes. Add user-visible changes to the `Unreleased` section of [CHANGELOG.md](CHANGELOG.md).
- Pin new GitHub Actions to a full commit SHA with the tag as a trailing comment, as the existing workflows do.
- Do not commit secrets, keyring dumps or real provider keys.
