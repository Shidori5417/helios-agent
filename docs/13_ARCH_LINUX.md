# Arch Linux Development and Distribution

## Development dependencies

Recommended packages:

```text
base-devel
rustup
git
ripgrep
sqlite
pkgconf
openssl (only if a dependency requires it; prefer rustls for HTTP)
```

Optional:

```text
clang
lld
mold
just
cargo-nextest
```

Use `rustup` for toolchain consistency rather than depending on whichever compiler happens to be current in the system repo.

## Build profiles

Development: fast compile, debug symbols.
Release: LTO only if benchmarks justify build-time cost; strip symbols for distribution artifact while publishing separate debug symbols if needed.

Test `mold`/`lld` for developer link speed, but do not make a niche linker mandatory for users.

## Packaging

Provide:

- prebuilt x86_64 Linux binary
- optional aarch64 Linux binary
- Arch PKGBUILD/AUR package after the CLI stabilizes
- source build via Cargo

Avoid a runtime Node/Python dependency for the core binary. MCP servers/plugins may independently require them.

## XDG paths

Follow XDG:

```text
$XDG_CONFIG_HOME/<agent>/config.toml
$XDG_DATA_HOME/<agent>/state.db
$XDG_CACHE_HOME/<agent>/...
$XDG_STATE_HOME/<agent>/logs/
```

Do not scatter state across `$HOME` unnecessarily.

## Linux sandbox

Design a backend abstraction. On Linux evaluate namespace/seccomp/Landlock/bubblewrap strategies, but keep policy semantics independent from the implementation so fallback modes remain explicit.

Never claim the sandbox is strong until escape tests exist.
