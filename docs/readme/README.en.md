# Zuno

> A Rust implementation of [`opencode`](https://github.com/sst/opencode), pinned to compatibility
> baseline **1.18.13**.

[简体中文](../../README.md) · English

## Contents

- [Project identity](#project-identity)
- [Install](#install)
- [Quick start](#quick-start)
- [Documentation](#documentation)
- [Run beside the TypeScript binary](#run-beside-the-typescript-binary)
- [Development](#development)
- [Resource gates](#resource-gates)
- [License](#license)

## Project identity

`zuno --version` reports `1.18.13`. This is the npm-plugin compatibility version, not the build
identity: plugins use the running version as a semver constraint and skip loading when it does not
match. Use the long form when you need the package version:

```console
$ zuno --version
1.18.13
$ zuno --version --long
Zuno 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
```

Keeping these identities separate is the declared `split-version-identity` divergence.

## Install

The repository is private, so authenticate with `gh auth login` first. On Linux and macOS, the
following command reads the installer through the authenticated GitHub CLI, passes its token to the
release download, and installs the matching archive into `$HOME/.local/bin`:

```sh
GH_TOKEN="$(gh auth token)" sh -c "$(gh api -H 'Accept: application/vnd.github.raw+json' repos/sunerpy/zuno/contents/scripts/install.sh)"
```

Set `ZUNO_VERSION` to pin a release or `ZUNO_INSTALL_DIR` to change the destination. You can also
download a prebuilt archive from [GitHub Releases](https://github.com/sunerpy/zuno/releases), or
build from source with `cargo install --path crates/oc-cli --locked`.

## Quick start

```console
$ zuno --version
1.18.13
$ zuno --help
```

Zuno does not automatically read or change your existing opencode data. Follow the migration guide
before testing against copied data.

## Documentation

| Page                                               | Purpose                                                                 |
| -------------------------------------------------- | ----------------------------------------------------------------------- |
| [Compatibility matrix](../compatibility-matrix.md) | Implemented, added, rejected, not-registered, and explicit 503 surfaces |
| [Declared divergences](../divergences.md)          | Every intentional difference and its reason                             |
| [Rejected inputs](../rejected-inputs.md)           | Deprecated configuration forms, replacements, and exact errors          |
| [Migration](../migration.md)                       | Existing databases, channel database selection, and rollback            |
| [Session retention](../session-retention.md)       | Reversible archive and irreversible delete operations                   |
| [Plugin authoring](../plugin-authoring.md)         | The three plugin tiers and a Rust example                               |
| [Performance methodology](../perf-methodology.md)  | How memory and liveness gates are measured                              |

Documentation tables are derived from code. Run `cargo test -p oc-cli --test docs` to detect drift,
or `OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs` to regenerate them.

## Run beside the TypeScript binary

Zuno uses separate configuration and data roots. It never falls back to the TypeScript binary's
directories. To test copied data safely:

1. Copy the original config and data into the Zuno roots.
2. Back up the copied `opencode.db` before any forward-only migration.
3. Run `zuno debug paths` and `ZUNO_DISABLE_CHANNEL_DB=1 zuno session list`.
4. Keep using `opencode` against its untouched original directories.

See the [migration guide](../migration.md) for exact commands, database precedence, and rollback.

## Development

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
make hooks
```

`unsafe_code` is forbidden workspace-wide. `make hooks` installs commit-time formatting and a fast
push-time test gate; the full workspace suite remains an explicit `make test` and CI gate.

## Resource gates

The Chinese root README is the canonical, test-generated owner of the current memory measurements:
[read the measured G1/G2 section](../../README.md#g1-与-g2--峰值常驻内存). Repeating those figures
here would create an unguarded translated snapshot. The formulas, pinned inputs, and reproduction
steps live in the [performance methodology](../perf-methodology.md).

## License

Licensed under the [MIT License](../../LICENSE).
