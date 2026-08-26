<div align="center">

# Zuno

### An independent Rust AI coding agent with a native Harness Runtime for composing drivers, capabilities, and tool sets

[![CI](https://github.com/sunerpy/zuno/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/zuno/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/zuno)](https://github.com/sunerpy/zuno/releases)
[![License](https://img.shields.io/badge/license-MIT-green)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98%2B-orange)](./rust-toolchain.toml)

[Install](#install) · [Quick start](#quick-start) · [Harness runtime](#harness-runtime) · [Documentation](#documentation) · [Development](#development)

[**English**](./README.md) · [简体中文](./docs/readme/README.zh-CN.md)

</div>

---

## Features

- **One static binary, no runtime dependency.** No Node, no Python, no dynamic plugin loader.
  `unsafe_code` is forbidden across the whole workspace.
- **Durable sessions.** Prompts, tool results, retries, and subagent reports are reconstructable
  from SQLite events, so a restart resumes rather than restarts.
- **Composable harness.** `AgentDriver` and `ToolManifest` are profile services, so the agent loop
  and tool surface are replaceable without patching a fixed main loop.
- **Native extensions.** Agents, slash-command workflows, skills, and executable tools ship as
  validated `zuno.extension/v1` packages — in-process WASI components or contained processes.
- **Pluggable providers.** OpenAI, Anthropic, Google, Bedrock, and OpenAI-compatible endpoints
  through native Rust transports.
- **Built-in TUI**, plus headless, ACP, and HTTP surfaces that all consume the same durable events.

## Project identity

Zuno is a standalone command-line AI coding agent: local session storage, pluggable model
providers, its own tool set, and a built-in TUI. `unsafe_code` is forbidden workspace-wide.

Zuno defines only its own configuration, data, commands, tool arguments, and extension protocols.
It does not retain the OpenCode plugin ABI, JavaScript hooks, HTTP compatibility routes, or
configuration shims. Extensions use the native [Harness runtime](#harness-runtime).

## Install

Supported platforms are Linux (x86_64, aarch64, static musl), macOS (Intel, Apple silicon), and
Windows x86_64. The installer downloads the archive for the running platform, verifies it against
the release's `SHA256SUMS`, and refuses to extract on a digest mismatch.

**Install script** — Linux and macOS. The tag in the URL pins both the script and the binary:

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/v0.1.0/scripts/install.sh | sh
```

**Install script** — Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/sunerpy/zuno/v0.1.0/scripts/install.ps1 | iex
```

Set `ZUNO_VERSION` to pin a different release and `ZUNO_INSTALL_DIR` to change the destination,
which defaults to `$HOME/.local/bin` (`%LOCALAPPDATA%\Programs\zuno` on Windows):

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/v0.1.0/scripts/install.sh \
  | ZUNO_VERSION=0.1.0 ZUNO_INSTALL_DIR=/usr/local/bin sh
```

**Prebuilt archives** — download and verify manually from
[GitHub Releases](https://github.com/sunerpy/zuno/releases) if you would rather not pipe a remote
script into a shell. Every release carries five archives and one `SHA256SUMS`.

**From source** — requires the pinned toolchain in [`rust-toolchain.toml`](./rust-toolchain.toml)
and a C compiler, because SQLite and the TLS stack are built from source:

```sh
cargo install --path crates/zuno-cli --locked
```

An installed Zuno can check and update itself in place:

```sh
zuno self-update --check
zuno self-update
```

The updater selects the exact platform archive, verifies it against the same release's
`SHA256SUMS`, and only then atomically replaces the running executable. Use `--tag v0.2.0` to pin
a release and `--yes` for non-interactive confirmation. See
[Self-update](./docs/reference/self-update.md) for the complete safety contract.

## Quick start

```console
$ zuno --help
$ zuno --version --long
```

Start first-time configuration from the checked native provider example. It uses the Rust `openai`
transport and neither installs Node packages nor loads an AI SDK:

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
install -m 600 examples/config/zuno.json "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
zuno debug config
zuno models myopenai --verbose
```

For a prebuilt installation without a source checkout, create the contents of
[`examples/config/zuno.json`](./examples/config/zuno.json) directly at the same configuration
path. Provider configuration accepts native `transport` values and has no `npm` field.

`zuno export` creates a cross-platform `.zuno-bundle` containing the resolved global Zuno
configuration and `$HOME/.zuno`: config, `AGENTS.md`, Agents, Skills, Markdown commands,
extensions, profiles, and other user assets. Session databases, transcripts, logs, caches, and
credentials are excluded by default; credentials require the explicit unencrypted
`--include-credentials` option. `zuno import` validates checksums and portable paths, supports
`--dry-run`, and requires `--replace` before transactionally replacing non-empty target roots.
See [Portable Zuno environment bundles](./docs/reference/portable-bundles.md).

In the TUI, pasting a supported local image path or clipboard image creates an `[Image #N]`
attachment. `@relative/path` adds bounded project text or images, and headless `zuno run -f`
accepts repeated files. See [Images and file references](./docs/reference/attachments.md).

## Harness runtime

Zuno's native extension unit is a Rust `Component`. Components form a `ProfileBundle`, and a
`HarnessProfile` mounts all bundles in one transaction: candidates are published only after full
validation, failures roll back in reverse order, and the old profile remains available during a
hot replacement. `AgentDriver` and `ToolManifest` are profile services, so benchmark, workflow,
remote, and specialized coding harnesses can replace the loop and tool surface without editing a
fixed main loop.

Session input uses a durable FIFO inbox shared by user prompts, live steering, and subagent
reports. In the TUI, `Enter` queues while an agent is active, `Ctrl+Enter` explicitly steers at
the next safe boundary, and `Shift+Enter` inserts a newline. `/plan` opens the user-confirmed
Plan/Work transition, `/start-plan` enters read-only planning directly, and `/start-work` requires
and confirms a durable plan before implementation. Background `task` calls support
`reportDelivery: nextStep | quiet`, while the `job` tool queries durable status. The right sidebar
tracks background terminals and agents live and points to `/ps` and `/subagent` for details.
`web_search` accepts a `queries` array, runs queries concurrently, cancels
sibling requests after the first failure, waits for settlement, and merges results deterministically
with URL deduplication. Active goals persist exponential backoff for network, rate-limit, truncated
stream, database-lock, and turn-budget failures, then reconstruct the deadline when the session is
reopened. Human input wins over automatic work. Tool failures become model-visible tool results and
are never replayed mechanically. Only read-only or idempotent tools that explicitly declare `Safe`
may be attempted by the model in a later turn; uncertain side effects require authoritative-state
inspection. Authentication, cancellation, and permanent configuration failures pause or block.

`build`, `plan`, `deep`, and the specialist agents share one native catalog. A provider-neutral
`PromptEnvelope` keeps kernel, agent role, collaboration mode, policy, AGENTS, work state, Skill,
and memory blocks separate until provider encoding. OpenAI Responses maps kernel and agent role to
`instructions`, keeps the other blocks as developer inputs, and preserves the user message
unchanged. Every receipt is persisted as `session.prompt.assembled`; inspect the latest redacted
receipt with `zuno debug prompt`. A visible, uniquely named Skill can be loaded directly as
`/<skill-name>`; arguments run it only after the exact source has loaded. See
[the Harness Runtime guide](./docs/harness-runtime.md).

## Extending agents and workflows

Put a custom agent under `.zuno/agent/`. Its path supplies the default name, frontmatter selects
model, mode, permissions, and steps, and the body becomes a traced prompt section:

```markdown
---
description: Review a change for security and authorization defects
mode: subagent
permission:
  "*": deny
  read: allow
  glob: allow
  grep: allow
  lsp: allow
  webfetch: allow
  web_search: allow
  shell: ask
---

Inspect repository files, relevant environment facts, current external evidence,
trust boundaries, permission checks, durable state, and failure behavior.
Return findings with exact file locations.
```

An agent may also declare a `zuno.extension/v1` package containing agents, slash-command workflows,
and skills. `extension_define` records an immutable definition only in the current process,
`extension_run` activates it, and `extension_stop`, `extension_undefine`, and
`extension_inspect` manage its lifecycle. The TUI recomposes the next turn inside the same process;
exiting Zuno loses process-local definitions.

For restart-persistent loading, write the same manifest to
`.zuno/extensions/<id>/extension.json` (or
`~/.config/zuno/extensions/<id>/extension.json`) and restart. Both lifetimes use one validator and
collision checker. Extension agents with mode `subagent` or `all` join the real
`task` target roster and retain their model, prompt, and native tool permissions
inside the child session. File access comes from `read`/`glob`/`grep`/`lsp`/`edit`,
network access from `webfetch`/`web_search`, and environment or ordinary process
access from permission-governed `shell`. Strict authorization still applies fresh
HITL to every side effect.

Static packages may also register executable tools. Prefer an in-process WASI
component with explicit workspace, network, and environment grants plus fuel,
memory, and wall-time bounds. Use a contained `host.full` process only when the
plugin needs unrestricted host APIs. Both are profile effects: unload withdraws
routing before reverse asynchronous cleanup, and an unprovable stop becomes
`Uncertain` without replay. Zuno neither evaluates the JavaScript/Cordis ABI nor
loads Rust dynamic libraries.
`host.full`, WASI `network`, and WASI `workspace.write` tools cannot claim
read-only or safe-replay policy, so a manifest cannot use those grants to bypass
strict authorization.

```sh
zuno plugin add examples/plugins/review-kit --project
zuno plugin list
zuno plugin update examples/plugins/review-kit --project
zuno plugin remove review-kit --project
```

See [the plugin guide](./docs/plugins.md) for manifests, capability tables,
WIT/JSON-RPC protocols, and custom agent, workflow, WASI, and process examples.

A native workflow does not modify the default loop. Implement `AgentDriver`, select a
model-visible `ToolManifest`, contribute any native tools, and activate the result as one
transactional profile:

```rust
let profile = zuno_harness::profile_with_tools(
    "release-review",
    Arc::new(ReleaseReviewDriver::new()),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep, BuiltinSlot::Task])?,
    ToolContributions::new([Arc::new(ReleaseSummaryTool::new())])?,
);

runtime.activate_profile(profile).await?;
```

Profiles can also mount providers, remote executors, approvals, evaluations, or benchmark
components. Each registration returns its exact disposer; a candidate profile is published only
after complete validation and otherwise rolls back in reverse order. The same extension works
through TUI, headless, ACP, HTTP, and future GUI clients because they consume shared commands,
durable events, the inbox, and projections. See the
[harness comparison](./docs/design/harness-comparison.md) and
[client interface architecture](./docs/design/client-interfaces.md).

## Documentation

| Page                                                        | Purpose                                                                  |
| ----------------------------------------------------------- | ------------------------------------------------------------------------ |
| [Self-update](./docs/reference/self-update.md)              | Release selection, SHA-256, authentication, proxies, and atomic replace  |
| [Harness Runtime](./docs/harness-runtime.md)                | Native components, profile transactions, durable input, custom harnesses |
| [Plugins](./docs/plugins.md)                                | Installation, agents/workflows, WASI/process grants, and protocols       |
| [Harness comparison](./docs/design/harness-comparison.md)   | Decisions from DSH, Codex, OMO, pi-agent, OpenCode, and Claw Code        |
| [Client interfaces](./docs/design/client-interfaces.md)     | Shared events and projections for TUI, ACP, HTTP, and a future GUI       |
| [Zed ACP integration](./docs/design/zed-acp-integration.md) | Stable pins, Zed setup, HITL, diffs, replay, and acceptance tests        |
| [Memory learning](./docs/design/memory-learning.md)         | Auditable candidates, reflection, review, promotion, and undo            |
| [Operational logging](./docs/logging.md)                    | Multi-process store, filters, redaction, retention, and plaintext debug  |
| [Database lifecycle](./docs/migration.md)                   | Zuno database selection and schema changes                               |
| [Session retention](./docs/session-retention.md)            | Reversible archive and irreversible delete operations                    |
| [Resource gates](./docs/resource-gates.md)                  | Measured results for the six gates, opt-in commands, and known limits    |
| [Performance methodology](./docs/perf-methodology.md)       | How memory and liveness gates are measured                               |

`cargo test -p zuno-cli --test docs` checks that the Harness guide covers runtime lifecycle,
durable delivery, and concurrent search, and prevents the READMEs from advertising retired
compatibility surfaces.

## Independent runtime

Zuno uses `$XDG_CONFIG_HOME/zuno`, project `.zuno` directories, and `$XDG_DATA_HOME/zuno`. Other
products' roots and files are not Zuno inputs and are not probed, migrated, or interpreted.

The config **filename** is Zuno's own too: every layer reads `zuno.jsonc` and `zuno.json` and
nothing else — the config root, a bare file on the walk up from the working directory to the
worktree root, `.zuno/`, the directory named by `ZUNO_CONFIG_DIR`, and the managed directory.
JSONC and strict JSON only; there is **no TOML config path**. Other filenames are ordinary files in
the directory and never enter Zuno's configuration graph.

Zuno's user interface, default paths, environment variables, and extension protocol all use Zuno's
identity.

## Development

```sh
make build
./dist/zuno --version --long
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
make hooks
```

`make build` retains Cargo's debug build and atomically stages a directly runnable binary at
`dist/zuno`; Cargo's original remains at `target/debug/zuno`. `make release` replaces `dist/zuno`
with the optimized build while retaining its original at `target/release/zuno`.

`make hooks` installs commit-time formatting and a fast push-time test gate; the full workspace
suite remains an explicit `make test` and CI gate. The resource gates need explicit opt-in — see
[resource gates](./docs/resource-gates.md).

## License

Licensed under the [MIT License](./LICENSE).
