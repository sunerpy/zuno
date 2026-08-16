# Zuno

> An independent AI coding agent. Written in Rust, it loads opencode plugins, but binary,
> configuration, and session compatibility are not product goals.

[简体中文](../../README.md) · English

## Contents

- [Project identity](#project-identity)
- [Install](#install)
- [Quick start](#quick-start)
- [Plugins](#plugins)
- [Documentation](#documentation)
- [Independent runtime](#independent-runtime)
- [Development](#development)
- [License](#license)

## Project identity

Zuno is a standalone command-line AI coding agent: local session storage, pluggable model
providers, its own tool set, and a built-in TUI. `unsafe_code` is forbidden workspace-wide.

It reads its own configuration and data, not opencode's. Cross-binary compatibility is not a
goal — the plugin tier is the only retained compatibility surface, described under
[Plugins](#plugins).

## Install

The repository is private, so authenticate with `gh auth login` first. On Linux and macOS, the
following command reads the installer through the authenticated GitHub CLI, passes its token to the
release download, and installs the matching archive into `$HOME/.local/bin`:

```sh
GH_TOKEN="$(gh auth token)" sh -c "$(gh api -H 'Accept: application/vnd.github.raw+json' repos/sunerpy/zuno/contents/scripts/install.sh)"
```

Set `ZUNO_VERSION` to pin a release or `ZUNO_INSTALL_DIR` to change the destination. You can also
download a prebuilt archive from [GitHub Releases](https://github.com/sunerpy/zuno/releases), or
build from source with `cargo install --path crates/zuno-cli --locked`.

## Quick start

```console
$ zuno --help
$ zuno --version --long
```

`zuno export` and `zuno import` close Zuno's own round trip: `import` accepts only a local document
that `export` produced, never an opencode session and never a share URL. Both are **top-level**
commands, not subcommands of `session` — `zuno session` carries only `list`, `prune`, and `delete`.

## Plugins

Zuno supports opencode plugins: an installed npm plugin loads against its existing ABI, including
the six handshake environment variables `OPENCODE_CLIENT`, `OPENCODE_CONFIG_CONTENT`,
`OPENCODE_CONFIG_DIR`, `OPENCODE_DISABLE_CLAUDE_CODE`, `OPENCODE_SERVER_PASSWORD`, and
`OPENCODE_SERVER_USERNAME`. They identify the plugin contract, not Zuno itself.

The JavaScript plugin runtime is off by default and must be enabled explicitly
(`ZUNO_ENABLE_JS_PLUGINS=1`, or `"plugin_runtime": {"javascript": true}`). Not starting a JS
runtime by default cuts startup from roughly 1465 ms to roughly 30 ms.

**Rust plugins are the recommended way to write a new one.** The first-party SDK is
`zuno-plugin-sdk`: in-process, no runtime dependency, and shipped with a conformance suite. The
three plugin tiers, the 21 hooks, and a runnable Rust example are in
[plugin authoring](../plugin-authoring.md).

## Documentation

| Page                                               | Purpose                                                                  |
| -------------------------------------------------- | ------------------------------------------------------------------------ |
| [Plugin authoring](../plugin-authoring.md)         | The three plugin tiers, the hook table, and a Rust example               |
| [Compatibility matrix](../compatibility-matrix.md) | Implemented, added, rejected, not-registered, and explicit 503 surfaces  |
| [Declared divergences](../divergences.md)          | Every intentional difference and its reason                              |
| [Rejected inputs](../rejected-inputs.md)           | Deprecated configuration forms, replacements, and exact errors           |
| [Database lifecycle](../migration.md)              | Zuno database selection, legacy-filename diagnostics, and schema changes |
| [Session retention](../session-retention.md)       | Reversible archive and irreversible delete operations                    |
| [Resource gates](../resource-gates.md)             | Measured results for the six gates, opt-in commands, and known limits    |
| [Performance methodology](../perf-methodology.md)  | How memory and liveness gates are measured                               |

Only regions delimited by `generated:BEGIN` and `generated:END` comments are generated from code and
checked byte-for-byte by `cargo test -p zuno-cli --test docs`; the test also derives assertions for a
small set of critical sections. Explanatory tables and prose outside those markers still require
review. Use `ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs` to regenerate managed regions.

## Independent runtime

Zuno uses `$XDG_CONFIG_HOME/zuno`, project `.zuno` directories, and `$XDG_DATA_HOME/zuno`. It never
falls back to the corresponding opencode roots, and it provides no way to adopt an opencode session:
`zuno import` reads Zuno's own `zuno export` documents only. Old roots appear only in
upstream-only fixtures, source notes, or historical evidence.

Outside the plugin ABI, Zuno's user interface, default paths, and own environment variables all use
Zuno's identity.

## Development

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
make hooks
```

`make hooks` installs commit-time formatting and a fast push-time test gate; the full workspace
suite remains an explicit `make test` and CI gate. The resource gates need explicit opt-in — see
[resource gates](../resource-gates.md).

## License

Licensed under the [MIT License](../../LICENSE).
