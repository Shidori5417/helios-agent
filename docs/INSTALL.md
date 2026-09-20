# Installation

## Source build

Install the Arch development packages from `docs/13_ARCH_LINUX.md`, then run:

```sh
cargo build --release --locked
install -Dm755 target/release/agent-cli ~/.local/bin/agent-cli

## Verify a release artifact

Release artifacts include `SHA256SUMS`, an SPDX JSON SBOM, and a Sigstore
keyless bundle. Verify the checksum first, then verify the bundle with the
OIDC issuer used by GitHub Actions:

```bash
sha256sum --check SHA256SUMS
cosign verify-blob \
  --bundle agent-cli-x86_64-unknown-linux-gnu.bundle \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'https://github.com/.+/.github/workflows/release.yml@.*' \
  agent-cli-x86_64-unknown-linux-gnu
```

The workflows use the official `sigstore/cosign-installer@v4.1.0` and
`anchore/sbom-action@v0.24.0` release tags. They are intentionally not
invented commit pins; pinning these actions to independently verified commit
SHAs remains supply-chain debt to resolve before treating releases as fully
reproducible. No signing key or repository secret is used.
```

## Arch package

Copy `packaging/arch/PKGBUILD` to a clean build directory, replace its release URL and
`REPLACE_WITH_RELEASE_SHA256` value, then run `makepkg -si`. The template never pipes
network content into a shell and uses Cargo's locked dependency set.

## Shell completion

Source the matching file from `completions/` using the shell's normal completion path.
`scripts/check-completions.sh` checks that all CLI options stay represented.

## Uninstall

For a source install, remove the explicitly installed binary:

```sh
rm -f ~/.local/bin/agent-cli
```

For an Arch package, use `sudo pacman -R agent-cli`. User state under XDG directories
is intentionally not removed; delete it separately only after reviewing its contents.

## Plugin registry format

Plugins are discovered from directories containing `plugin.json`. The schema is versioned,
capabilities are allowlisted, hooks have bounded timeouts and typed fail-open/fail-closed
policies, and arbitrary plugin code is not executed by the current runtime. See
`examples/plugins/sample/plugin.json` and `crates/plugins/src/lib.rs`.
