<div align="center">

<img src="./docs/assets/zuno-logo.svg" alt="Zuno logo" width="160" />

# Zuno

### A Rust coding agent for durable, bounded work

[![CI](https://github.com/sunerpy/zuno/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/zuno/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/zuno)](https://github.com/sunerpy/zuno/releases)
[![License](https://img.shields.io/badge/license-MIT-green)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98%2B-orange)](./rust-toolchain.toml)

[Install](#install) · [Quick start](#quick-start) · [Runtime](#runtime-and-extensions) · [Documentation](#documentation)

[**English**](./README.md) · [简体中文](./docs/readme/README.zh-CN.md)

</div>

Zuno is a local coding agent with a built-in terminal interface, headless execution,
ACP support, and HTTP serving. It stores sessions in SQLite and runs as a native Rust
binary; Node and Python are not part of the runtime.

The project is in active early-stage 0.x development. Zuno defines its own
configuration, commands, data formats, tool arguments, and extension protocol.

## Why Zuno

- **Work survives interruption.** Prompts, tool results, retries, plans, and child-agent
  reports are durable session state. Reopening a session resumes recorded work.
- **Agent roles have fixed ceilings.** `plan` is read-only, `build` owns delivery, and
  `deep` handles difficult cross-cutting changes without recursive delegation.
- **Command authority is explicit.** Permissions, risk checks, and OS confinement are
  independent controls. Restricted modes fail closed when the requested sandbox cannot
  be deployed unless trusted policy explicitly allows native execution.
- **Providers stay replaceable.** OpenAI, Anthropic, Google, Bedrock, and
  OpenAI-compatible endpoints use native Rust transports.
- **One runtime serves every client.** TUI, headless, ACP, and HTTP surfaces consume the
  same commands, events, inbox, and projections.

## Install

Release installers download the platform archive and verify it against the release
`SHA256SUMS` before extraction.

```sh
# Linux and macOS
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

To build the current Git revision from source:

```sh
cargo install --git https://github.com/sunerpy/zuno zuno --locked
```

`rg` (ripgrep) 14 or newer is required only when the `glob` or `grep` tool is used;
it is not a Zuno startup or core-runtime dependency. Bubblewrap 0.8.0 or newer is
required only for confined `read-only` and `workspace-write` Shell execution on
Linux. Explicit `danger-full-access` runs natively, as can an eligible, trusted
`workspace-write` `run-unconfined` fallback. macOS and Windows currently use those
native paths because no confined backend is implemented there. See
[Installation](./docs/guide/installation.md) for platform tools, configuration
paths, release targets, checksum verification, and source-build prerequisites.

An installed release can update itself:

```sh
zuno self-update --check
zuno self-update
```

Install completion for the current user without editing a shell profile:

```sh
zuno completion zsh --install
```

The complete update contract is documented in
[Self-update](./docs/reference/self-update.md).

## Quick start

Zuno does not assume a provider or model. Start from the checked `myopenai` example:

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
install -m 600 examples/config/zuno.json \
  "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
zuno debug config
zuno models myopenai --verbose
```

On Windows PowerShell the default configuration path is
`$HOME\.config\zuno\zuno.json`:

```powershell
$config = Join-Path $HOME ".config\zuno"
New-Item -ItemType Directory -Force -Path $config | Out-Null
Copy-Item examples\config\zuno.json (Join-Path $config "zuno.json")
notepad (Join-Path $config "zuno.json")
$env:MYOPENAI_API_KEY | zuno providers login --provider myopenai
zuno debug config
zuno models myopenai --verbose
```

The example uses the native `openai` transport. For a prebuilt installation without a
source checkout, copy the contents of
[`examples/config/zuno.json`](./examples/config/zuno.json) into the same config path.

On Linux with working confinement, verify the full path with a read-only run:

```sh
zuno run --agent plan "summarize this repository's architecture"
```

On macOS or Windows, or on a trusted Linux host without confinement, use the explicit
native path documented in [Quick start](./docs/guide/quick-start.md).

Then start the terminal application or run a bounded task directly:

```sh
zuno
zuno run "add pagination to the users endpoint and run the tests"
```

See [Quick start](./docs/guide/quick-start.md) for provider configuration, sandbox
checks, credentials, and first-run diagnostics.

## Runtime and extensions

Zuno's runtime is composed from typed Rust `Component`s. A `HarnessProfile` mounts
components transactionally; each registration returns the disposer that removes the
exact effect it created. `AgentDriver` controls the loop, while `ToolManifest` controls
the model-visible tool surface.

```rust
let profile = zuno_harness::profile_with_tools(
    "release-review",
    Arc::new(ReleaseReviewDriver::new()),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep, BuiltinSlot::Task])?,
    ToolContributions::default(),
);
```

Agents, workflows, Skills, WASI components, and contained process tools use the same
profile lifecycle. The assembled model request is persisted as
`session.prompt.assembled` before it is sent to a provider.

Read [Harness Runtime](./docs/harness-runtime.md) for the component model and
[Plugins and extensions](./docs/plugins.md) for package formats and capability grants.
Use [Developing agents and extensions](./docs/guide/extension-development.md) for
complete declarative Agent, WASI guest, and native Rust implementation paths.
The design record is in
[Harness comparison](./docs/design/harness-comparison.md), and shared client ownership
is described in
[Client interfaces](./docs/design/client-interfaces.md).

## Documentation

Full documentation is published at [zuno.firlab.app](https://zuno.firlab.app); the
source is in [`docs/`](./docs/README.md). Common starting points:

- [Quick start](./docs/guide/quick-start.md) — provider setup, credentials, first run
- [Configuration](./docs/reference/configuration.md) and
  [Providers](./docs/reference/providers.md)
- [Permissions and sandboxing](./docs/guide/permissions.md) — Shell authority
- [Attachments](./docs/reference/attachments.md) — images and `@file` input
- [Export and import](./docs/reference/portable-bundles.md) — portable config bundles
- [Zed ACP](./docs/reference/zed-acp.md) — editors and other ACP clients
- [Agent and extension development](./docs/guide/extension-development.md) — WASI and native Rust interfaces
- [Documentation coverage](./docs/design/documentation-coverage.md) — canonical ownership map
- [FAQ](./docs/faq.md) — troubleshooting

## Development

```sh
make build
./dist/zuno --version --long
make test-par
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

`make build` keeps Cargo's debug artifact in `target/debug` and stages the runnable
binary at `dist/zuno`. See [CONTRIBUTING.md](./CONTRIBUTING.md) for repository workflow
and required checks.

## License

Licensed under the [MIT License](./LICENSE).
