# Zuno

> An independent AI coding agent. Zuno keeps [`opencode`](https://github.com/sst/opencode) plugin
> ABI integration, but binary, configuration, and session compatibility are not product goals.

[简体中文](../../README.md) · English

## Contents

- [Project identity](#project-identity)
- [Install](#install)
- [Quick start](#quick-start)
- [Documentation](#documentation)
- [Independent runtime and plugin ABI](#independent-runtime-and-plugin-abi)
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

Zuno reads only its own configuration and data roots. `zuno session export` and `zuno session
import` close Zuno's own round trip: `import` accepts only a local document that `export` produced,
never an opencode session and never a share URL.

## Documentation

| Page                                               | Purpose                                                                  |
| -------------------------------------------------- | ------------------------------------------------------------------------ |
| [Compatibility matrix](../compatibility-matrix.md) | Implemented, added, rejected, not-registered, and explicit 503 surfaces  |
| [Declared divergences](../divergences.md)          | Every intentional difference and its reason                              |
| [Rejected inputs](../rejected-inputs.md)           | Deprecated configuration forms, replacements, and exact errors           |
| [Database lifecycle](../migration.md)              | Zuno database selection, legacy-filename diagnostics, and schema changes |
| [Session retention](../session-retention.md)       | Reversible archive and irreversible delete operations                    |
| [Plugin authoring](../plugin-authoring.md)         | The three plugin tiers and a Rust example                                |
| [Performance methodology](../perf-methodology.md)  | How memory and liveness gates are measured                               |

Only regions delimited by `generated:BEGIN` and `generated:END` comments are generated from code and
checked byte-for-byte by `cargo test -p oc-cli --test docs`; the test also derives assertions for a
small set of critical sections. Explanatory tables and prose outside those markers still require
review. Use `OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs` to regenerate managed regions.

## Independent runtime and plugin ABI

Zuno uses `$XDG_CONFIG_HOME/zuno`, project `.zuno` directories, and `$XDG_DATA_HOME/zuno`. It never
falls back to the corresponding opencode roots, and it provides no way to adopt an opencode session:
`zuno session import` reads Zuno's own `export` documents only. Old roots appear only in
upstream-only fixtures, source notes, or historical evidence.

The plugin tier is the sole retained compatibility layer. `COMPATIBILITY_VERSION = "1.18.13"`
continues to satisfy npm `engines.opencode` checks. The six plugin-ABI names also remain unchanged:
`OPENCODE_CLIENT`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_CONFIG_DIR`,
`OPENCODE_DISABLE_CLAUDE_CODE`, `OPENCODE_SERVER_PASSWORD`, and `OPENCODE_SERVER_USERNAME`.
They identify the plugin contract, not Zuno itself.

That decision has since been taken. The whole-surface differential suites that byte-compared
Zuno's output against a released `opencode` binary are gone, and the success criteria that
required them are retired. What remains is named, per surface, and is a verification asset rather
than a product promise: `cargo test -p oc-cli --test cli_parity` still compares every implemented
command's normalized output against the pinned release, and `crates/oc-cli/tests/rollback.rs` plus
`crates/oc-testkit/tests/session_interop.rs` still drive one session through both real programs,
printing a visible `SKIPPED` when that release is absent. Their presence does not make cross-binary
compatibility a product goal or justify adding legacy-path fallback or adoption of opencode
sessions.

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
