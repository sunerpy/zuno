# Diagnostics

Zuno's `debug` subcommands answer one question: what does this installation actually
think is true? They are read-and-report surfaces, so running one is safe and never
mutates session state.

Reach for them when behavior disagrees with configuration. The common thread in almost
every confusing case is an assumption about a merged result that was never printed.

This page is organized by symptom. [zuno debug](/cli/debug) is the complete option
reference for each subcommand.

## My configuration edit had no effect

Start with which roots this executable resolved. A second installation, a channel
build with a different database, or an unexpected `ZUNO_CONFIG_DIR` all present as
"my edit was ignored".

```sh
zuno debug paths
zuno debug config
```

`debug paths` reports the resolved data, config, and log paths. `debug config` prints
the merged document, which is the only reliable way to see how layers combined —
objects merge recursively while arrays and scalars replace, so an array you expected to
extend may have been replaced instead. See [Files and precedence](/config/files).

If `debug config` shows your value but behavior still disagrees, the value is being
narrowed downstream by an agent contract or a permission rule. Continue with
`debug agent` or `debug permissions` below.

## A tool call is blocked or prompts unexpectedly

```sh
zuno debug permissions
```

This reports both the configured mode and the effective mode. They differ when an agent
contract narrows authority or when `danger-full-access` is in play, and that difference
is usually the answer. Explicit denies remain terminal in every mode, including
`allow_all`, so a rule you added to loosen things cannot override a deny elsewhere.

The full authority model is in [Permissions and sandboxing](/guide/permissions).

## One agent behaves differently from another

```sh
zuno debug agent build
```

`debug agent <name>` prints the effective contract for that agent: its resolved model
route, tool visibility, permission ruleset, and the agent-filtered Skill view including
metadata and selected-body budgets, rendered/omitted/truncated coverage, and a bounded
preview.

Read this after any change to `tools`, `permission`, `requiredSkills`, or a preset. The
resolved set is a product of several layers and is not reliably predictable from
configuration alone. See [Custom agents](/config/custom-agents).

## A Skill is not being triggered

```sh
zuno debug skill
```

This is raw discovery, before agent filtering. Its output explicitly reports
`view.kind: "raw_discovery"`, `agentFiltered: false`, and
`extensionOverlayApplied: false`. The `skills` array preserves same-name entries from
different sources, and `summary` reports source, described, and unique counts along with
ambiguous names.

Three findings and what they mean:

| Finding | Cause |
| --- | --- |
| The Skill is absent | Frontmatter is invalid, or the root is not in the discovery order |
| Present but not described | No `description`, so it is hidden from the model-facing catalog |
| Listed under ambiguous names | Two sources declare the same name, which disables the direct slash form |

Restart before reading, since the output reflects this process's discovery. If the Skill
appears here but a specific agent does not use it, compare against
`zuno debug agent <name>`. See [Authoring Skills](/config/authoring-skills).

## The model did not follow an instruction file

```sh
zuno debug prompt
zuno debug prompt --session ses_1a2b3c --step 2
zuno debug prompt --show-sensitive
```

Every model-visible prompt section is durably logged, so this is a question with a
factual answer rather than an inference. `--session <ID>` selects a session and defaults
to the latest receipt; `--step <N>` selects a one-based provider request step within it.

`--show-sensitive` includes model-visible instruction, AGENTS, skill, and memory
content. Treat that output as sensitive before pasting it into a ticket. Without the
flag the sections are still listed, which usually answers whether a file was included.

See [Instructions and AGENTS.md](/config/instructions) for the discovery rules that
decide what should have been there.

## A shell command fails only under Zuno

Probe whether the confinement mode is actually deployable on this host:

```sh
zuno debug sandbox --mode workspace-write
zuno debug sandbox --mode read-only --check
zuno debug sandbox --mode workspace-write --network allow
zuno debug sandbox --mode workspace-write \
  --sandbox-on-unavailable run-unconfined
```

| Option | Values | Default |
| --- | --- | --- |
| `--mode <MODE>` | `read-only`, `workspace-write`, `danger-full-access` | `workspace-write` |
| `--network <NETWORK>` | `deny`, `allow` | `deny` for confined modes, `allow` for `danger-full-access` |
| `--sandbox-on-unavailable <ACTION>` | `deny`, `run-unconfined` | `deny` |
| `--check` | Exit unsuccessfully when the requested policy is not deployable | |

A restricted mode verifies bubblewrap deployment, so this is the command that
distinguishes "my configuration is wrong" from "this host cannot enforce the mode I
asked for". Use `--check` in CI or a health check, where a non-zero exit is more useful
than output.

The JSON report separates the requested policy from the execution resolution. Inspect
`requestedMode`, `requestedNetwork`, `effectiveMode`, `effectiveNetwork`,
`fallbackEligible`, `resolutionKind`, and `fallbackReason`. An eligible
`run-unconfined` result can therefore report `ready: false` for requested confinement
while showing `resolutionKind: "unavailable_fallback"` and effective host authority.

`--check` stays strict: it exits unsuccessfully whenever the requested confinement is not
deployable, even when runtime fallback is permitted. This makes it safe as a deployment
gate instead of accidentally validating an unconfined host.

The confinement semantics themselves are in
[Permissions and sandboxing](/guide/permissions).

## File search returns the wrong set

The search backend has its own ignore handling, so what it sees is not always what a
plain shell glob sees:

```sh
zuno debug rg files --query harness --limit 20
zuno debug rg files --glob '*.rs' --limit 50
zuno debug rg search 'ToolReplayPolicy' --glob '*.rs' --limit 20
```

| Subcommand | Argument | Options |
| --- | --- | --- |
| `rg files` | | `--query <QUERY>`, `--glob <GLOB>`, `--limit <LIMIT>` |
| `rg search` | `<PATTERN>` | `--glob <GLOB>`, `--limit <LIMIT>` |

If a file is missing here, it is being excluded by ignore rules rather than by the
query. `zuno excluded` reports exclusion decisions directly.

## Diagnostics or symbols are missing

```sh
zuno debug lsp diagnostics src/main.rs
zuno debug lsp symbols ToolRegistry
zuno debug lsp document-symbols file:///abs/path/src/main.rs
```

| Subcommand | Argument |
| --- | --- |
| `lsp diagnostics` | `<FILE>` |
| `lsp symbols` | `<QUERY>` |
| `lsp document-symbols` | `<URI>` |

`document-symbols` takes a URI, not a path — that is the most common reason it returns
nothing. Empty output from `diagnostics` usually means no language server is configured
or started for that file type; check the `lsp` key with `zuno debug config`.

## An edit needs to be inspected or undone

```sh
zuno debug snapshot track
zuno debug snapshot diff <HASH>
zuno debug snapshot patch <HASH>
```

`track` reports what the snapshot store currently holds. `diff` shows a snapshot's
changes and `patch` prints its patch, both taking a `<HASH>`. Snapshots are recorded so
edits can be undone; the top-level `snapshot` key controls whether they are taken and
defaults to true.

## Getting more log detail from any subcommand

Every `debug` subcommand accepts the global options:

| Option | Values |
| --- | --- |
| `--print-logs` | Print logs to stderr in addition to the structured local log store |
| `--log-level <LOG_LEVEL>` | `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` |
| `--sandbox <SANDBOX>` | `read-only`, `workspace-write`, `danger-full-access` |
| `--sandbox-on-unavailable <ACTION>` | `deny`, `run-unconfined` |

```sh
zuno debug config --print-logs --log-level DEBUG
```

Logs go to the structured local store by default; `--print-logs` adds stderr without
disabling the store. See [Logging](/logging) for the store's location and retention.

## See also

- [zuno debug](/cli/debug)
- [Permissions and sandboxing](/guide/permissions)
- [Configuration overview](/config/)
- [Logging](/logging)
