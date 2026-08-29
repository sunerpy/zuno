# What is Zuno?

Zuno is a coding agent that runs as a single Rust executable. You give it an
objective; it reads code, edits files, runs commands, and reports what it verified.

What separates it from most coding agents comes down to three things: **goals are
durable objects with budgets and recovery**, **a roster of specialist agents
instead of one all-purpose prompt**, and **orchestration structure owned by
configuration rather than decided by the model at runtime**.

## 1. Goal mode: long work that can be tracked and converged

In most agents a "task" exists only inside one conversation. A Zuno goal is a durable
object carrying three things:

| Field | Purpose |
| --- | --- |
| `objective` | The concrete outcome. The model cannot quietly narrow it after creation. |
| `success_criteria` | What completion means. Immutable to the model once set. |
| `token_budget` | A ceiling. The goal stops at it rather than burning indefinitely. |

The point is that **termination is constrained**. An active goal continues until it
genuinely completes, is explicitly paused, reaches its budget, or hits a typed
permanent failure. To mark completion the model needs authoritative evidence; to
report blocked it needs a concrete `blocking_condition`, and the same true impasse has
to persist for three consecutive turns.

```sh
zuno run "migrate the /users endpoint to pagination and get the integration tests passing"
```

That is not just one question. Goals, plans, todo items and background jobs are durable
records in SQLite, so a process that dies mid-turn can reconstruct the work — including
retry deadlines. Which is why prose like "next I will..." is not progress: only a change
in durable state counts.

Further reading: [Goals, plans and todos](/guide/durable-state).

## 2. A specialist roster, not one all-purpose prompt

`zuno agent list` shows fourteen built-in agents. Four of them (`compaction`,
`council-synth`, `summary`, `title`) serve internal runtime tasks; the ten below are the
roles you actually select. They are not rewordings of one prompt — they are **roles with
different capability ceilings**:

| Agent | Role |
| --- | --- |
| `orchestrator` | Default primary. Decomposes and delegates, keeps architecture decisions and the final audit. |
| `build` | End-to-end delivery. |
| `plan` | Read-only planning. Write tools are never registered at all. |
| `deep` | Difficult cross-cutting implementation, without recursive delegation. |
| `explorer`, `librarian`, `oracle`, `looker`, `fixer`, `general` | Specialist subagents with explicit positive and negative responsibilities. |

The split matters because **an agent contract can only narrow authority, never widen
it**. Selecting a read-only agent is therefore a guarantee rather than a default that
configuration can reverse:

```sh
# Cannot write a file, whatever sandbox.mode says.
zuno run --agent plan "audit when the retry budget starts counting"
```

Delegation has real boundaries too: a child never obtains a tool its parent lacks,
`delegates` names exactly who it may call, and `subagent_depth` bounds nesting. A child's
report is **evidence the parent verifies**, not a conclusion to adopt.

Further reading: [Agents](/guide/agents), [Orchestration](/orchestration).

## 3. Orchestration is owned by configuration

Council has several isolated seats evaluate the same question independently, then
synthesizes. Its seats, model routes, quorum, concurrency, retry policy, end-to-end
deadline, reserved synthesis time and output bounds are **all configuration** — the model
supplies the question and cannot rewrite any of those parameters.

Workflows are the same: `maxAgents` (default twelve), `maxParallel` (default four) and
the node DAG are an immutable template.

The trade is deliberate. Handing orchestration parameters to the model is handing it the
ability to relax its own constraints under pressure. Fixed in configuration, the
behaviour is reproducible and auditable.

## Design lineage

Zuno treats DeepSeek Harness, Codex, oh-my-openagent, pi-agent, OpenCode and Claw Code
as **design sources, not compatibility targets**.

The deepest influence is DeepSeek Harness's "everything is a plugin." In Zuno that ABI
is concretely a native Rust `Component`: it prepares typed services and deferred
effects, receives an exact asynchronous disposer for every started effect, and
participates in transactional `HarnessProfile` replacement. A capability is complete only
when its interface, provider and consumer are all present.

Several project-wide rules follow from that:

- **Registration is an effect.** Mounting returns the disposer that removes exactly what
  was registered, and profile replacement rolls back in reverse order on failure.
- **Model-visible means logged.** Any input that can change a model request must be
  reconstructable from durable session events.
- **Composition over branching.** Deployment choices go into validated profile fields
  rather than a growing pile of conditionals in a central loop.

The full adopt/reject ledger is in
[Harness comparison](https://github.com/sunerpy/zuno/blob/main/docs/design/harness-comparison.md).

## What one binary actually buys

Linux ships as a static musl artifact; macOS and Windows are native builds. No Node, no
Python, no package manager in the execution path, and no runtime to keep aligned with the
agent's version.

There is exactly one external dependency: `rg` (ripgrep) major version 14 or newer,
because `glob` and `grep` drive real ripgrep rather than reimplementing its walker. A
missing or unsupported `rg` is a startup error for the tool runtime, never a silent
fallback.

Extensions are native too: declarative packages (agents, workflows, Skills), WebAssembly
components under explicit WASI grants, or contained child processes. Zuno loads no Rust
dynamic libraries — Rust has no stable plugin ABI, and unloading a library cannot prove
its threads, callbacks and borrowed values are gone.

## How this differs from a chat-shaped tool

A chat interface that can call tools is optimized for conversation. Zuno is optimized for
a unit of work that survives interruption.

| Concern | Chat-shaped tool | Zuno |
| --- | --- | --- |
| Task | Implicit intent of one conversation | Durable goal with success criteria and a budget |
| Division of labour | One all-purpose prompt | Ten selectable agents with different ceilings |
| Delegation | Another prompt in the same context | Child session with its own durable state and ceiling |
| Orchestration | Model decides | Seats, quorum and concurrency fixed by configuration |
| History | In-memory transcript or a hosted thread | Durable SQLite events, replayable and resumable |
| Retries | Client loop, lost on restart | Persisted exponential-backoff deadline |
| Repeated tool calls | Retry on failure | At-most-once by default; only read-only or idempotent tools may declare replay |
| Command safety | Ask the model not to do damage | OS confinement plus an independent permission gate |

The difference is sharpest when a timeout happens near a side effect: Zuno records that
outcome as **uncertain** and requires authoritative-state inspection, instead of
mechanically replaying the call and hoping the first attempt did nothing.

## What Zuno does not do

- **No hosted service.** No console, bundled web application or hosted GitHub agent.
  Those command names exist only to state the replacement, and they exit with failure.
  See [Excluded commands](/cli/excluded).
- **No compatibility with other agents.** No configuration, plugin, hook or tool-argument
  compatibility with OpenCode, Codex or Claude Code. They are design references.
- **No incremental migration chain.** The project is unreleased, so a schema change bumps
  the format version and development databases are rebuilt. See
  [Database lifecycle](/migration).
- **No self-uninstall.** Remove the binary the way you installed it.
- **No confined sandbox on macOS or Windows yet.** The confinement backend is Linux-only,
  so other platforms fail closed by default; write-capable agents can select native
  execution through an explicit trusted choice. See
  [Permissions and sandboxing](/guide/permissions).

## Where to start

To get running: [Installation](/guide/installation), then [Quick start](/guide/quick-start).

To understand the execution model first, [Goals, plans and todos](/guide/durable-state)
and [Agents](/guide/agents) are the densest pages.

## See also

- [Goals, plans and todos](/guide/durable-state)
- [Agents](/guide/agents)
- [Orchestration](/orchestration)
- [Permissions and sandboxing](/guide/permissions)
