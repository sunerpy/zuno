# What is Zuno?

Zuno is a local coding agent distributed as a Rust executable. It can inspect a
repository, edit files, run commands, delegate bounded work, and report the checks it
completed.

Its main design concern is continuity: a task should keep its recorded state and safety
boundaries when a provider request fails, a process exits, or the work moves to another
client.

## Durable work

A Zuno session is stored as events in SQLite. User input, assembled prompts, tool
results, retry notices, and child-agent reports are written before they are needed for
the next model request.

Goals, plans, todos, and background jobs are separate durable records:

- a goal defines the outcome and optional budget;
- a plan records ordered implementation steps;
- todos track smaller pieces of work;
- jobs track delegated agents and background commands.

Recoverable provider, stream, network, and database failures use persisted retry
deadlines. Restarting Zuno reconstructs the deadline instead of starting a new retry
loop. A tool call with an uncertain side effect is recorded for inspection and is not
replayed mechanically.

See [Goals, plans and todos](/guide/durable-state) and
[Sessions and turns](/guide/sessions).

## Agents define authority

The selected agent determines which tools and delegation paths exist for a run.

| Agent | Intended use |
| --- | --- |
| `orchestrator` | Default primary agent; decomposes work and verifies delegated results |
| `build` | End-to-end implementation and validation |
| `plan` | Read-only investigation and planning |
| `deep` | Difficult cross-cutting implementation without recursive delegation |

Specialist agents handle exploration, research, review, visual inspection, and focused
fixes. An agent contract can remove tools or delegates, but it cannot add authority
beyond the parent runtime. A child session therefore cannot obtain a tool its parent
does not have.

Council and workflow topology are configuration: seats, quorum, concurrency, routes,
deadlines, and retry policy are fixed before the model supplies the question.

See [Agents](/guide/agents) and [Orchestration](/orchestration).

## Shell execution has independent gates

A Shell request passes through tool-argument validation, permission policy, command-risk
checks, and the selected execution backend. These controls do not substitute for one
another.

On Linux, `read-only` and `workspace-write` use bubblewrap, capability dropping, and
seccomp. If the requested confinement cannot be deployed, Zuno refuses the command by
default. Trusted configuration can select `danger-full-access` directly, select the
native backend for every agent with `sandbox.backend: native` while keeping the
permission mode, or allow `workspace-write` to fall back only for eligible
sandbox-availability failures. Read-only agents never use that fallback.

The confined backend is not yet implemented on macOS or Windows. Every agent can use an
explicit trusted native-execution choice on those platforms; for a read-only agent that
choice is `sandbox.backend: native`, under which its read-only contract is a
role boundary rather than an OS boundary.

See [Permissions and sandboxing](/guide/permissions).

## One runtime, several clients

The TUI, headless runner, ACP server, and HTTP server use the same session commands,
durable events, inbox, and projections. Client disconnects do not create a separate
agent lifecycle.

The runtime itself is composed from typed Rust `Component`s. Components register
services and effects through a `HarnessProfile`; profile replacement validates the new
profile before publication and rolls back failed mounts in reverse order.

Extensions can contribute agents, workflows, Skills, WASI components, or contained
process tools. Zuno does not load Rust dynamic libraries. `glob` and `grep` use an
external `rg` (ripgrep) 14 or newer, but its absence does not prevent Zuno or unrelated
tools from starting; the search tools report the missing capability when invoked.

See [Harness Runtime](/harness-runtime), [Plugins and extensions](/plugins), and the
[Harness comparison](https://github.com/sunerpy/zuno/blob/main/docs/design/harness-comparison.md).

## Current boundaries

- Zuno is in early 0.x development; data and extension formats may change through
  documented versioning and migration boundaries.
- It is a local CLI and server, not a hosted coding service.
- It uses Zuno configuration and protocols rather than compatibility layers for other
  coding agents.
- Confined Shell execution is currently Linux-only.
- Providers and models must be configured explicitly.

## Where to start

| Goal | Page |
| --- | --- |
| Install and run Zuno | [Quick start](/guide/quick-start) |
| Understand sessions and recovery | [Sessions and turns](/guide/sessions) |
| Choose an agent | [Agents](/guide/agents) |
| Configure command authority | [Permissions and sandboxing](/guide/permissions) |
| Connect a provider | [Providers and credentials](/reference/providers) |
