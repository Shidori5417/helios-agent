# Support and reporting

Helios Agent is a young, actively developed project. Before reporting
an issue on GitHub, capture `helios doctor --json`, the exact command shape, and the
relevant non-secret stderr output. Never include API keys, keyring values,
cookies, private keys, or raw tool spill files.

For a local checkout, run:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Security-sensitive reports must not be filed as public issues; follow
[SECURITY.md](../SECURITY.md).
