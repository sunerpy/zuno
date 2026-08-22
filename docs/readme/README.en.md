# Zuno

> An independent Rust AI coding agent with a native Harness Runtime for composing drivers,
> capabilities, and tool sets.

[简体中文](../../README.md) · English

## Contents

- [Project identity](#project-identity)
- [Install](#install)
- [Quick start](#quick-start)
- [Harness runtime](#harness-runtime)
- [Extending agents and workflows](#extending-agents-and-workflows)
- [Documentation](#documentation)
- [Independent runtime](#independent-runtime)
- [Development](#development)
- [License](#license)

## Project identity

Zuno is a standalone command-line AI coding agent: local session storage, pluggable model
providers, its own tool set, and a built-in TUI. `unsafe_code` is forbidden workspace-wide.

Zuno defines only its own configuration, data, commands, tool arguments, and extension protocols.
It does not retain the OpenCode plugin ABI, JavaScript hooks, HTTP compatibility routes, or
configuration shims. Extensions use the native [Harness runtime](#harness-runtime).

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
[`examples/config/zuno.json`](../../examples/config/zuno.json) directly at the same configuration
path. Provider configuration accepts native `transport` values and has no `npm` field.

`zuno export` and `zuno import` close Zuno's own round trip. Both are **top-level** commands, not
subcommands of `session`; `zuno session` carries only `list`, `prune`, and `delete`.

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
with URL deduplication. Active goals persist exponential backoff for network, rate-limit, truncated
stream, database-lock, and turn-budget failures, then reconstruct the deadline when the session is
reopened. Human input wins over automatic work. Tool failures become model-visible tool results and
are never replayed mechanically. Only read-only or idempotent tools that explicitly declare `Safe`
may be attempted by the model in a later turn; uncertain side effects require authoritative-state
inspection. Authentication, cancellation, and permanent configuration failures pause or block.

`build`, `plan`, `deep`, and the specialist agents share one native catalog. The final model prompt
is assembled as agent, policy, memory, extension, instruction, and skill sections, then persisted as
`session.prompt.assembled` with order, source, content digests, and the actual post-hook text. See
[the Harness Runtime guide](../harness-runtime.md).

## Extending agents and workflows

Put a custom agent under `.zuno/agent/`. Its path supplies the default name, frontmatter selects
model, mode, permissions, and steps, and the body becomes a traced prompt section:

```markdown
---
description: Review a change for security and authorization defects
mode: subagent
permission:
  edit: deny
  bash: deny
---

Inspect trust boundaries, permission checks, durable state, and failure behavior.
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
collision checker. They do not evaluate JavaScript/Cordis plugins or load Rust dynamic libraries.

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
[harness comparison](../design/harness-comparison.md) and
[client interface architecture](../design/client-interfaces.md).

## Documentation

| Page                                                  | Purpose                                                                  |
| ----------------------------------------------------- | ------------------------------------------------------------------------ |
| [Harness Runtime](../harness-runtime.md)              | Native components, profile transactions, durable input, custom harnesses |
| [Harness comparison](../design/harness-comparison.md) | Decisions from DSH, Codex, OMO, pi-agent, OpenCode, and Claw Code        |
| [Client interfaces](../design/client-interfaces.md)   | Shared events and projections for TUI, ACP, HTTP, and a future GUI       |
| [Database lifecycle](../migration.md)                 | Zuno database selection and schema changes                               |
| [Session retention](../session-retention.md)          | Reversible archive and irreversible delete operations                    |
| [Resource gates](../resource-gates.md)                | Measured results for the six gates, opt-in commands, and known limits    |
| [Performance methodology](../perf-methodology.md)     | How memory and liveness gates are measured                               |

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
[resource gates](../resource-gates.md).

## License

Licensed under the [MIT License](../../LICENSE).
