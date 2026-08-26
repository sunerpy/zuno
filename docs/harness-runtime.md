# Harness Runtime

Zuno assembles an agent from a native harness profile. A profile is a set of bundles, and each bundle contributes typed components to one scoped runtime.

## Runtime model

- `Component` is the lifecycle unit. `prepare` is side-effect-free: it stages typed
  services, requirements, and deferred effects in a `PrepareContext`.
- An effect starts only after the complete candidate composition has prepared. Its
  start returns the exact asynchronous disposer that must prove quiescence.
- `ProfileBundle` groups components that are installed and replaced together.
- `HarnessProfile` is the complete composition selected for a session.
- `HarnessRuntime` owns `Profile`, `Session`, `Agent`, and `Turn` scopes. A child scope inherits services and may override them locally.
- `AgentDriver` owns the turn-driving policy. The default driver wraps the standard agent loop; benchmark, workflow, remote, and evaluation harnesses can install another driver without modifying that loop.
- `ToolManifest` is the profile's model-visible tool surface. The registry filters all built-ins, including automatically assembled file tools, through this manifest.
- `ToolContributions` carries native `Tool` implementations owned by the profile. Contributions are assembled after built-ins and before MCP tools, pass through the same visibility rules, and may intentionally replace a built-in by wire id.
- Native executable values remain typed services. A coordinated named plane
  projects stable keys, schema contracts, provenance, owner, generation, and
  availability for dynamic consumers without putting executable Rust values in
  a string map. Tool contributions publish their provider-visible schema digest;
  `orchestration_capabilities_bundle` publishes the typed immutable snapshot plus
  Agent Profile, Workflow Template, and source-isolated Skill descriptors.

Profile activation is transactional and exclusive-resource safe. Candidate
components prepare against a staging service view, duplicate identifiers and
missing requirements fail before any effect starts, and no candidate service is
visible outside the transaction. Replacement first withdraws local services and
stops the old composition in reverse order. Only a proven-clean stop permits the
candidate effects to start and their services to publish atomically. Candidate
startup failure cleans the partial candidate and restores the previous definition
through a fresh prepare/start cycle.

Cleanup failure or timeout is never reported as success. The runtime becomes
`Failed` or `Uncertain`, retains typed lifecycle diagnostics, and refuses a second
composition that could overlap the unresolved resource. Repeated shutdown
preserves that terminal outcome. Parent shutdown closes child scopes first; parent
recomposition rejects a still-live child consumer rather than silently leaving it
bound to stale services.

`RuntimeSnapshot` and `ComponentSnapshot` expose lifecycle state, effect ids,
provided/required service types, and scrubbed diagnostics without coupling a
client to the runtime implementation. The TUI projects this inventory today; the
same value is available to future server, ACP, and GUI surfaces.

## Agent and prompt contracts

Agent prompts define role ownership, not a second copy of the runtime manual.
`build` owns delivery and requirement-by-requirement completion evidence; `plan`
owns repository-grounded implementation decisions without product mutation; and
`deep` owns one cross-cutting implementation without recursive delegation. Each
specialist states both its positive responsibility and its delegation boundary.

The primary prompt requires authoritative evidence before mutation, forbids
duplicate delegated discovery, and treats interruption, restart, and uncertain
side effects as verification concerns rather than presentation details. The plan
prompt ties each implementation decision to an observed constraint or explicit
user choice. Goal tools separately enforce creation authority and terminal-state
audits, so prompt wording is guidance over typed runtime policy rather than the
only safety mechanism.

Plan and Work are also typed collaboration contracts. `collaboration.mode` is a
runtime-trust prompt block, separate from the native kernel, agent role, project
instructions, work state, and user input. Plan tells the model to inspect and
update durable planning state without product mutation; Work tells it to execute
against the durable Goal, Plan, Todo, Job, and queue projections. Neither block
grants capabilities or authorizes a mode transition by itself.

Static tool descriptions live in dedicated text files and are byte-pinned by the
prompt golden test. Prompt changes are therefore reviewed as model-visible
behavior, while schemas, permission policy, replay policy, and execution remain
independently testable.

## Extension packages and executable plugin hosts

Zuno exposes one validated package protocol for agents, slash-command workflows,
skills, and runtime tools. It adapts DSH's lifecycle outcome without loading the
Cordis/JavaScript ABI or a Rust dynamic library:

- `extension_define` records an immutable package in the current process and worktree scope.
- `extension_run` validates the desired package set and stages a pending revision.
- `extension_stop` stages removal of contributions while retaining the definition.
- `extension_undefine` removes an inactive definition immediately or stages removal
  of a running definition.
- `extension_inspect` projects static and process-local package state.

Staging never changes the committed catalog. Every live host owns a
`CompositionLease` for one workspace-local revision. A transition can reserve the
pending revision only after all old leases are gone; reservation blocks late old
consumers. The candidate host then starts against the desired catalog and commits
the exact transaction. Only that commit publishes `Running` and advances the
active revision.

The TUI performs this transition as an in-process remount. The server serializes
host acquisition with transition reservation and lets the last old request host
publish the candidate. Both paths rebuild the agent catalog, command registry,
skill catalog, prompt provenance, permissions, and tool definitions together.
Clean candidate preparation/start failure explicitly aborts the transaction and
restores the prior registry state. A cleanup result that cannot prove quiescence
marks the workspace composition `Uncertain` and prevents further mutation until
the process is restarted.

Process-local definitions are held only by `StartupEnvironment`'s shared `ExtensionRegistry`; a new
process starts with an empty registry. Static packages live at
`.zuno/extensions/<id>/extension.json` or
`~/.config/zuno/extensions/<id>/extension.json`, are loaded at composition startup, and require the
directory name to match the package id. Dynamic and static packages use the same
`zuno.extension/v1` schema and contribution merger. Duplicate package ids or duplicate
agent/workflow/skill/tool names across active extension packages fail instead of
silently choosing a winner. An agent contribution cannot rename its map identity
or mark itself disabled.

Static packages may declare one executable runtime:

- `kind: "wasi"` loads a Component Model artifact through Wasmtime. Workspace
  read/write, sockets, and exact environment names are explicit grants. Fuel,
  linear memory, instance resources, wall time, cancellation, and shutdown are
  bounded.
- `kind: "process"` starts a contained executable speaking Zuno's bounded
  line-delimited JSON-RPC protocol. It must declare `host.full`, because an OS
  process cannot enforce narrower host authority.

Here “contained” means that the lifecycle host owns and attempts to reap the
child process tree; it is not an OS security boundary. A `host.full` package is
fully trusted and must itself be sandboxed at the deployment boundary when its
code is not trusted.

All package hosts initialize before their tool routing is published. Unload
withdraws routing and stops hosts in reverse order. Timeout, protocol loss, or a
cleanup result that cannot prove quiescence becomes `Uncertain` and is never
replayed. Process-local definitions reject executable runtimes; install
persistent packages with `zuno plugin add|update|remove|list`.

Extension tools use the native effect, strict-authorization, replay,
concurrency, and UI-intent pipeline. Version 1 keeps runtime calls exclusive,
defaults effect to side-effecting and replay to never, and permits safe replay
only when a WASI capability envelope itself excludes network and workspace
writes. Process tools are always side-effecting and non-replayable because
`host.full` cannot enforce a read-only claim. A runtime with no tool consumer is
rejected.

Configured and extension agents are not prompt-only aliases. Agents whose mode
is `subagent` or `all` join the exact `task` target roster and retain their
configured model, variant, prompt, and permissions in the child turn. File,
network, and environment access comes through the same native tools and
permission rules as built-in agents: `read`/`glob`/`grep`/`lsp`, `edit`,
`webfetch`/`web_search`, and `bash`. `bash` inherits the Zuno process
environment and host visibility and therefore remains a side-effecting,
approval-governed capability. A workflow that requires one custom agent
explicitly calls `task` with that `subagent_type`.

Providers, drivers, approvals, and arbitrary typed services remain trusted
compiled Rust `Component` implementations mounted through a `HarnessProfile`.
See [plugins, custom agents, and workflows](plugins.md) for manifests,
capability tables, protocols, and runnable examples.

## Native agents

The built-in catalog separates primary modes, delegable specialists, and hidden engine agents:

| agent | role |
| --- | --- |
| `orchestrator` | Default multi-agent delivery owner and the only native agent that may delegate. |
| `build` | Direct end-to-end implementation in one lane, with all subagent tools withheld. |
| `plan` | Read-only repository research and implementation-ready planning. |
| `deep` | Difficult cross-cutting implementation without recursive delegation. |
| `fixer` | Focused local implementation with minimal change and regression scope. |
| `general` | Bounded miscellaneous execution when no narrower specialist owns the work. |
| `explorer` | Read-only repository structure, definition, caller, and impact discovery. |
| `librarian` | Current external documentation, release, and upstream research. |
| `oracle` | Read-only architecture review, root-cause analysis, and explicit trade-off advice. |
| `looker` | Visual artifact inspection when a vision-capable model is available. |

`compaction`, `title`, and `summary` are hidden engine agents. A user-defined agent may be declared under `agents.<name>` or as Markdown under `.zuno/agent/**/*.md`; it enters the same resolution, permission, prompt, and provenance pipeline as a native agent.

See [agent orchestration and model routing](orchestration.md) for the exact
delegate roster, per-Agent and preset model routes, reasoning precedence,
background report delivery, configured workflow DAGs, and Council.

User-facing agents answer in natural Markdown. Zuno does not require XML-like
reply envelopes unless a typed runtime consumer exists for that exact structure.
The built-in prompts emphasize intent matching, deliberate tool use, scoped
changes, proportional verification, and concise outcome-first reporting. In
particular, self-contained reasoning or writing does not justify a shell call or
a throwaway file.

### Delegated session projection and approval routing

A native delegation runs in its own child `TurnHost`; the parent tool waits for the
foreground result but does not own the child's event channel. Interactive composition
installs a `ChildTurnObserver` that first receives the child's durable replay and resolved
identity, then every live `TurnEvent`. The TUI folds those records into a per-session read
model and attaches views to that model. Switching the visible session never remounts or
aborts the parent or a sibling host.

An attached native child is also an independent input target. The TUI sends its durable
session id with the submission instead of routing the text through the parent transcript.
The child inbox commits the text before delivery. A running child receives a soft steer;
an idle or completed child acquires a run lease and reopens a `TurnHost` with the resolved
Agent, model, effort, and inherited orchestration identity captured for that child. The
`SessionWakeCoordinator` closes the active-to-idle race, so input that misses the running
turn remains pending and starts the next child turn. Delivery belongs to the workspace
supervisor and is cancelled with that lifecycle. A direct child continuation updates only
the child session; it does not fabricate a report or another input for the parent.

Permission attribution comes from immutable coordinates captured by `ToolContext`, not
from a broker-wide current-session slot. A root or child request therefore carries the
session, assistant message, and provider call that raised it through every rule and human
approval layer. The foreground TUI broker serializes all such asks, scopes standing grants
to one session, and fails closed by rejecting pending requests when its last surface or
wake channel closes.

## Prompt provenance

Prompt assembly is ordered data, not string concatenation spread across the CLI. Every section has a stable identifier, source, exact content, and SHA-256 digest. The composition root currently orders:

1. native or configured agent base prompt;
2. generated agent policy;
3. the typed Plan or Work collaboration mode;
4. global and project memory;
5. extension lifecycle guidance and the exact active package projection;
6. discovered instruction files;
7. the skill trigger policy;
8. the bounded skill metadata catalog.

The trigger policy makes a named or clearly matching skill a pre-action requirement. The base prompt carries bounded name, description, and source metadata; descriptions are shortened before a source identity is omitted. `skills.maxContextTokens` overrides the default two-percent context budget, while `skills.includeInstructions: false` disables prompt injection. The `skill` tool pages the complete catalog with `list`, searches it with `search`, reads a selected body with `load`, and resolves relative text with `read_resource`. Same-named sources remain distinct and require the advertised source locator. Reads use content-bound cursors and must continue to completion; disk bodies are read after selection rather than retained for the process lifetime.

Skill discovery is Zuno-owned. It advertises project `.zuno`, `.agents`, and
`.claude` roots before Zuno's user-global config and user-global Agent Skills,
then configured paths and pulled URL caches. `.agents` precedes `.claude` within
the same scope. Zuno does not scan OpenCode directories. Canonical paths are
de-duplicated, including symlink aliases, but same-named files from distinct
sources remain separate identities that require source-qualified selection.
Discovery order controls presentation and provenance; it does not silently
choose a same-name winner.

A visible Skill whose name is unique across sources and does not collide with a
real command is also advertised as `/<skill-name>`. A bare invocation loads the
complete body, emits the loaded projection, and does not create a model turn.
Supplying arguments loads the Skill first and then admits the exact canonical
slash text as user input. Real commands always win; ambiguous names remain
available through `/skills` and source-qualified `skill` operations.

### Repository instruction initialization

The command registry always seeds two Zuno instruction workflows before
`/review`:

- `/init [focus...]` creates or improves the repository-root `AGENTS.md`. It is
  the compact choice for a repository whose guidance does not need scoped
  overrides.
- `/init-deep [--create-new] [--max-depth=N] [focus...]` maps the repository with
  CodeGraph first, then creates or updates the root file and adds scoped
  `AGENTS.md` files only at real responsibility, build, language, or deployment
  boundaries. A scoped file contains only rules that differ from its parent; it
  must not duplicate inherited guidance.

Both workflows preserve accurate existing content and treat remaining arguments
as user priorities. By default `/init-deep` may improve existing files and create
missing ones. `--create-new` leaves every existing `AGENTS.md` unchanged and only
creates missing files. `--max-depth=N` counts the repository root as depth zero
and prevents inspection or scoped-file creation below `N`; the root file remains
in scope.

Before the provider request, the loop persists `session.prompt.assembled`. The event records the ordered sections and the actual post-hook system prompt, so a model request can be reconstructed even when a hook transformed the assembled text. Identical prompt content is logged once per turn.

## Auditable memory and reflection

Resident memory has one mutation boundary: `memory_propose`. Foreground agents
and the isolated post-delivery reflection fork both use that tool, which validates
the requested add/replace/remove operation and inserts a durable
`MemoryCandidate`; it never edits the resident file directly. Candidates retain
scope, action, reason, confidence, source session/message, timestamps,
diagnostics, and exact before/after snapshots.

The default promotion policy is `review`. `high_confidence` applies candidates at
or above the configured threshold, while `automatic` applies every validated
candidate. All policies use the same durable state machine:

```text
pending -> applying -> applied -> undoing -> undone
       \-> rejected
       \-> failed / uncertain
```

`applying` and `undoing` are written before touching the file. After process loss,
the runtime compares the resident file with both stored snapshots and marks the
observed result; it never replays the write or undo. Any third state becomes
`uncertain` and requires user inspection.

Reflection runs only after a final response was delivered and uses an explicitly
configured reachable `small_model`. Zuno persists the exact review prompt,
replayed durable turn transcript, current resident-memory snapshot, tool schema,
model identity, digest, and terminal outcome as `memory.reflection.request` and
`memory.reflection.outcome`. Stream truncation, malformed arguments, denied tools,
and proposal failures are durable failed outcomes. The fork can call only
`memory_propose`; it cannot reach shell, files, normal tools, or foreground
conversation state.

Periodic cadence is admitted from a durable per-session delivery sequence rather
than a process-local counter. The source assistant message is counted once across
host rebuilds and restarts. A selected review owns a leased durable job; process
loss changes an expired job to `uncertain` and never replays its model request.
The reviewer compares the supplied resident snapshot before proposing changes,
prefers replacement to duplicate additions, and can organize memory only through
the same audited add/replace/remove candidate workflow.

Candidate validation rejects prompt injection, credential literals, ambiguous
locators, over-budget results, and external file drift. Automatic learning is
limited to durable user facts, explicit corrections, repository rules, and
verified reusable recovery knowledge. It cannot rewrite code, prompts, agents,
extensions, or skills. `/memory` is the user-owned review and correction surface.
See [auditable memory and reflection](design/memory-learning.md).

## Durable inputs

Every model-visible external input is admitted to the session event log and durable inbox in one SQLite transaction before execution is attempted. The inbox is the source of truth across active turns, idle sessions, process restarts, and competing drivers.

An interactive `SessionChoice::New` is prepared without inserting a `session` row. The
process-local identity is stable across model, agent, MCP, and theme changes, but opening,
browsing, or leaving the welcome screen creates no durable session. The first model-bound
submission inserts the session and its user message in one transaction, then emits
`session.materialized` for clients. Existing and continued sessions still hydrate immediately.
The TUI `/new` command selects another prepared `SessionChoice::New` in the same
terminal activation. It opens an empty conversation shell directly instead of
returning to the launch welcome surface, and it does not bypass this lazy
materialization boundary.

Drivers promote inputs in FIFO order. Promotion is transactional and can target one input identifier for a live soft interrupt. A malformed input records a session error and does not strand later queue entries.

User prompts and subagent reports share this protocol:

- An active parent receives a soft interrupt and promotes the report at the next tool-safe point.
- If the report misses the final safe point, the wake coordinator waits for the active lease to end and starts another turn while the input is still pending.
- An idle parent is claimed and driven immediately.
- A restarted process recovers pending reports from the durable inbox.

Interactive TUI input uses the same durable boundary. When idle, `Enter`
starts a turn. During an active turn, `Enter` admits a FIFO `queue` item for the
next turn; `Ctrl+Enter` is the explicit `steer` override and requests a soft
interrupt at the nearest safe step boundary. `Shift+Enter`, `Alt+Enter`, and
`Ctrl+J` insert a newline. The UI reports an item as queued only after SQLite
commits it, and pending items can be edited or cancelled by revision and survive
a process restart.

A provider stream or provider-retry delay is wakeable for explicit steering:
Zuno checkpoints any partial assistant output with `finish: steer`, promotes the
durable input, and starts the next model step without emitting
`TurnInterrupted`. An executing tool is not abandoned merely to steer; its
result reaches the next tool-safe point first. Commands and ordinary active-turn
submissions target the FIFO queue. If the turn ends before a steer is consumed,
the already admitted item remains pending and is promoted in FIFO order on the
next turn; it is never lost or duplicated. The bounded in-process prompt channel
is only a wakeup and handoff path, not the queue of record.

Tool-owned human input is projected separately from execution. A permission
prompt reports `awaiting approval`; a structured question reports `awaiting
answer`. Both surfaces replace the composer region rather than becoming another
transcript card. Permission choices support Left/Right, the existing Up/Down
aliases, Enter, and mouse selection; explicit expansion moves the prompt to the
larger overlay. Questions show `Question i/n`, the remaining unanswered count,
numbered choices, and a numbered `Other` input. They support Up/Down and `j/k`
within a question, Left/Right and `h/l` across questions, number-key selection,
Enter, Space for multi-select, and mouse selection. Per-question cursors and
custom drafts survive navigation. Cancelling either interaction resolves the
tool as a typed denial and never fabricates an answer.

The conversation surface separates reply identity from transient work state. The
identity row contains the resolved agent, catalog model display name, and configured
reasoning effort. It follows the bottom of a short assistant reply; once transcript
content fills the available viewport, the same row becomes sticky immediately above
the composer. It does not invent cost or speed multipliers when no authoritative
runtime metadata exists.

The frame's final row is the live control surface. During a turn it shows one
animation-clock-driven pulse, the resolved interrupt key, latest provider-prompt
occupancy, and the command-list key. The first interrupt press changes that same row
to its confirmation state, so the transcript and composer do not reflow. Permission
and question waits replace the pulse with their explicit reason. When idle, the row
returns to directory, context, and command discovery. Transient `working` rows are not
inserted into the transcript; durable activity, errors, interruption markers, and
assistant content remain reconstructable from session events.

Context occupancy is the most recent complete provider prompt divided by the catalog
context limit. It is replaced on each provider report rather than accumulated across
the session; cumulative disjoint token buckets remain available in the usage
projection and sidebar.

## Plan and Work transitions

`/plan` is the interactive mode switch. In Work mode it opens a confirmation in
the same keyboard- and mouse-capable dialog system used by other TUI choices. In
Plan mode it becomes the handoff path to implementation: a durable plan must
exist, and the confirmation names its title, revision, and completed-step count.
`/start-plan` enters Plan mode directly; `/start-work` performs the same durable
plan check and confirmation without first toggling through `/plan`.

Plan is enforced below the prompt by a deny-by-default capability overlay. It
allows repository inspection, read-only LSP and search, questions, Skills, and
typed Goal/Plan/Todo operations, while shell and file mutation remain denied.
The model can recommend Start Work but cannot select it for the user. A confirmed
selection is persisted as the session agent, so explicit resume and in-process
session switching restore the collaboration mode without restarting the TUI.

## Compaction and hard interruption

`/compact` invokes the hidden compaction agent through the live `TurnHost`; it
does not synthesize a client-only summary. The summary, retained tail, marker,
prompt provenance, and usage are durable. Proactive compaction uses the validated
`compaction.threshold_percent` of the usable model window and can be disabled
with `compaction.auto: false`. A provider-confirmed context-limit failure retains
its bounded recovery compaction, while a manual command is always eligible.

A hard interruption is session-scoped and linearizable across turn handoff. If
the previous run guard has dropped but an already admitted follow-up has not yet
acquired its guard, the registry arms that next guard instead of discarding the
interrupt. The turn starts with its interrupt signal set, emits the normal
terminal interruption event, and issues no provider request.

Within one mounted TUI session, model, agent, effort, and MCP changes may replace
the `TurnHost`, but they must reuse the mounted session's `SessionRunRegistry`
and rebind its `SessionTitleSink`. Cancellation controls therefore continue to
target the current host generation, and generated title updates continue to
reach the sidebar. Only a real session remount creates a new continuity scope.

After the TUI confirms a hard interruption, it keeps the stopping state visible
and suppresses late provider or tool presentation until `TurnInterrupted` or
`TurnCompleted` establishes the terminal boundary. Durable persistence and
diagnostics still run; the client merely refuses to present post-cancel work as
continued conversation. A side effect that completed before cancellation remains
an observed result and is never mechanically replayed. `TurnInterrupted` adds a
separate session-owned `Conversation interrupted by user.` row to the live
transcript. If an assistant checkpoint exists, the same marker is persisted in
its typed abort error and reconstructed as a separate row when the session is
resumed; it is never presented as assistant prose.

Assistant checkpoints reconcile message usage and the session usage projection in the same
transaction. Repeated checkpoints subtract the previous message snapshot before adding the new
one. Provider accounting is persisted with each snapshot so cache tokens are counted exactly once;
stored assistant rows without a reliable accounting mode remain explicitly unavailable instead of
being reported as zero. The projection stores cumulative disjoint token buckets, the latest whole prompt,
the context limit, and the latest accounting mode.

## Durable goal recovery

An active goal uses two recovery layers. The provider request layer retries a bounded sequence in place and rolls back unpublished partial output before another request. Its in-place backoff is interruptible by both hard cancellation and durable live steering; waking it does not replay the stale provider request. If that sequence still ends in a recoverable error, the goal controller writes a `goal_retry` row before waiting and starts a fresh agent turn when its persisted deadline arrives. There is no cross-turn retry-count ceiling for recoverable failures: the delay grows exponentially, reaches the configured cap, and the goal remains active until it completes, is paused, reaches its token budget, or encounters a permanent failure.

The retry row is tied to the exact `goal_id` and stores the attempt, typed reason, selected delay, schedule time, and next eligible time. Reopening the same session reconstructs the wait from SQLite. Queued user input has priority over an automatic turn, and long waits are split by `poll_interval_ms` so an interactive surface can notice that input promptly.

Local delays use exponential backoff with symmetric jitter and never collapse to zero. A valid provider `Retry-After` value is never shortened by jitter; it is clamped to the configured ceiling rather than replaced by an earlier local delay.

```json
{
  "goal": {
    "retry": {
      "initial_delay_ms": 2000,
      "max_delay_ms": 300000,
      "jitter_percent": 20,
      "poll_interval_ms": 250
    }
  }
}
```

Recovery is selected from typed errors, never rendered messages:

- Transport failures, rate limits, incomplete streams, SQLite writer contention, empty assistant messages, and per-turn step limits schedule another goal turn.
- Context-limit failures compact retained history before retrying. Successful compaction is persisted as its own retry phase so a restart does not compact the same history twice.
- Authentication failures, user interruption, and a closed event consumer pause the goal for human action.
- Invalid provider protocol, unavailable agent/model configuration, corrupt durable state, and other permanent failures block the goal.

### Tool effects and strict authorization

Authorization, replay, and concurrency are independent declarations. Every tool
classifies each invocation as `ReadOnly`, `UserMediated`, `Delegating`, or
`SideEffecting`; the default is `SideEffecting`, so an unknown harness or MCP
tool fails closed. Mixed tools may classify from validated arguments: `bg`
inspection is read-only and `bg cancel` is side-effecting. `execute` is
delegating, and each child call passes through the same permission context with
its own effect.

```json
{
  "permission": {
    "mode": "strict",
    "rules": {}
  }
}
```

Strict mode is off by default. When enabled, an explicit deny is evaluated first,
then every side-effecting invocation requires a fresh attached-user approval even
when a normal rule or plugin says allow. The ask cannot be satisfied by a standing
grant or automatic approval and offers no "always" choice. TUI `--auto` yields to
the human broker; headless surfaces deny the call. Approval covers the same
tool's internal resource checks for that invocation only, while a later explicit
resource deny still wins.

The shell's destructive-command gate is independent of strict mode. A protected
target is denied, while a bounded deletion, a dynamic destructive target, or a
redirect that would replace an existing path marks the ordinary `bash`
permission request as human-only. Permission rules still evaluate first, so an
explicit deny remains terminal; a model-authored argument cannot approve its own
operation. A new static redirect target inside the working directory or the OS
temporary directory is creation rather than overwrite and does not receive this
extra risk prompt. An exact, non-recursive forced removal of a statically named
path that is currently absent below the OS temporary directory is likewise a
no-op cleanup; existing targets, recursive removal, and dynamic targets remain
human-only. The filesystem probe is advisory and does not turn `bash` into a
sandbox.

Refusal is a typed lifecycle outcome rather than an execution failure.
Malformed or unsafe arguments, unavailable tools, and permission denials emit
`ToolDispatchBlocked` with `invalid_arguments`, `unavailable`, or `denied`
before the model-visible error result is appended. Durable tool state retains
`outcome: "blocked"` and `blockKind`, so clients can use warning treatment and
state that the requested effect never ran. Process, transport, and tool
implementation failures remain error outcomes.

Hard turn interruption is observed during tool hooks, permission waiting, and
execution. Cancelling before permission resolves drops the pending approval
future and guarantees that the tool body never starts; cancelling a running
tool joins its cancelled task or process tree before the dispatch returns. A
foreground `task` delegation carries the same interrupt through
`ChildTurnHost`, converts it to the child runner's cancellation token, aborts the
live child turn, and waits for event drain plus host shutdown before returning.

Tool execution is at-most-once by default. `ToolReplayPolicy::Never` is inherited by every tool unless the implementation explicitly declares `Safe`; current safe tools are read-only or idempotent inspection operations such as file reads, glob, grep, skill lookup, session search, job status, LSP inspection, goal status, and web search/fetch.

The loop never mechanically replays a call. It persists the failed tool result and gives it to the model in the next step, including timeouts that might have completed an external side effect before their response was lost. A later recovery turn receives a hidden, SQL-derived notice naming the retry attempt. A `Safe` failure may be attempted again after backoff; a `Never` failure requires authoritative inspection of the worktree or external state before the model decides whether another mutation is appropriate.

Tool overlap is a separate declaration from replay safety.
`ToolConcurrencyPolicy::Exclusive` is the default; only implementations that
declare `ParallelSafe` or `IsolatedBackground` may overlap. The dispatcher still
resolves tools, validates arguments, runs hooks, and asks permissions in model
order. It then executes consecutive non-exclusive calls under the configured
bound and persists results in original call order, regardless of physical
completion order. Shell, writes, unknown extension tools, and MCP tools without an
explicit safety declaration remain exclusive.

The configured `tool_calls` bound is applied even when one parallel-safe group
contains more calls than the limit. `Exclusive` is a two-sided barrier: every
earlier `ParallelSafe` or `IsolatedBackground` call settles before it starts,
and no later call starts until it settles. Physical overlap is bounded while
durable results and client events remain in model order.

MCP lifecycle operations use the same bounded pattern across different servers,
while operations for one server remain generation-serialized. LSP startup and
requests may overlap across servers under one global semaphore; protocol ordering
inside one client remains unchanged. Setting any bound to `1` restores serial
behavior.

Native child sessions, workflow nodes, Council seats, and Codex/Claude Code
`ProductAgent` instances share one process-local delegation budget for a workspace. The
budget survives turn-host replacement within that process, so a background agent
started by an earlier turn still consumes capacity. Reloading configuration
adjusts the bound without cancelling active work: a lower bound waits for enough
active delegations to finish, while a higher bound admits queued work. The queue
is explicit and fair FIFO; later calls cannot barge ahead of existing waiters.
Workflow `maxParallel` remains an additional per-workflow ceiling. Separate Zuno
processes do not yet share a durable quota lease. Within that ceiling, the
workflow scheduler is work-conserving: whenever one node settles, the next ready
node is admitted in template order without waiting for slower siblings from the
same wave. Durable/model-facing results remain in template order. If parent
cancellation and final node completion become ready in the same scheduler tick,
cancellation wins and the workflow cannot publish a false `completed` outcome.

Background native and product-agent jobs commit `queued` before waiting for a
permit, then atomically transition to `running` immediately before invoking the
runner. The TUI reports both states separately. On restart, queued jobs settle as
`cancelled` because execution never began; running jobs settle as `uncertain` and
are not replayed.

`/council` is a TUI launcher over that same native execution path, not another
scheduler. The current Agent must expose `council_run`; otherwise no Council
preset is advertised. Zuno persists the user's original slash message and adds a
one-turn `routing.council` prompt block to the cloned resolver. That block asks
the Agent to invoke `council_run` exactly once with background execution and
`nextStep` delivery, while the base resolver remains byte-identical for later
turns. A launch entered while another turn is active waits in the durable input
queue instead of steering the in-flight model generation.

```json
{
  "concurrency": {
    "tool_calls": 8,
    "delegations": 8,
    "mcp_connections": 8,
    "lsp_requests": 4
  }
}
```

Every value is validated in `1..=64`.

## Native search and shell isolation

`glob` and `grep` use the official `rg` executable as their only search engine.
Zuno contributes a thin adapter for typed arguments, cancellation, bounded JSON
decoding, stable ordering, and result shaping; it does not maintain a second
ripgrep-compatible walker. `rg` major version 14 or newer must be available on
`PATH` (or packaged beside Zuno by a distributor). Missing or unsupported
ripgrep is a startup error for the tool runtime, never a silent fallback.

The `bash` tool is not an OS sandbox. Its tree-sitter command analysis,
deterministic destructive-command gate, permission checks, process-tree
containment, working directory, and time limits reduce accidental execution
risk, but the child still inherits the Zuno process's filesystem, network, and
credentials. Existing redirect targets and other confirmable destructive
operations require a fresh attached-user decision; static creation under the
working directory or OS temporary directory and exact non-recursive `rm -f`
cleanup of an absent OS-temporary path do not. Strict authorization adds HITL to
every side-effecting shell call; neither mechanism adds confinement.

## Resident process containment

Local MCP and LSP servers, process extensions, product agents, PTY sessions, and
background commands share `zuno-process`, but their ownership shapes are
explicit. Local stdio MCP commands are direct process-group leaders, matching
Codex's ordinary MCP topology. They terminate with bounded `SIGTERM` to
`SIGKILL` escalation. Zuno inserts no `__zuno_child_guard` process in front of
or beside MCP.

Other resident and interactive hosts retain dedicated guards where a surviving
per-tree owner or terminal foreground transfer is required. The direct child
returned by `guarded_argv` is the guard, not the payload; owners request
shutdown through `request_contained_process_shutdown` and reap it only after
the contained group settles. On Linux the guard blocks on signals and uses the
parent-death signal. Direct MCP relies on owner close/Drop and therefore does
not promise descendant cleanup after an uncatchable owner `SIGKILL`. The pinned
Codex comparison and the split ownership decision are recorded in
[Resident process containment](design/process-containment.md).

## Background command execution

`bash` registers a command with the process-owned
`BackgroundExecutionService` before spawning it. Explicit background mode and a
foreground attention timeout therefore retain one execution identity and one
process tree; neither path adopts a detached task or starts a second command.
An ordinary foreground command is ephemeral: while it runs, its complete output
is spooled so the normal output policy can inspect it, but its state is hidden
from `/ps` and both the in-memory row and spool file are removed as soon as the
caller consumes the terminal result. A command is made durable only when
`background: true` was requested or the foreground attention deadline promotes
the still-running process.

Durable commands keep a bounded 2 MiB live tail, persist complete output
separately, and record status under `.zuno/background`. The service retains at
most 32 terminal commands per workspace and removes the oldest row together with
its `.status.json` and `.output` files. Running commands are never evicted.
Consequently ordinary `bash` calls no longer accumulate files, while `/ps`,
`bg`, and restart reconciliation keep the state they actually require. Other
tools such as `read`, `grep`, `glob`, and web search never use this directory.
Because the product is still pre-release, terminal rows written by the old
always-durable format are discarded on first open; an old running row is
conservatively rewritten as `uncertain` and is never replayed.

The `bg` tool supports `list`, `output`, `wait`, and `cancel` for executions owned
by the current session. The complete tool has `ToolReplayPolicy::Never` because
one action cancels a process tree. Cancellation reaches descendants through the
shared process containment layer. A hard process ceiling records failure; a
process restart converts a previously running row to `uncertain` and never
replays it.

`StartupEnvironment` shares one service per workspace across parent sessions,
child turns, and in-process session switches. Client projections and `/ps` use
that same service rather than maintaining a second process list. The TUI
subscribes to created and settled execution events, refreshes from the
authoritative service after lag, and updates the right-sidebar `Background`
section even while a model turn is active. Each row carries status, command,
pid, elapsed time, and failure context; the section advertises `/ps` for the
scrollable output view.

## Background subagents and product agents

Foreground `task` runs remain attached to the parent turn's hard interrupt. A
cancelled parent waits until the child runner has acknowledged cancellation and
shut down; it cannot return a successful child result from the same cancellation
tick. `task` creates a distinct `job_*` identifier for each background run while
retaining a separate child session identifier for conversational continuation.

Enabled `productAgent` instances register independent static tools backed by a host-installed Codex or Claude Code process. A product invocation has a one-shot `run_*` id and, in background mode, a separate `job_*` id. It does not create a Zuno child session and cannot be resumed as one.

`reportDelivery` supports:

- `nextStep` (default): settle the job and admit the report to the parent inbox atomically, then wake the parent.
- `quiet`: settle the job without admitting a parent input.

The `job` tool reads durable status for jobs owned by the current parent session. `JobSubject` distinguishes child sessions, product agents, and workflow runs; status is `queued`, `running`, `completed`, `failed`, `cancelled`, or `uncertain`, together with delivery policy, result, error, and subject identity. Council execution remains stored through the workflow service, while the frontend-neutral projection types `council:<preset>` as a Council and attaches its durable child WorkItems. The same child projection represents ordinary workflow nodes and Council seats, including owner, status, elapsed time, and usage.

The right-sidebar `Jobs` section and `/subagent` consume that same projection rather than reconstructing state from tool output. The sidebar shows compact node or seat progress and the user's current `session_child_first` key binding, with `/subagent` as the command fallback. The detailed view keeps workflows and Councils as jobs, never fabricates child sessions, and shows each durable node or seat plus report-delivery and safety diagnostics.

`job_cancel` verifies parent ownership and requests cancellation from the live supervisor. It never pre-settles a job and has `ToolReplayPolicy::Never`; the executor records `cancelled` only after the child session or complete product process tree has stopped. Product protocol or process loss after work may have begun records `uncertain`. A restart reconciles still-running product jobs to `uncertain` and never replays them.

Codex and Claude Code retain ownership of their native login, configuration, and model choice. Zuno inherits the session directory and proxy environment but never copies product tokens into `AuthStore`. See [Codex and Claude Code product agents](design/product-agents.md).

## Concurrent web search

`web_search` accepts only `queries: string[]`. The consumer deduplicates queries by first occurrence, runs the remaining requests concurrently through a single-query `WebSearchProvider`, and combines cancellation with the turn interrupt.

The first failed query cancels its siblings and waits for every request to settle before returning. Successful output is deterministic regardless of completion order: query content follows input order, sources are merged by rank round-robin, duplicate URLs are removed, and profile-owned query, result, and timeout limits are applied.

Provider adapters normalize transport output into `SearchResult` and `SearchSource`; they do not own batch scheduling or model-facing presentation.

## Network egress

`zuno-network` owns the outbound HTTP construction policy shared by providers,
authentication, catalogs, remote instructions, remote MCP, and web tools.
Session traffic uses `ProxyPolicy::Environment`, which resolves the standard
HTTP, HTTPS, all-proxy, and no-proxy environment variables when a connection
pool is constructed. A capability that constructs reqwest directly bypasses
this product contract and is incomplete.

`ProxyPolicy::Direct` is an explicit security boundary, not a fallback. It is
used only for local control-plane probes and cloud metadata endpoints. Bedrock
therefore has two transports with separate lifecycles: runtime and SSO traffic
is proxy-aware, while IMDS and approved local ECS credential endpoints are
direct. Remote HTTPS container credential endpoints remain on the proxy-aware
transport.

Child processes inherit the process proxy environment unless their typed
configuration deliberately overrides a variable. The agent loop does not
rewrite process globals per session; deployment-specific proxy choices belong
in the environment that launches the Zuno process.

## Building a harness

Use `zuno_harness::profile_with_tools` to combine an `AgentDriver`, `ToolManifest`, and native `ToolContributions`, then add more `ProfileBundle` values for typed capability providers:

```rust
let profile = zuno_harness::profile_with_tools(
    "review",
    Arc::new(ReviewDriver::new()),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep, BuiltinSlot::Task])?,
    ToolContributions::new([Arc::new(ReviewSummaryTool::new())])?,
)
.with_bundle(zuno_harness::orchestration_capabilities_bundle(
    Arc::clone(&capability_snapshot),
));

runtime.activate_profile(profile).await?;
```

Registrations are effects: a component registers each acquisition in
`PrepareContext`, and the runtime owns the returned disposer. Tokio tasks are
cancelled and joined, process trees are terminated and reaped, protocol sessions
are closed before transports disappear, and registration handles remove exactly
what they added. `Drop` is only a last-resort safety net and does not prove a
successful unload. Deployment choices belong in profile configuration rather than
hardcoded branches in the agent loop.

## Client surfaces

The TUI, headless CLI, server, ACP adapter, and future GUI consume the same
commands, durable events, inbox, and frontend-neutral projections.
`ActivityProjection`, `WorkStateProjection`, `SessionUsage`, and
`BackgroundExecutionProjection` prevent clients from rebuilding agent-loop state
privately. Cursor replay closes gaps after disconnects; live delivery is only a
wake/latency path. See [client interface architecture](design/client-interfaces.md).

The design sources and explicit adopt/adapt/reject decisions are recorded in [the harness comparison](design/harness-comparison.md).
