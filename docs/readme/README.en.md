# Zuno

> An independent Rust AI coding agent with a native Harness Runtime for composing drivers,
> capabilities, and tool sets.

[简体中文](../../README.md) · English

## Contents

- [Project identity](#project-identity)
- [Install](#install)
- [Quick start](#quick-start)
- [Harness runtime](#harness-runtime)
- [Documentation](#documentation)
- [Independent runtime](#independent-runtime)
- [Development](#development)
- [License](#license)

## Project identity

Zuno is a standalone command-line AI coding agent: local session storage, pluggable model
providers, its own tool set, and a built-in TUI. `unsafe_code` is forbidden workspace-wide.

It reads its own configuration and data, not opencode's. Zuno does not retain the OpenCode plugin
ABI, JavaScript hooks, HTTP compatibility routes, or configuration shims. Extensions use the
native [Harness runtime](#harness-runtime).

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

## Harness runtime

Zuno's native extension unit is a Rust `Component`. Components form a `ProfileBundle`, and a
`HarnessProfile` mounts all bundles in one transaction: candidates are published only after full
validation, failures roll back in reverse order, and the old profile remains available during a
hot replacement. `AgentDriver` and `ToolManifest` are profile services, so benchmark, workflow,
remote, and specialized coding harnesses can replace the loop and tool surface without editing a
fixed main loop.

Session input uses a durable FIFO inbox shared by user prompts, live steering, and subagent
reports. Background `task` calls support `reportDelivery: nextStep | quiet`, while the `job` tool
queries durable status. `web_search` accepts a `queries` array, runs queries concurrently, cancels
sibling requests after the first failure, waits for settlement, and merges results deterministically
with URL deduplication. See [the Harness Runtime guide](../harness-runtime.md).

## Documentation

| Page                                              | Purpose                                                                  |
| ------------------------------------------------- | ------------------------------------------------------------------------ |
| [Harness Runtime](../harness-runtime.md)          | Native components, profile transactions, durable input, custom harnesses |
| [Rejected inputs](../rejected-inputs.md)          | Deprecated configuration forms, replacements, and exact errors           |
| [Database lifecycle](../migration.md)             | Zuno database selection, legacy-filename diagnostics, and schema changes |
| [Session retention](../session-retention.md)      | Reversible archive and irreversible delete operations                    |
| [Resource gates](../resource-gates.md)            | Measured results for the six gates, opt-in commands, and known limits    |
| [Performance methodology](../perf-methodology.md) | How memory and liveness gates are measured                               |

`cargo test -p zuno-cli --test docs` checks that the Harness guide covers runtime lifecycle,
durable delivery, and concurrent search, and prevents the READMEs from advertising retired
compatibility surfaces.

## Independent runtime

Zuno uses `$XDG_CONFIG_HOME/zuno`, project `.zuno` directories, and `$XDG_DATA_HOME/zuno`. It never
falls back to the corresponding opencode roots, and it provides no way to adopt an opencode session:
`zuno import` reads Zuno's own `zuno export` documents only. Old roots appear only in
upstream-only fixtures, source notes, or historical evidence.

The config **filename** is Zuno's own too: every layer reads `zuno.jsonc` and `zuno.json` and
nothing else — the config root, a bare file on the walk up from the working directory to the
worktree root, `.zuno/`, the directory named by `ZUNO_CONFIG_DIR`, and the managed directory.
JSONC and strict JSON only; there is **no TOML config path**. `opencode.jsonc`, `opencode.json`, and
a `config.json` in the config root are no longer read: a user still holding one of those names gets
a startup error naming the file, its directory, and the name to rename it to, rather than silence.

Zuno's user interface, default paths, environment variables, and extension protocol all use Zuno's
identity.

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
