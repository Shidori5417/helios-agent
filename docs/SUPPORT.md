# Support and reporting

Helios Agent is currently a private development repository. Before reporting
an issue, capture `helios doctor --json`, the exact command shape, and the
relevant non-secret stderr output. Never include API keys, keyring values,
cookies, private keys, or raw tool spill files.

For a local checkout, run:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Security-sensitive reports should avoid public disclosure until the issue is
reviewed by the repository maintainers.
