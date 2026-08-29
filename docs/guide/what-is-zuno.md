# What is Zuno?

Zuno is a coding agent that runs as a single Rust executable. You give it a bounded
objective; it reads code, edits files, runs commands, and reports what it verified.
There is no runtime to install beside it and no service to keep running.

The distinguishing property is not the model. It is that every input which could change
a model request is written to SQLite before that request leaves, and that command
execution is confined by the operating system rather than by a prompt asking the model
to behave.

## Design commitments

Four decisions shape everything else. Each trades some convenience for a property you
can rely on.

### One binary, no runtime dependency

Linux releases are static musl artifacts; macOS and Windows use native builds. There is
no Node, Python, or package manager in the execution path. One external executable is
required: `rg` (ripgrep) major version 14 or newer, because `glob` and `grep` drive the
real ripgrep rather than a second reimplementation of its walker. A missing or
unsupported `rg` is a startup error for the tool runtime, never a silent fallback.

### Durable by construction

A session is not a scrollback buffer. Prompts, tool results, retry notices, and
subagent reports are durable session events, and the assembled prompt is persisted with
stable section identifiers and a content digest before the provider request. A process
that dies mid-turn can be resumed, and a retry deadline is reconstructed from SQLite
instead of dying with the process.

That is also why the store grows and eventually needs pruning. See
[Sessions and turns](/guide/sessions).

### The sandbox fails closed by default

`read-only` and `workspace-write` both require a proved OS confinement backend. When
none is available, the default `sandbox.onUnavailable: "deny"` refuses Shell:

```text
no trusted system bubblewrap executable was found
```

Unconfined execution is always a trusted opt-in. `danger-full-access` names it directly,
skips confined-backend discovery, and uses the native process backend. Alternatively,
`sandbox.onUnavailable: "run-unconfined"` may let a write-capable
`workspace-write` Agent fall back only after an eligible typed availability failure;
read-only Agents and unsafe or internal failures never fall back. The confinement
backend is currently implemented for Linux only, so macOS and Windows fail closed by
default but can use either explicit trusted option for write-capable execution. Full
detail is in [Permissions and sandboxing](/guide/permissions).

### Extensions are native, not a plugin ABI

An extension package is declarative (agents, workflows, skills), a WebAssembly
component under explicit WASI grants, or a contained child process which must declare
`host.full` because an ordinary OS process cannot enforce anything narrower. Zuno does
not load Rust dynamic libraries: Rust has no stable plugin ABI, and unloading a library
cannot prove that its threads, callbacks, and borrowed values are gone. See
[Plugins](/plugins).

## How this differs from a chat-shaped tool

A chat interface that can call tools optimizes for conversation. Zuno optimizes for a
unit of work that survives interruption.

| Concern | Chat-shaped tool | Zuno |
| --- | --- | --- |
| History | Transcript in memory or a hosted thread | Durable SQLite events, replayable and resumable |
| Prompt | Assembled per request, not retained | Persisted post-hook with section ids and a digest |
| Command safety | The model is asked not to cause damage | OS confinement plus an independent permission gate |
| Retry | Client loop, lost on restart | Persisted exponential-backoff deadline |
| Delegation | Another prompt in the same context | Child session with its own durable state and a capability ceiling |
| Tool re-execution | Retry on failure | At-most-once by default; only read-only or idempotent tools declare replay safety |

The practical consequence is visible after a timeout around a side effect: Zuno records
that outcome as uncertain and requires inspection of authoritative state. It will not
mechanically re-run the call and hope the first attempt did nothing.

## What Zuno does not do

Being explicit about this saves time:

- No hosted console, bundled web application, or hosted GitHub agent. Those command
  names are registered only to explain what replaces them, and they exit unsuccessfully.
  See [Excluded commands](/cli/excluded).
- No self-uninstall. Remove the binary with whatever installed it.
- No incremental database migration chain. The project is unreleased, so a schema change
  bumps the format and development databases are rebuilt. See
  [Database lifecycle](/migration).
- No configuration, plugin, hook, or tool-argument compatibility with OpenCode, Codex,
  or Claude Code. Those are design references, not compatibility targets.
- No confined sandbox on macOS or Windows yet. The default is fail-closed; trusted
  `danger-full-access` or unavailable-only fallback can select native execution for a
  write-capable Agent.

## Where to start

To get it running, read [Installation](/guide/installation) and then
[Quick start](/guide/quick-start). To understand the execution model first,
[Sessions and turns](/guide/sessions) and [Agents](/guide/agents) explain the most per
page.

## See also

- [Installation](/guide/installation)
- [Quick start](/guide/quick-start)
- [Permissions and sandboxing](/guide/permissions)
- [Harness runtime](/harness-runtime)
