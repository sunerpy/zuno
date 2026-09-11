# Harness Runtime

Zuno assembles an agent from a native harness profile. A profile is a set of bundles, and each bundle contributes typed components to one scoped runtime.

## Runtime model

- `Component` is the lifecycle unit. `prepare` is side-effect-free: it stages typed
  services, requirements, and deferred effects in a `PrepareContext`.
- An effect starts only after the complete candidate composition has prepared. Its
  start returns the exact asynchronous disposer that must prove quiescence;
  `Component::stop_budget` declares how long the runtime waits for that proof.
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

Each component declares how long the runtime waits for its disposers through
`Component::stop_budget`. The default, `StopBudget::Runtime`, defers to the runtime's
configured stop timeout; a component whose disposer must terminate and reap a process
tree, drain a socket, or wait for a flush returns `StopBudget::Bounded(duration)` with
the bound that work actually needs, because a runtime-wide timeout sized for closing a
channel reports such a disposer as an overrun on every shutdown. A zero bound cannot
prove that anything reached quiescence and is read as `Runtime`. Disposers still run
last-in-first-out, one at a time; the budget bounds only how long the runtime waits for
each before it records the overrun and moves on. A disposer that overruns its budget is
reported as a `TimedOut` lifecycle diagnostic and is not cancelled: dropping it
mid-flight would abandon whatever it was reclaiming, a half-terminated child process or
a lock never released, which is the one outcome worse than a late stop, so it is
detached and keeps reclaiming in the background. A disposer that returns an error or
panics is reported as `Uncertain`.

A deployment may bound every one of those waits with `runtime.max_component_stop_ms`,
which the runtime applies as `min(declared, ceiling)`; absent or `0` means this host
imposes no ceiling, which is the default. The ceiling only shortens the wait — the
disposer is still detached rather than cancelled, so a clamped process-tree reap still
runs to completion in the background.

`RuntimeSnapshot` and `ComponentSnapshot` expose lifecycle state, effect ids,
provided/required service types, and scrubbed diagnostics without coupling a
client to the runtime implementation. The TUI projects this inventory today; the
same value is available to future server, ACP, and GUI surfaces.

The background TUI supervisor is a process-lifetime owner outside the agent loop. It
binds a password-protected loopback server, creates the normal TUI as a retained PTY
child, and lets attachments come and go without closing that child. Detach is not session
close; explicit PTY removal or supervisor shutdown is.

### Principal attribution

The preview foundation carries an immutable `PrincipalScope` from `TurnContext`
through dispatch to tool permission origins and composed calls. Tenant, subject,
calling application and policy revision are separate from session and tool names.
Changing a tool's public session fields or argument metadata does not change its
captured attribution.

The existing local profile constructs an explicit local scope. Enterprise hosts
must authenticate a caller before supplying another scope; serialized scope data
is not an authorization grant. Ownership checks do not replace current operation
policy or shared-resource ACLs. Format 13 stores private session ownership
independently from editable metadata. Remote authentication remains a separate
enterprise implementation stage.

### Turn persistence

The shared loop accesses durable state through the asynchronous `TurnPersistence`
port. `TurnContext::new` installs the local SQLite adapter;
`TurnContext::from_persistence` accepts a provider selected by the host's backend
bundle. A turn retains that provider and its owner/session scope throughout
execution. The loop no longer retains a SQLite connection or performs SQL during
model and tool orchestration.

The port owns history reads and repair, exact prompt receipts, provider bookkeeping,
assistant-step commits, tool handoff/result commits, inbox consumption and bounded
driver admission/settlement. Assistant metadata, ordered parts and cumulative usage
commit together. Repeating that step does not count usage twice, and message/part
identities cannot overwrite another session's records. Ownership is checked before
reading model context or initiating any provider request.

Provider observations may await the state service. Attempt start commits before
provider I/O; a retry deadline commits before waiting. Provider events and their
usage/backoff updates share one transaction. Tool handoff commits before execution,
and result commits check the original call, tool and parameters. A settled result
cannot be replaced with a different outcome. A completed parallel group's results
commit as one ordered batch. If a state-service acknowledgement consumes the retry
window, no replacement provider request starts after that deadline.

Attachment validation/admission happens before the inbox transaction. The store
assigns the input's durable order and consumes the inbox with its message and parts.
Dynamic-context refresh is an asynchronous host service, awaited after a committed
tool result, with its own scoped storage access.

The ordinary driver consumes SQLite and fenced PostgreSQL providers. The latter
rechecks current organization authorization and execution ownership, and commits
driver checkpoints with native Job handoff/settlement in one transaction.
In-process records are not a public Web wire protocol. The separately versioned
Worker codec and HTTPS client now connect this port to authenticated state routes;
service identity and signed Job scope are checked independently from current
database authority. The Worker client has no PostgreSQL dependency. Environment
receipts and enterprise launch/profile assembly are still required for a runnable
distributed deployment. A lost state-service acknowledgement pauses recovery;
it never authorizes mechanical effect replay.

### Bounded driver checkpoints

The preview adds `AgentDriver::advance` to the same provider/tool loop used by
`drive`. The default driver advertises `supports_advance`; hosts must select a
compatible driver before admitting recoverable work. `AdvanceRequest` bounds the
number of provider steps and binds the original turn request to a resolved,
immutable configuration digest. It does not authorize the caller.

`AdvanceOutcome` distinguishes a committed checkpoint, completion, interruption,
a local human-request result, and a durable invocation wait. A complete step
yields after its tool results and history repairs are durable. A deferred tool
phase instead commits its exact unfinished calls and waits before yielding.
The checkpoint retains step/tool counters, disjoint usage,
elapsed wall-clock time, dynamic context, prompt receipt identities and unresolved
recovery obligations. Process handles and provider caches are reconstructed.
Resuming the same turn neither emits another turn start nor resets its limits.
Checkpoint schema 3 retains the tool-phase cursor and database-clock start anchor, so time between
advances counts toward the same wall-clock allowance; a clock rollback cannot
reduce already-accounted time. Unpublished schema-1/2 checkpoints are conservatively
refused, with their transcript and evidence retained.

The local journal compares the latest driver event in a short SQLite transaction
before admitting an advance, then conditionally commits its checkpoint or terminal
outcome. A repeated request after a lost response returns that committed result.
Wrong owners, changed configuration, stale references, unsupported schemas and
in-flight advances without a completed checkpoint fail closed. Checkpoint bodies
are internal state, limited to 8 MiB; clients carry stable references.

The checkpoint contract has SQLite and PostgreSQL providers and authenticated
Worker transport, sharing admission and immutable request validation.
`PreparedToolDispatch::Pending` carries a typed `WaitRef`; it never supplies an
interim successful tool result. `WaitCompletionStore` publishes the authoritative
completion. Driver admission atomically consumes that fact, writes the original
result and advances the checkpoint before remaining tools or models run.
The PostgreSQL wait releases the Worker lease while retaining the logical Job.
History repair rejects attempts to overtake an exact protected tool checkpoint.

Expired claims can resume only when the latest durable driver event is still
exactly the Job's committed checkpoint. Newer in-flight advances retain uncertain
recovery. Waiting counts toward the same turn allowance; remaining tools recheck
that allowance and current authority. See [durable invocation waits](../enterprise/WAITING.md)
for producer boundaries, atomic transitions and current delivery limits.

Environment/tool assembly and distributed child producers remain later stages. A checkpoint
does not move a running process or prove that an external side effect stopped.
The host must resolve the recorded configuration and recheck current authorization
before entering each advance.

### Scoped application service

`zuno-application` owns the `AgentApplication` facade and asynchronous
`SessionPersistence` port. The current service creates, reads and pages sessions,
and queues native text inputs. Its client DTOs carry logical session, request and
workspace IDs; host directories, permission overrides and owner overrides are not
accepted. A host authenticates and authorizes before constructing a scoped view.

`SqliteSessionPersistence` binds a principal to host-registered local workspaces.
Views share a connection pool and bounded blocking capacity, while every lookup
and list applies ownership in SQL. Paging uses both update time and session ID,
so equal timestamps do not lose rows. Idempotency keys include the principal,
calling application and operation scope. Reusing a key with different input
conflicts; retrying after a title edit reads the current resource without replacing
its original creation receipt.

Session creation, ownership and its audit event commit together. Input admission
and caller attribution commit with the existing native inbox event. The queued
payload keeps the existing `user` shape and captures the session's Agent/model
selection. Admission is not execution: Job scheduling, input-version CAS, steering,
enterprise authorization and Memory/backend coordination remain separate work.
This local binding is not an execution sandbox or a multi-user filesystem.
Cancelling an API future does not release its blocking capacity until the database
operation actually finishes.

### Durable runtime store

`RuntimeStore` and the profile-installed `JobDispatcher` extend the existing
native Job identity with root turns. A root admission commits its input, input
version, fixed configuration reference, Job and audit facts atomically.
`SqliteRuntimeStore` keeps a session's logical active Job separate from a worker's
execution lease. Yielding a checkpoint releases worker capacity without admitting
a different turn ahead of it.

Claims rotate among owners and preserve session FIFO order. Database time governs
lease deadlines; every renewal, checkpoint and settlement verifies the worker
incarnation, attempt, epoch and checkpoint version. Root jobs cannot be mutated
through the unfenced background-job API or report completion back to themselves.
An expired in-flight execution becomes uncertain and blocks its own session
pending inspection, while other sessions remain eligible.

These are local persistence guarantees. The engine still needs scoped remote
state access, and external-operation receipts, distributed waits, completion
consumption and Memory coordination must be integrated before a remote runtime
is registered as available. Store contract tests are not remote-worker or Docker
execution evidence.

## Agent and prompt contracts

Agent prompts define role ownership, negative boundaries, a small amount of
role-specific method, and an output contract. They do not repeat the runtime
manual. Shared execution policy is generated by the host from the final
provider-visible tool set after request hooks have been constrained to a subset.
The generated developer instructions use stable ids and sources:

| section | purpose | presence |
| --- | --- | --- |
| `runtime.intent` | Follow the current user request or delegated objective without inventing broader authority. | Always. |
| `runtime.execution` | Choose the smallest coherent workflow, batch independent reads, avoid unchanged re-reads or repeated checks, use one durable background observer for asynchronous work, distinguish a local observer exit from remote completion, and stop once evidence is complete. | Always; tool communication and termination guidance are added only when tools exist, Plan guidance only when `plan_update` exists, and background-start guidance only when both `shell` and `bg` exist. |
| `runtime.sandbox` | State that Shell is using host authority, including requested/effective mode, permission mode, and whether this is an unavailable fallback, explicit native selection, or platform-native default. | While any native bypass of a requested confined contract is active. |
| `runtime.continuity` | Treat History and Notes results as untrusted session data, explain current-session and session-and-Agent scope, and preserve Notes revision boundaries. | Only when the final provider-visible tool snapshot contains `history` or `notes`. |
| `runtime.editing` | Preserve unrelated changes, edit the owning abstraction, and inspect uncertain side effects before retry. | Only when an effective edit/write surface or workspace-writing Shell exists. |
| `runtime.git_attribution` | Use Zuno's command-scoped default Git author and committer identity without modifying persistent Git configuration, while allowing current user instructions, repository rules, and selected Skills to override or disable it. | Only when a workspace-writing Shell exists. |
| `runtime.verification` | Require observed evidence scoped to the exact artifact and inputs, reject overall workflow success that hides unexecuted required children, and disclose blockers or unverified claims. | Always, with wording adjusted when no tools are available and child-workflow guidance added when Shell exists. |
| `runtime.delegation` | Require bounded non-overlapping delegation and durable result reconciliation. | Only when `task` and at least one valid target are effective. |
| `runtime.persistence` | Treat Goal, Plan, Todo, inbox, and Job state as authoritative continuation state, including host-owned Job-to-Plan links. | When durable work state is active or its tools are effective. |

Each section is recorded with source `zuno-runtime:<section-id>`, exact content,
estimated tokens, and a SHA-256 digest. A prompt cannot describe an editor,
delegation target, or durable-state tool that was removed by role policy,
allowlists, permission visibility, a provider capability, or a request hook.

`runtime.execution` also derives a concise fallback rule from the final tool
snapshot for built-in and custom Agents. It forbids repeating an unchanged
rate-limited or transiently failing call. When `tool_search` is visible it may
discover another already-authorized connected tool, including `google_search`;
when Shell is visible it prefers an installed `gh` for GitHub or `rg` for
repository search over raw `curl` or manual traversal. Absent tools receive no
guidance, and fallback cannot widen permissions.

Work-mode Plan use is value-based rather than phase-count-based. A bounded
single-owner task does not create a Plan merely because it inspects, edits, and tests.
Cross-component dependencies, delegation, independent gates, interruption recovery, or
explicit ownership structure may make one useful. Active Goal criteria already provide durable gates, so
the runtime asks for an additional Plan only when it adds ordering, ownership, or restart
value. An existing Plan remains authoritative and is updated on material transitions.

Remote delivery guidance is split along the same ownership boundary. The
`github-delivery` Skill carries GitHub-, Actions-, artifact-, and release-specific
method only when relevant. The runtime owns the generic safety contract: a Shell
command that only observes remote work uses `background: true` with
`backgroundPurpose: "remoteObserver"`. Its durable terminal report wakes the
session, but does not prove the remote workflow or release succeeded. The
resumed turn must inspect the retained output and re-query authoritative remote
state by a stable run, attempt, ref, or release identifier. Required child jobs
that were skipped, cancelled, missing, or never expanded are not execution
evidence unless an explicit repository policy marks them optional.

If durable Plan, Todo, or Job work remains while that remote observer is still
running, reconciliation records `waiting_background` and ends the current turn
without polling or creating a generic human request. An active Goal is not auto-driven again while
the observer remains live. The existing process-owned completion watcher admits
the terminal report and wakes the session, which then refreshes authoritative
remote state and resumes normal reconciliation.

Git commit attribution follows the same prompt-owned, auditable policy boundary.
For commits Zuno creates, the fallback author and committer use the
[`zuno-agent`](https://github.com/zuno-agent) name and
`zuno-agent@firlab.app` email. The agent applies the fallback to one command with
`git -c user.name=... -c user.email=...` and does not alter global or repository
Git configuration. Git commit objects store a name and email, not a profile URL.
A current user instruction, applicable repository instruction, or selected Skill
can replace or disable the fallback. An amend preserves the existing author
unless an explicit instruction requests a reset; the fallback applies to the new
committer. Zuno does not add a second co-author or generated-by trailer unless
instructed.

This adapts the official Codex
[GitHub Action](https://learn.chatgpt.com/docs/github-action) and
[non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode)
guidance: repository-owned prompts, least privilege, machine-readable output,
retained artifacts, and separation between read-only analysis and credentialed
writes. Zuno keeps organization-specific branch, approval, signing, and release
policy in repository instructions or user Skills rather than compiling an
`auto-release` workflow.

After request hooks, runtime context, replayed history, attachments, and the
final tool schemas have been applied, the engine estimates the complete
provider-visible input. When the resolved model has a known context limit and
that aggregate estimate exceeds it, the turn fails with a typed prompt-assembly
error before provider I/O. Zuno does not make the request fit by truncating
instructions, history, selected Skills, or tool schemas. An unknown model
context limit leaves this final enforcement to the provider.

The built-in role prompts remain intentionally small:

- `orchestrator` handles one clear action directly and constructs a dependency
  graph only when bounded specialization or parallelism has value. It owns
  integration, conflict resolution, and the final audit. A repeated failure at
  one integration boundary forces a recorded end-to-end
  producer/artifact/consumer hypothesis reset before another release or
  deployment. Verification is tied to the exact commit, build, tag, deployment,
  configuration, and inputs; asynchronous external work keeps one authoritative
  observer rather than overlapping poll loops.
- `build` owns one end-to-end implementation lane and cannot delegate.
- `deep` owns reproduction, ranked hypotheses, causal tracing, root repair, and
  recovery verification without recursive delegation.
- `plan` is read-only and produces a decision-complete plan from observed facts,
  necessary decisions, implementation design, and acceptance evidence.
- Specialists use concise natural Markdown and may use
  `Outcome`, `Evidence`, `Inspected/Changed`, and `Risks/Blocker`; the model is
  not required to invent a JSON or XML report protocol.

Durable planning separates collaboration mode from the optional Work-mode
checklist. The default profile publishes a typed `HostPlanningCapability`;
custom profiles opt in explicitly. Before the first provider request, the host
applies state policy shared by CLI, TUI, ACP, server, and child turns:

- explicit `plan` collaboration mode is `Required`;
- an existing active Plan is `Maintain`;
- ordinary Work input is `Optional`, including structured image, resource,
  selection, branch-diff, and multi-block context;
- host-generated input with no active Plan, or an empty input, is `Atomic`;
- a hidden `plan_update` surface is `Unavailable`.

The host never parses action verbs, question words, acknowledgements, or
language-specific scope markers to guess complexity. In Work mode, model
instructions use a Plan only for meaningfully multi-step coordination, ordering,
delegation, or recovery; they skip straightforward and single-step work and
never create a single-step Plan. Thus `OK, apply that change` and a complex
cross-component request reach the same optional tool surface, while the model
chooses whether durable coordination adds value from the full conversation.

For explicit Plan mode, the runtime tells the model to read the current Plan and
use operation-based `plan_update`. It calls `plan_get` immediately before every
mutation and copies the returned current revision into `expected_revision`.
Only the first `create`, after `plan_get` returned `null`, may omit it:

- `create` creates the first strategic Plan, or replaces the visible root for a
  genuinely new objective. The host generates every step id. Replacing an
  existing Plan requires its current `expected_revision`; the old root is
  archived as completed or superseded.
- `patch` changes only the title or named step ids.
- `append` adds new host-identified steps.
- `push` persists a focused child Plan while suspending the exact parent.
- `pop` carries only `expected_revision`, archives a terminal child, and restores
  the exact parent once.

Every mutation of an existing Plan is revision guarded. `completed` and
`superseded` are terminal states. Todo items are optional dynamic execution
detail beneath strategic Plan steps; they must not mechanically mirror every
step. Completed verification remains immutable evidence, so a changed commit,
build, tag, deployment, configuration, or other relevant input requires a new
artifact-scoped verification step.

Work-state tools participate in two typed engine contracts. Reads publish a semantic
`ToolProgressObservation` whose fingerprint contains authoritative Plan or Todo state,
not free-form call narration. Mutations publish
`ToolDynamicContextRefresh::WorkPlan` or `WorkItems`. If another provider request will
run, the engine passes the committed connection to a host-owned
`DynamicContextRefresher`; the CLI host regenerates Goal/Plan/Todo/Job context from SQL,
and a Plan mutation replaces the one-time Required instruction with Maintain.

Machine execution state does not leak into the visible Plan. The
`PlanReconciliationDriver` persists `idle`, `executing`, `reconciling`,
`waiting_retry`, `waiting_background`, `paused`, and `terminal` phase
events in the existing session event log. Before a successful answer is
delivered it evaluates only typed Plan, Todo, Job, Goal, background-observer,
tool-result, and verification state:

- a session that recorded no durable work finishes on its first answer;
- terminal Plan state with no active Todo or Job may finish;
- a completed answer from the built-in read-only `plan` Agent is a typed
  planning handoff: its current Plan and Todos remain durable for Start Work
  and do not trigger execution reconciliation. A matching Plan-authorization
  wait permits the source turn to finish its summary without authorizing Work;
- a live background execution or required child Job can register an exact
  external source/cycle wait without consuming reconciliation attempts;
- an active Goal owns the next durable continuation;
- authorized ordinary Work continues from a durable `Recovery` token only when
  the host proves executable Todo/dependency work. Unfinished Plan steps and
  blocked Todos alone produce `no_executable_work`, not another provider call;
- the driver hashes authoritative Plan, Todo, Job, and Goal revisions into a
  progress fingerprint; three consecutive identical fingerprints pause with
  typed `no_progress`;
- no reconciliation branch manufactures a generic human confirmation request.

Unreconciled work means durably recorded work. A Work-mode `Optional` decision
is not recorded work, so a request that creates no Plan, Todo, or Job settles
rather than being driven again.

A process restart preserves the session-level fingerprint and unchanged-progress
count, even across callback cycles. `session_execution_state.scheduling` owns the
ready, exact human/external wait, paused and completed gates for ordinary sessions
and Goals alike. A pause is committed to that row, not only a Goal store or an
event projection. Callbacks may be recorded while paused but cannot reopen it.
A status query can run without resuming work; explicit `/resume` queues a Work
control but cannot waive a pending wait or approve a Plan. Assistant prose is
never parsed as evidence that work completed or as a substitute for a typed wait. Hiding
`plan_update` prevents the model from creating or mutating a new strategic Plan;
existing Plans remain durable, projected, and recoverable.

ACP mirrors this state through a session-owned projection pump subscribed to
`TurnHost::work_state_changes()`. A wake causes an authoritative Plan read and a
complete stable-V1 update; `(plan_id, revision)` prevents duplicates and stale
revisions, and a missing Plan emits empty entries to clear client state. Live
changes, prompt-terminal flush, load, resume, detached continuation, and host
remount all share this projector. The adapter does not infer Plan changes from
tool names.

Native Task admission also captures the active Plan location in the Job's
versioned `workContext`: optional Goal id, Plan id, Plan revision, and Plan step
id. A Plan step cannot complete while one of its linked Jobs is queued, running,
uncertain, or still has an unconsumed report. Completed and failed child evidence
remains in `runtime.work_state` until that linked step reaches a terminal state.
An explicit new Goal objective may supersede the step, but it never settles or
cancels the linked Job implicitly.

Root-session messaging is a separate durable-input capability. `session_message` exists
only when the turn has no immutable parent tool authority, and execution verifies again
that the source is a root. Same-project roots may address one another, and a root may
address its own descendants; foreign children, archived targets, self targets, and
cross-project targets are rejected. TUI, ACP, and server drivers consume the same
`sessionMessage` shape. An online process may steer at a safe point; otherwise the row
remains queued.

Plan and Work are also typed collaboration contracts. `collaboration.mode` is a
runtime-trust prompt block, separate from the native kernel, agent role, project
instructions, work state, and user input. Plan tells the model to inspect and
update durable planning state without product mutation; Work tells it to execute
against the durable Goal, Plan, Todo, Job, and queue projections. Neither block
grants capabilities or authorizes a mode transition by itself.

Static tool descriptions live in dedicated text files and are byte-pinned by the
prompt golden test. Prompt changes are therefore reviewed as model-visible
behavior, while schemas, permission policy, replay policy, and execution remain
independently testable. Operation-tagged parameter enums (`plan_update`, `notes`,
`history`) reach the provider as one object schema whose `action` property
enumerates the operations and is the only schema-required field; the typed
deserializer enforces each operation's own fields. A field whose shape differs
between operations is sent in its nullable form when one operation merely makes it
optional, with each operation's description attributed to it when only the
descriptions differ, and as an `anyOf` of the distinct shapes otherwise.

An Agent has no implicit turn-step ceiling. `agents.<name>.steps` may explicitly
set a positive maximum number of tool-capable provider steps. Reaching that
limit closes tool authority and permits exactly one additional text-only
provider request with a host instruction to summarize completed work, remaining
work, evidence, and blockers. The finalization request and instruction digest
are persisted in `session.provider.request.1.stepLimitFinalization`. If that
single finalization still cannot terminate cleanly, the turn ends with a typed
step-limit failure.

Zuno does not inject a convergence instruction after an arbitrary number of
tool calls. Tool-capable prompts instead require a short preamble before a
substantial batch, concise progress updates at meaningful milestones, and a
specific evidence gap before more tool work. Once the outcome and evidence are
complete, the Agent must stop calling tools and answer. The provider-driven loop
continues while the model requests follow-up work, subject to interruption,
context management, durable goal state, and an optional operator-configured
step ceiling.

There is one narrow host convergence guard: three consecutive successful single-tool
reads carrying the same semantic work-state observation end with
`StagnantToolLoop`. The third result is persisted and projected before the error. A
different tool, failed/blocked/interrupted result, written path, continuation request, or
changed observation resets the sequence. Recovery is `Pause`, and an active Goal records
`no_progress`; automatic retry would only repeat the same paid read.

### Instruction file admission

Repository and user instruction files are admitted whole or not at all. A local
rule file the host cannot read still fails the turn before the first provider
request with a typed error naming the file and remedy. An intact entry that does not fit the instruction budget
— 64 KB, or a quarter of the model's context
window when that is smaller — is instead skipped atomically. The host reports
the `warning` notice `instruction.not_in_force` with source, byte count, budget,
and remaining space, then considers later smaller entries normally. It never
truncates a rule file, and an oversized `AGENTS.md` does not prevent ACP, TUI,
server, or CLI startup. A remote instruction source that could not be fetched
uses the same non-fatal notice because network availability must not decide
whether the Agent runs.

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
`webfetch`/`web_search`, and `shell`. `shell` inherits the Zuno process
environment and host visibility and therefore remains a side-effecting,
approval-governed capability. A workflow that requires one custom agent
explicitly calls `task` with that `agent` and a complete typed delegation
contract.

Providers, drivers, approvals, and arbitrary typed services remain trusted
compiled Rust `Component` implementations mounted through a `HarnessProfile`.
See [plugins, custom agents, and workflows](plugins.md) for manifests,
capability tables, protocols, and runnable examples.
See [developing agents and extensions](guide/extension-development.md) for the
complete WASI guest and native Rust implementation paths.

## Native agents

The built-in catalog separates primary modes, delegable specialists, and hidden engine agents:

| agent | role |
| --- | --- |
| `orchestrator` | Default multi-agent delivery owner, and one of the two native agents that may delegate. |
| `build` | Direct end-to-end implementation in one lane, with all subagent tools withheld. |
| `plan` | Read-only repository research and implementation-ready planning. |
| `review` | Read-only high-assurance review: `review_open` automatically runs the bound `balanced-review` Council, records anchored evidence, and gates draft against ready. |
| `deep` | Directly selectable or delegable deep debugging and cross-cutting implementation, without recursive delegation. |
| `fixer` | Focused local implementation with minimal change and regression scope. |
| `general` | Bounded miscellaneous execution when no narrower specialist owns the work. |
| `explorer` | Read-only repository structure, definition, caller, and impact discovery. |
| `librarian` | Current external documentation, release, and upstream research. |
| `oracle` | Read-only architecture review, root-cause analysis, and explicit trade-off advice. |
| `looker` | Visual artifact inspection when a vision-capable model is available. |

`compaction`, `title`, and `summary` are hidden engine agents. A user-defined agent may be declared under `agents.<name>` or as Markdown under `.zuno/agent/**/*.md`; it enters the same resolution, permission, prompt, and provenance pipeline as a native agent.

`zuno-review` is a native Component publishing one typed `ReviewService`. It owns source
and artifact probing, typed seat reports, the event-backed review projection and the four
review tools. `review_open` invokes the configuration-owned `balanced-review` Council
through a host adapter; `council_run` is not exposed to the review model. A seat counts
only after its `DelegationEvidenceReport` parses, its repository-relative anchors are
reopened by the host, and a structured receipt binds the preset source, run, job, seat,
Agent, report digest and source snapshot. The receipt must also match the exact completed
seat in the durable Council Job. Ready requires at least two distinct seats from one
authoritative run, and its receipt binds all evidence-anchor digests. Finalization performs
a post-commit reconciliation. Each anchor observation deduplicates repository paths and has
one 32 MiB total read budget. Any later claim, issue, Plan, source, artifact or anchor mutation
advances the review revision and invalidates the earlier source-bound Ready receipt.

Agents have no fixed provider-step ceiling by default. A user who needs a
deployment guard may set `agents.<name>.steps` to a positive integer:

```json
{
  "agents": {
    "orchestrator": {
      "steps": 200
    }
  }
}
```

The configured number limits tool-capable provider iterations, not the total
lifetime of a goal. If the final permitted iteration still requests
continuation, the engine issues exactly one additional request with an empty
tool list and a volatile developer instruction to report what completed, what
remains, and any evidence or blocker. The `session.provider.request` event
persists that exact instruction and its digest under
`stepLimitFinalization`, so replay can reconstruct why the request was
text-only. A provider that still emits tool calls cannot extend that turn; the
protocol failure becomes a typed `StepLimit` recovery.

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
The child inbox commits the text before delivery. Ordinary busy input waits for an idle
FIFO wake; an explicit Send Now targets the displayed turn through precise admission.
an idle or completed child acquires a run lease and reopens a `TurnHost` with the resolved
Agent, model, effort, and inherited orchestration identity captured for that child. The
`SessionWakeCoordinator::deliver_when_idle` preserves queued order without implicit
steering. A stale precise steer is refused before admission, with its draft retained
by the TUI. Delivery belongs to the workspace
supervisor and is cancelled with that lifecycle. A direct child continuation updates only
the child session; it does not fabricate a report or another input for the parent.

Permission attribution comes from immutable coordinates captured by `ToolContext`, not
from a broker-wide current-session slot. A root or child request therefore carries the
session, assistant message, and provider call that raised it through every rule and human
approval layer. The foreground TUI broker serializes all such asks, scopes standing grants
to one session, and fails closed by rejecting pending requests when its last surface or
wake channel closes.

ACP consumes the same child observer without creating another child loop. The stable
projection is always the `task` tool card. A client that directly negotiates the draft
`subagents` capability additionally receives foreground child replay and live events on
the durable child session id, with spawn and terminal state on the direct parent route.
The parent prompt response waits for the child projection queue to drain. Historical
children replay as `disconnected`; background children remain durable jobs.

Child permission requests retain their immutable origin. A negotiated native ACP client
receives them on the child route. A compatibility client receives them on the declared
root route with the child id in typed metadata. Delegated children do not receive the
synchronous `question` tool: they report blockers to the parent, and any later user
elicitation initiated by the parent belongs to the root session. Session-level permission
grants are owned by that root and survive host replacement, but `session/close` clears
them and cancels and joins only background jobs owned by the closing root.

### Child capability authority and Skill loading

A child resolves through the same configuration, model catalog, MCP catalog,
Skill discovery roots, permission ceiling, and sandbox configuration as its
parent composition. In the absence of a per-Agent or preset route, the child
inherits the parent session model and reasoning choice. By default the
model-facing `task` surface cannot override model, effort, category, MCP, Skill,
or sandbox policy. `subagent_model_selection.enabled` may expose optional
`model` and `effort` fields under a separate host-global exact allowlist;
category, MCP, Skill, and sandbox policy remain host-owned.

The enabled state and canonical sorted allowlist are validated against the
active model catalog and persisted as a durable session policy event with a
digest. Every Attempt references that digest and each child inherits the same
snapshot, so later configuration edits cannot change an existing session.
Explicit effort requires an explicit allowed model and must resolve to a variant
that model actually declares. A `task_id` continuation may omit both fields or
repeat the first frozen values exactly; any change is rejected before child
execution.

A native child does not recompute an independent tool superset from the current
configuration. The parent Attempt persists the exact provider-visible tool schemas used
for its model request, and child resolution treats that frozen set of
`ToolSchemaIdentity` values as its authority ceiling. Matching only a wire id is
insufficient: a same-named tool whose provider-visible schema changed is not inherited
from that Attempt.

The target Agent role, its extension-tool policy, the configured exact `tools`
allowlist, and the effective global, user, and Agent permission rules are then
intersected with that ceiling. Each layer may hide or deny more tools; no later
`allow` or `permission.mode: "allow_all"` can restore a tool schema that the parent
model did not receive. `allow_all` affects HITL prompting, not capability construction.

`prepare_request` hooks run after the registry snapshot is locked. They may remove or
reorder tool schemas, but the engine rejects any added, replaced, or duplicated schema
before provider dispatch. The durable Attempt therefore records the exact post-hook set
without allowing the hook seam to widen registered authority. If a hook removes a schema
that retained history still references, the request does not fail locally: the historical
call becomes an inert assistant record and its result becomes explicitly untrusted user
data for this request only. Current-turn calls remain native until their result
continuation settles. Historical `ToolUse`/`ToolResult` blocks have a separate locked
sequence and role snapshot: hooks may still edit prose, but cannot add, remove, replace,
duplicate, reorder, split, insert another message inside a native call/result pair, or
re-role native tool protocol history. Stored identity failures and post-hook declaration
removals are combined before one occurrence-ordered fallback projection, so a mixed
parallel batch keeps its durable result order.

Root MCP exposure is resolved after permission, allowlist and parent-schema filtering.
`mcp_tool_exposure` defaults to `auto`: small whole services are direct within count/byte
budgets; `eager` and `deferred` can override globally or per configured service.
This policy is independent of transport configuration and never changes
`McpConnectionIdentity` or shares external clients across sessions. It adapts Codex's
direct/deferred exposure and source-listing design (inspected at `9ba1d9eb`, in
`tools/src/tool_executor.rs` and `core/src/tools/handlers/tool_search_spec.rs`) to Zuno's
durable tool snapshots and bounded service catalogs.

Deferred MCP implementations stay executable in the dispatcher while `tool_search`
advertises original service names and bounded capability metadata; each
successful search increments a turn-local revision and expands the frozen provider tool
snapshot on the next step. The completed `tool_search` result is also the durable
session exposure ledger. Rebuilding a host for a detached report, process restart, or
client remount restores those ids before the first provider request, intersected with
the currently connected, permission-visible catalog; unavailable ids do not regain
authority. Unversioned registry drift remains ignored, while stale revisions cannot roll
the snapshot back. An exact Agent `tools` allowlist pins its named MCP schemas eagerly.
ACP session-local `mcpServers` also pin their schemas eagerly after the strict connection
gate, while host-configured servers follow the exposure policy. The catalog
carries that session boundary into child and background turns. Child turns do not receive
a fresh deferred superset: schemas that survived the parent's exact Attempt authority are
eager inside that already-bounded ceiling.

The generated discovery tool must itself be visible. Native role grants include it
before user overrides; if it is denied/disabled or its name collides, allowed MCP
schemas remain direct rather than becoming unreachable. Search never restores a
permission-hidden tool. `Tool::source` retains original service attribution without
reverse-parsing sanitized wire ids. Runtime guidance proactively prefers relevant
authorized MCP capabilities, distinguishes configured/connected/cached/deferred states,
and does not treat configuration or extension/resource listing as tool discovery.
`customize-zuno` explicitly excludes ordinary use of already-configured services.

Every new tool part records the exact provider-visible schema identity beside the call.
Before a request, retained history from earlier turns is checked against the current
post-hook definitions. Current-turn tool continuations always keep their native pair so
an unknown or refused call can receive its protocol-complete result. For earlier turns, a
matching replay schema preserves native tool-use/result protocol. Replay hashes remove
annotation-only JSON Schema keys such as descriptions, titles, examples, comments, and
defaults while retaining required fields, types, enums, and every other value constraint.
Normalization follows schema and subschema positions only: property/definition names and
objects inside `const`, `enum`, or unknown extension values remain intact even when their
keys happen to be `description`, `title`, or another annotation name.
Older identities without a replay hash still require exact description and schema hashes.
A missing tool, a structurally changed schema, or an unreadable identity is replayed as
bounded inert JSON text for that request; arguments and results are UTF-8-safely bounded
per field and across the request before the fallback object is serialized. Durable
history is not rewritten and an unavailable implementation is never advertised as
callable merely to satisfy replay.
An older Zuno binary does not recognize the additive `replaySchemaSha256` field and
therefore fails closed to inert history after a downgrade; the stored call is not
corrupted or rewritten.
For released rows without an identity, Zuno first recovers the exact hashes from the
immutable provider-request Attempt keyed by the assistant message; if that proof is
absent, the call is downgraded even when a same-named tool is currently active. This
closes the `missing_tool_declaration` failure mode without silently binding an old call
to a new schema. Tool-free internal compaction applies the same rule more broadly:
tool calls and results enter the summarizer as bounded inert JSON text, never as native
function protocol without declarations.

Goal, Plan, and Todo state tools use `AuthoritativeState` history policy. When one of
their old declarations is incompatible, its historical call/result pair is omitted
instead of converted into model-visible prose; the current typed state comes from
`runtime.work_state`. Generic tools retain exact-declaration fallback behavior. Replay
repair notices have diagnostic audience: they are de-duplicated per session, context
epoch, tool, and stored/current identity, written to structured logs, and excluded from
ACP thought chunks, TUI conversation rows, and HTTP event history.

MCP and extension tools therefore do not flow unconditionally into every child. An
exact schema must be present in the parent Attempt and no later allowlist or explicit
deny may remove it. Work-capable native roles may opt into automatic extension
inheritance. Read-only roles deliberately do not inherit arbitrary dynamic tools
automatically; an operator may still grant one audited tool id with an exact per-Agent
permission rule. This avoids treating every unknown MCP operation as read-only merely
because the receiving Agent is read-only, without making a safe repository query
impossible to authorize.

Skill loading is separate from tool authority. Each initial or resumed child host
independently discovers the Skill catalog from its own working directory,
configuration layers, mounted profile, and active extensions. A Skill body already
loaded by the parent is not copied into the child prompt. Instead,
`agents.<name>.requiredSkills` names instruction sets that must resolve, after Agent and
profile visibility filtering, to exactly one source. Before each provider-bound input,
Zuno ensures those exact sources are loaded and de-duplicates sources already present
in the durable prompt. A missing name or multiple visible sources with the same name
fails child startup; Zuno never silently picks the first discovery result.

The live Skill catalog watches exact project Skill roots rather than the
worktree itself. For every directory from the session directory through the
worktree, `.zuno/skill` remains a logical root unless project configuration is
disabled, and `.agents/skills` remains one unless external Skills are disabled.
The canonical user root `$XDG_CONFIG_HOME/zuno/skill` and explicit configured
paths are likewise preserved even before those directories exist.

A missing root registers only the nearest existing ancestor and always does so
non-recursively. After a filesystem event, the catalog consumer reconciles the
subscription outside the native watcher callback, moves it one or more
components toward the logical root, and enables recursion only when the exact
root exists. Every move installs the narrower subscription before dropping the
old one. Ignore filtering is event policy, not a substitute for bounded native
registration: Zuno never registers the whole worktree recursively merely to
discover a future Skill root.

Zuno does not watch `~/.zuno` or its remote Skill cache. The cache is private
download state created only by configured remote indexes. The standard shared
`~/.agents/skills` root is watched only when it already exists; explicit
`skills.paths` remains the hot-install mechanism for any other shared root.

For example, a code-retrieval Agent may declare
`requiredSkills: ["codegraph"]` so every child turn receives the CodeGraph operating
instructions. That declaration grants no executable capability. CodeGraph MCP tools
remain available only when their exact schemas are inside the parent Attempt ceiling,
the role either inherits extension tools or has exact per-Agent grants, its `tools`
allowlist retains them, and no effective permission rule denies them.

This authority model is informed by Codex's child-from-parent-effective-capability
design. Codex is a design source, not a compatibility target: Zuno does not promise
Codex configuration, role, MCP, Skill, wire, or runtime semantics.

### Typed delegation contract

The model-facing `task` tool no longer accepts loose `description`, `prompt`, or
`load_skills` arguments. Its required work agreement is:

```json
{
  "objective": "Locate the prompt receipt ownership gap",
  "deliverable": "A call path and minimal affected-file set",
  "instructions": "Inspect only; use structural code navigation.",
  "success_evidence": "Name the owning symbols and distinguish facts from inference.",
  "scope": {
    "include": ["crates/zuno-engine", "crates/zuno-cli"],
    "exclude": ["credential stores"]
  },
  "constraints": {
    "must": ["Preserve unrelated changes"],
    "must_not": ["Edit files"]
  },
  "dependencies": ["The CodeGraph index is current"],
  "agent": "explorer",
  "background": true,
  "reportDelivery": "nextStep"
}
```

`scope`, `constraints`, and `dependencies` are optional. `agent` selects one
member of the effective delegate roster. `task_id` resumes an existing child
session owned by the same parent. Unknown fields and removed loose arguments,
including `description`, `prompt`, `subagent_type`, `category`, `model`,
`effort`, and `load_skills`, fail validation; there is no compatibility
translation.

## Prompt provenance

Prompt assembly is ordered data, not string concatenation spread across the
CLI. Every section has a stable identifier, source, exact content, and SHA-256
digest. Present sections are sorted into stable semantic lanes:

1. kernel, when a profile contributes one;
2. native or configured Agent role;
3. typed Plan or Work collaboration mode;
4. capability-derived runtime policy;
5. global instructions;
6. project, configured, and nearby instructions;
7. Goal and other model-visible work state;
8. extension and routing policy;
9. selected Skill bodies;
10. the bounded Skill metadata index;
11. memory.

The trigger policy makes a named or clearly matching indexed Skill a pre-action
requirement. The base prompt carries bounded name, description, and source
metadata only for `index` exposure; `search` exposure remains available through
the `skill` tool, and `explicit` exposure requires `$name`, `/<name>`,
`requiredSkills`, or an exact load. Descriptions are shortened before a source
identity is omitted. `skills.maxContextTokens` overrides the default character
budget equal to two percent of a known context; an unknown context falls back
to approximately 8,000 characters, while `skills.includeInstructions: false`
disables prompt catalog injection.

“Clearly matching” is specific to the trigger, not a synonym for “generally useful.”
The generic Git and verification Skills exclude bounded disposable edits whose acceptance
commands are already explicit; the runtime's own Git-safety and verification sections
still apply without spending a provider step loading duplicate workflow instructions.

Fully selected Skill bodies have a separate aggregate budget. By default it is
ten percent of a known model context, with a 2,000-token floor and a
32,000-token ceiling; an unknown context uses 8,000 approximate tokens.
`skills.maxSelectedContextTokens` overrides the derived value while retaining
the ceiling. Loading or restoring a body that would exceed the aggregate budget
fails before the provider request; Zuno does not silently omit part of a
selected Skill or reuse the metadata budget for full instructions.

The `skill` tool pages the `index` and `search` catalog with `list`, searches it
with `search`, reads a selected body with `load`, and resolves relative text
with `read_resource`. Unique names omit source locators; same-named enabled
sources remain distinct and require an advertised source locator. Reads use
content-bound cursors and must continue to completion; disk bodies are read
after selection rather than retained for the process lifetime.

Skill discovery is Zuno-owned. It advertises project `.zuno` and `.agents`
roots before Zuno's user-global config and user-global Agent Skills, then
configured paths and pulled URL caches. Zuno does not implicitly scan Claude,
OpenCode, or another product's directories; operators may select a shared
directory explicitly through `skills.paths`. Canonical paths are de-duplicated,
including symlink aliases, but same-named files from distinct sources remain
separate identities that require source-qualified selection. Unique names omit
their source path from the compact prompt index; ambiguous names retain it.
Discovery order controls presentation and provenance; it does not silently
choose a same-name winner.

Filesystem Skills may carry `agents/openai.yaml` shared metadata and a
field-wise `agents/zuno.yaml` override. Zuno consumes display name, short
description, and implicit-invocation policy; the native file may additionally
select `index`, `search`, or `explicit` exposure. Ordered `skills.config` path
rules have final precedence and can disable, re-enable, or reclassify an exact
Skill or recursive subtree. Existing path and symlink aliases resolve to one
canonical policy identity. Disabled sources never enter the runtime catalog;
client surfaces consume the same effective catalog and diagnostics.

A visible Skill whose name is unique across sources and does not collide with a
real command is also advertised as `/<skill-name>`. A bare invocation loads the
complete body, emits the loaded projection, and does not create a model turn.
Supplying arguments loads the Skill first and then admits the exact canonical
slash text as user input. Real commands always win; ambiguous names remain
available through `/skills` and source-qualified `skill` operations.

Reusable workflows belong in Skills by default. A Markdown command remains useful
for a literal prompt template or a short argument-expansion macro, but it has no
resource bundle, implicit trigger, or authority of its own. The first-party
`ui-design` workflow is a Skill and therefore receives a direct slash entry while
its name remains unambiguous. Named organization-specific review and release policy
is user owned: users may define `dual-review`, `auto-release`, or other named Skills
in global or project Skill roots, but Zuno does not compile those policy bodies into
the binary. The generic `balanced-review` council remains a reusable synthesis
primitive and does not prescribe either workflow.

### Repository instruction initialization

The command registry seeds two Zuno instruction workflows:

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

Before each provider request, the loop persists
`session.prompt.assembled.1`. The event records the ordered sections, semantic
role, trust, priority, source, byte and token estimates, content digest,
provider system/developer projection, and the actual post-hook projection when
it differs. Each `session.provider.request.1` started event points to the exact
receipt through `promptReceiptID`. Identical final projections reuse their
receipt id within the turn, including an `A -> B -> A` sequence.

### Prompt and Agent diagnostics

Inspect the receipt used by the latest provider request in one session:

```sh
zuno debug prompt --session <session-id>
zuno debug prompt --session <session-id> --step <non-zero-step>
```

Without `--session`, the command prints the latest prompt receipt in the
database. With a session, it first resolves the latest or selected
`session.provider.request.1`, then follows `promptReceiptID`; it does not guess
from a matching step stored in another receipt. Prompt bodies, system/developer
projections, and the post-hook system prompt are redacted by default while
section ids, sources, sizes, roles, digests, session ids, and event ids remain
visible. `--show-sensitive` reveals exact AGENTS, Skill, memory, runtime, and
hook-transformed model input and must not be pasted into an issue or log without
review.

Inspect current configuration-time resolution for one Agent:

```sh
zuno debug agent deep
```

The command uses the real `TurnPlan` and `McpRuntime` resolvers without creating
a session or contacting the model provider. It connects every enabled MCP
server, records lifecycle state, discovery status, connected servers, exact
current tool schema identities, warnings, and cleanup warnings, then closes
every transport before returning. Discovery failure, cancellation, or timeout
also runs bounded transport cleanup. Discovered tools are evaluated against the
current role rules and Agent allowlist. A root diagnostic has no parent Attempt
authority to invent; delegated historical authority must be read from that
Attempt's persisted orchestration snapshot. If no MCP runtime exists,
inheritance remains `not-connected` rather than being tested with fabricated
ids.

The output also reports effective model, reasoning and selected variant,
policy-visible and unavailable tools, delegates, sandbox readiness, and Skill
catalog counts, metadata/body budgets, bounded preview coverage, and ambiguous
names. Interactive `question` still requires a client asker, and a later request
hook may only narrow the final tool set.

### Provider request routing context

Foreground requests carry a private, typed `ProviderRequestContext` beside the
model-visible request. A root turn uses `MainTurn` with its durable session id;
a delegated child uses `ChildTurn` with the child's own durable session id.
Every continuation in one tool loop reuses that same context, and resuming the
durable session after a process restart reconstructs the same identity.

Title generation, lifecycle summaries, compaction, learning extraction, and
Council synthesis use explicit isolated purposes with no foreground-session
identity. This prevents lifecycle work from joining either the root or a child
provider conversation.

Only an OpenAI Responses wire surface projects the typed identity, as
`metadata.zuno_session_id`. Both the official OpenAI adapter and the compatible
adapter used by a custom OpenAI `baseURL` implement the same projection. Chat
Completions and Anthropic Messages do not receive a fabricated equivalent.
Unrelated object-shaped metadata is preserved, while
`metadata.zuno_session_id` is reserved and cannot be replaced through provider
options or request parameters. The routing context remains private to
`CompletionRequest`, so ordinary request hooks cannot mutate it or move it into
prompts, headers, or tool definitions.

Each foreground `session.provider.request` event records `requestPurpose`,
`affinityAttached`, and, when attached,
`affinitySource: "durable-session"`. It does not persist the raw routing
identity, credentials, or upstream account and conversation identifiers.

### Encrypted reasoning replay

Some Responses endpoints seal a step's reasoning into an opaque envelope bound to
one model, one account, and one conversation. A provider declares that capability
with `reasoningReplay: "encrypted"`, which is an endpoint option and never a rule
inferred from a provider id. Zuno then adds
`include: ["reasoning.encrypted_content"]` to every Responses request for that
provider and echoes each sealed item back on later requests. The declaration is honored wherever the request
resolves to a Responses surface: the catalog `openai` provider reaches it with no
declaration, and a gateway whose endpoint comes from a provider option reaches it
only by declaring `transport: "openai"` with `surface: "responses"`. Config
validation refuses the routing that provably cannot carry a sealed item, per
provider and per model, and accepts what already resolves to Responses.

The default is `off`, which sends neither `include` nor any sealed item, including
envelopes the same session stored while the option was `encrypted`. It is not a
claim that the request bytes match earlier releases: the ordering fix below
applies to every Responses provider regardless of the option, so a turn whose text
preceded a tool call now sends the text item first. Each replayed tool call also
carries the provider's own `arguments` bytes rather than a re-serialization,
because the endpoint fingerprints the string it sent, and a sealed item whose step
produced no following output is withheld rather than sent alone, counted as
withheld rather than as a replay.

A sealed envelope is durable state, so a step is persisted as a ledger of
positioned parts instead of one text blob plus a trailing pile of tool calls. Each
part id carries its position in the stream, `prt_{turn}_{step}_{position}_{kind}`,
and every part of a step shares the assistant message's creation time, so
hydration returns exactly the order the provider produced. A step that reasons,
writes text, calls a tool, reasons again, and calls a second tool replays as that
same sequence, each envelope immediately before the output it explains. That is
what a sealing endpoint validates: a reordered or summary-only replay is refused
on the wire.

An automatic Goal continuation is also a new provider turn, even when no new user
message exists. Zuno persists the prompt-receipt reference on the first assistant
row of that turn, not another copy of the prompt. If two assistant responses would
otherwise be adjacent in Responses `input`, the engine attaches the receipt's
actual post-hook developer items as a structured `ResponsesInputBoundary` sidecar
on the later request message. The shared Responses cursor used by OpenAI,
OpenAI-compatible, and Bedrock Mantle/Runtime emits those standard input items
between the assistant outputs before replaying the later reasoning envelope.
Generic message content, Chat Completions, Anthropic Messages, and Bedrock
Converse never receive that sidecar; an empty boundary serializes identically to
the ordinary message. Rows written by older releases recover the same receipt
through the durable
`assistantMessageID -> promptReceiptID -> actualProviderProjection.developer`
chain, falling back to `providerProjection` only when no hook changed the prompt.
Runtime-policy sections are removed from that receipt prefix, so only the original
turn context, memory, and request-hook context become the historical boundary. No
user message or fake tool result is manufactured.

Compaction summaries and imported old exports may legitimately have no receipt.
For those histories Zuno withholds only the sealed capsules in the ambiguous
assistant-output group while preserving text, tool calls, and real tool results.
The session remains usable at reduced reasoning continuity. Every Responses
request builder retains a final local validator, so a malformed group that
escapes the shared repair fails with message indexes and never renders opaque
token bytes.

Replay is scoped, and the scope is enforced while the request is assembled, not
when the row is written. An envelope is replayed only to the catalog provider and
model recorded on the assistant message that produced it, and only while it is
newer than `reasoningReplayMaxAge`. Anything else is withheld in memory for that
one request while the durable row keeps its ciphertext, so returning to the
original model resumes replay. Title, summary, compaction, learning extraction,
and Council requests run on other models and receive no envelope at all, which
also keeps the compaction transcript free of provider state.

Each foreground `session.provider.request` event records `reasoningReplay`,
`replayedReasoningCapsules`, `withheldReasoningCapsules`, and
`restoredReasoningReplayBoundaries`, plus
`withheldAmbiguousReasoningCapsules`. Those fields
are the evidence that replay is working: a session whose second and later requests
report zero replayed capsules is not replaying, whatever the endpoint claims. The
replayed count is what the adapter puts on the wire, so an envelope the pairing
rule drops is reported under the withheld count and never as a replay. The
restored-boundary count names older assistant rows whose exact developer suffix
was rebuilt from prompt receipts for that request. The ambiguous count is the
subset withheld because no exact boundary survived. The request event records
counts only, never the envelope or developer text.

The envelope itself is opaque provider ciphertext held as session content: it
lives in the reasoning part's `metadata.providerReasoning`, is returned by the
HTTP messages endpoint, and is forwarded on a stream event that carries it in
full: `provider.reasoning.item` on the server's SSE stream, and
`provider_reasoning_item` in `zuno run --json`, with the ciphertext under
`encryptedContent` on both. It is durable state a later
request needs, so it is not redacted; read access to a session's messages or
event stream is read access to its envelopes.

### Provider timeout and retry boundaries

An active provider request and recovery after a failed request are separate
lifecycles under one absolute recovery deadline. The deadline is anchored when
the initial request starts. That initial request remains governed by its
transport and stream-idle policies, but rollback emission, jittered backoff, and
every later replay must complete before the deadline. Expiry cancels an active
replay and persists a typed attempt failure. User cancellation continues to
interrupt either operation through the turn's control signal.

OpenAI-compatible error frames retain structured stream and protocol codes.
`upstream_stream_error`, `upstream_stream_incomplete`,
`upstream_stream_idle_timeout`, `malformed_upstream_tool_arguments`, and
`request_deadline_exceeded` are typed replacement-safe stream failures. They may
discard partial text, reasoning, and unfinished tool calls before replaying the
unchanged request. `malformed_upstream_tool_arguments` is reserved for a
completed model tool-call argument payload that is not valid JSON, so no
complete tool call from that attempt can be dispatched. Structural or
ambiguous tool-call violations remain `invalid_upstream_tool_call`. Protocol
codes such as `upstream_protocol_error`, `invalid_upstream_reasoning`, and
`invalid_upstream_tool_call` are terminal. The legacy generic `upstream_error`
remains terminal because it mixed both recovery classes. An opaque transient
error after partial output is also terminal for that request; only the
structured stream variant authorizes replacement.

Every provider call in the bounded recovery sequence has a durable
`session.provider.attempt.1` lifecycle. Its started and terminal events share
`attemptID` and `requestID` and record the attempt number, maximum attempts,
terminal status, whether partial output existed, the typed provider code when
present, and whether that code permits partial-output replacement. The enclosing
`session.provider.request.1` remains the logical step lifecycle. Replays clone
the original `CompletionRequest`, including its private durable-session affinity,
and the engine never adds the failed partial assistant output to history.

Tool execution begins only after the successful assistant checkpoint, so a
failed streamed tool call cannot dispatch a side effect. ACP is append-only and
cannot retract an already published text chunk. Its live projector therefore
holds provider text, reasoning, and pending tool updates until
`AssistantCheckpointed`; `RetryRollback` clears the provisional attempt and only
the replacement attempt is published. A completed structured question is
different: the accepted answer and tool result are already durable before the
continuation provider request begins. ACP reasserts that same tool call as
`in_progress` with typed `continuationPending` metadata, keeps it across provider
rollback, and emits the real `completed` update before the next successful
checkpoint. Failure, interruption, or event-stream closure settles the durable
tool result without publishing provisional assistant content. Other clients may
continue consuming the engine's lossless live events directly.

OpenAI-compatible transports resolve three independent provider options:

- `timeout`: a whole-request deadline in milliseconds, or `false` for none;
- `headerTimeout`: the maximum wait for HTTP response headers in milliseconds,
  or `false` for none;
- `chunkTimeout`: the maximum silent gap between streamed body chunks in
  milliseconds.

The whole-request deadline spans headers and body. Header timeout ends after the
response arrives; chunk timeout restarts after every received chunk. When more
than one deadline applies, the earliest one wins and produces a typed transient
provider error naming the phase. OpenAI-compatible providers default to a
330-second response-header timeout and a 120-second streamed-chunk idle timeout;
`headerTimeout: false` explicitly disables the former. Provider-specific
gateways should set their own upstream deadline below Zuno's matching phase
deadline so their typed error reaches Zuno before the client cancels the
connection.

The provider entry's typed `retry` block is not an SDK option. Its
`max_attempts` includes the initial request, while `recovery_window_ms` begins
only after that request returns a retryable failure. Rollback, backoff, and
every replacement attempt must finish inside that window. Defaults are three
attempts, a 180-second window, 2-second initial delay, 30-second maximum delay,
and 20 percent jitter. The policy is frozen with the resolved provider and is
never sent upstream.

The four native providers — OpenAI, Anthropic, Google, and Bedrock — do not read
those keys. Each applies one fixed 330-second response-header deadline and no
whole-request deadline, because a legitimate long turn has no upper bound the
provider can know in advance. Their streamed-chunk phase stays with the shared
300-second stream idle allowance, which `ZUNO_STREAM_IDLE_TIMEOUT_SECS` raises or
lowers for every provider. A native request that stalls before its first response
header now fails typed at the ceiling instead of waiting for the user to
interrupt it.

A stream that ends without a terminator is an incomplete upstream stream, not a
finished answer. Every native decoder reports `ProviderError::Stream` carrying
`upstream_stream_incomplete` when the transport reaches end of input while a
message is still open, so the failure is retryable and permits partial-output
replacement: the engine emits `RetryRollback`, discards what the truncated stream
produced, and replays the unchanged request. One terminator is sufficient, so a
Chat Completions stream that sends only `finish_reason`, or only `[DONE]`,
completes normally.

## Resident memory and user learning

The preview separates `MemoryPersistence` from `MemoryAuthority`.
`MemoryService` receives one coherent persistence provider covering candidates,
document revisions, evidence, retraction and maintenance settlement. The default
`SqliteMemoryPersistence` constructs all three underlying stores from one pool;
batch changes, source evidence, Job settlement and the maintenance watermark
retain their original atomic boundary.

An injected provider requires an explicit authority. `MemoryAccess` distinguishes
read, proposal, apply, edit, rejection, undo, import, maintenance and forgetting.
The trusted personal profile uses `LocalMemoryAuthority`. This is neither Entra
authentication nor an enterprise approval service. Organization policy must also
be rechecked by the committing backend; a preflight policy decision cannot replace
transactional session-generation, source-validity or learning-lease checks.

Candidate lookup and edit enforce the same document-path ownership as apply and
undo. An out-of-scope candidate returns a denied result without disclosing its
path. Model Memory tools use the immutable call origin for session and message
provenance; changing a tool context's public fields cannot change that origin.
Authority denial is a permission failure, not a model-correctable proposal.

These synchronous persistence transactions belong to the Memory data owner.
Remote HTTP/state-service consumers need bounded execution around that owner;
they must not block an Agent Worker's async reactor on a remote backend call.
Local file projection remains explicit in this implementation. Managed document
namespaces, enterprise authorization and the PostgreSQL provider are subsequent
integration work, not capabilities implied by an injectable trait.

Resident Memory has one model-visible mutation boundary: `memory_update`. It
validates add/replace/remove operations and inserts a durable `MemoryCandidate`;
it never edits the resident file directly. Candidates retain scope, action,
reason, confidence, source session/message, timestamps, diagnostics, and exact
before/after snapshots. `memory_read` exposes bounded current entries and revisions.
`memory_update` uses `ToolEffect::ManagedMemory`: generic strict side-effect
approval is unnecessary for this native data-only capability, while explicit
deny/ask and session generation policy still apply. It cannot modify permissions,
arbitrary files, MCP configuration or Skills.

Each durable session also owns a revisioned `session_memory_policy`.
`use_memories=false` removes resident Memory and retrieved Experience sections
from later prompt assemblies without deleting either store.
`generation=disabled` stops new learning; `generation=excluded` is the
fail-closed external-context state. The TUI exposes these controls through
`/memories`, while `/memory` remains the reviewed mutation surface. Policy
changes and their audit events commit together.

Child/job creation inherits the latest parent memory policy in that same transaction.
`SessionMemoryPolicyDefaults` contains fallback values only, never a durable revision;
an existing parent at revision 1 or greater is valid and always outranks those defaults.
The child seeds its own revision 1 and subsequent parent changes cannot rewrite it.
This corrects the scheduling failure caused by passing a durable projection as a
revision-zero default, without changing the database format or resetting user revisions.

Goal Markdown projections and promoted Resident Memory files share
`zuno-atomic-file` for visibility-atomic replacement. The provider writes a
completed sibling and uses `rename` on Unix or `ReplaceFileW` over an existing
Windows destination. A successful open sees only a complete old or new version.
Because Windows can reject a fresh open with `ERROR_FILE_NOT_FOUND` or
`ERROR_SHARING_VIOLATION` while `ReplaceFileW` holds its handles, the same
component gives consumers a bounded retry for exactly those two errors. It does
not hide permissions or other durable failures. This boundary is separate from
crash durability; authoritative session state remains in SQLite, and each caller
owns any stronger sync policy.

The default Memory promotion policy is `automatic`; explicit `review` remains
available. `high_confidence` applies candidates at or above the configured threshold.
New applications and undo commit resident entries, revision
history, exact candidate snapshots and terminal state in one SQLite transaction:

```text
pending -> applied -> undone
       \-> rejected
       \-> failed / uncertain
```

The resident Markdown files are versioned projections. Cooperating writers hold
an OS-backed path lock around comparison and replacement. Projection failure
does not discard accepted entries; a missing projection can be repaired from the
committed revision, while external divergent bytes are preserved. Every
foreground turn captures current resident versions before prompt assembly.
Historical `applying` and `undoing` rows still reconcile from stored snapshots;
any third state becomes `uncertain` and never authorizes replay.

User learning is a separate native subsystem and is enabled by default.
`learning.use` and `learning.generate` are independent below the
`learning.enabled` ceiling, so existing Experience can remain available when
generation is explicitly disabled.
A completed turn with tools,
artifacts, recovery, correction, or explicit feedback admits an idempotent
`learning_job` keyed by `(session, message, extractor_version)`. The dedicated
`learning.extractor_model` is authoritative when present. Without one, the
runtime selects a reachable `small_model` from the active provider and then the
active session model; it does not open another provider. The selected model
receives a bounded, redacted source manifest and a structured response schema with
no tools, network, filesystem authority, or foreground-session identity.
Request and terminal outcome are persisted as `learning.extraction.request` and
`learning.extraction.outcome`.

Extraction settlement atomically stores `ExperienceRecord` rows and evidence.
Unresolved issues remain searchable but cannot become Memory, patterns, or Skill
evaluation cases. Only project-scoped Memory proposals at confidence `>= 0.9`,
with validated citations and authoritative execution evidence, may auto-apply through the learning path; every other proposal remains
reviewable.

A process-owned project service semantically mines project and cross-project patterns through
durable interval-bucketed jobs. Automatic Skill candidates require three
independent sessions. Global patterns require two projects and become a
project-specific companion only after explicit promotion. Rejected evidence is
suppressed until its digest changes. Identical evidence preserves promoted status.
`LearningSupervisor` outlives individual foreground hosts, limits process-wide
concurrency, and propagates cancellation with bounded shutdown. Startup catches
missed admission; each claim has a fresh token, and periodic heartbeats recheck
policy. Token checks fence Memory/pattern commits.

Raw extraction only stores Experience and hints. `MemoryMaintainer` schedules
separate `project_aggregation` work with `purpose: memory`, without the Skill
aggregation evidence-count/interval gate. `MemoryConsolidator` has no tools and
returns at most 32 typed operations over bounded current memory and verified
evidence. `MemoryMaintenanceStore` atomically checks both scope revisions, lease
ownership, source content and generation policy, then commits candidate rows,
resident versions, provenance, job completion and a no-op watermark.
There is at most one semantic repair; unchanged successful inputs are not rerun.

`resident_memory_provenance` keeps independent support separate from direct
user-owned memory. Invalid sources cause immediate recall suppression; explicit
source forgetting and managed retractions are atomic. Retraction cannot restore
superseded old text or erase a user's reaffirmed note. Explicit forget/undo
decisions fence future automatic rewrites. Recall is fallible data, not an
authorization or an enforced instruction. Format 12 adds this state through the
guarded migration of formats 5–11; no database rebuild is required.

Automatic extraction is bounded background work. `learning.post_turn.idle_delay_ms`
defaults to zero: completing an eligible logical turn saves a bounded, closed
source snapshot, enqueues an idempotent job and wakes its worker. A new foreground
turn does not invalidate or block the previous completed-turn snapshot. Explicit
positive delays retain the idle/activity gate. The 60-second recovery poll,
two-job wake cap, project concurrency bound and session-policy checks remain;
newly completed work takes precedence over historical repair.
Web and MCP results carry a durable external-context marker; when
configured, consuming one excludes the session from automatic generation.
Bounded source values are scrubbed at secret-value granularity before clipping and
before they enter a learning job or extraction event, while the original durable Message and
non-secret evidence are preserved. Retryable extractor failures return the same
durable job to a positive, capped and jittered exponential-backoff deadline. Typed provider rate
limits preserve `Retry-After`; authentication, context-limit, protocol, and
other permanent failures settle instead of replaying the unchanged request.
The extractor reuses the selected model's resolved parameters and capabilities,
including sampling support and output limits. Diagnostic records retain bounded,
redacted provider response details, HTTP status and request identity. A historical
400 is a diagnostic task, not authorization to retry unchanged input or select
another model blindly.
There is no quota-percentage, daily-token, or currency budget; eligibility,
idempotency, the wake cap, the three-attempt ceiling, and `learning.execution`
input/output/step and total-time limits bound background work.

`/learn reprocess <assistant-message-id>` explicitly schedules the current
extractor version for an exact bounded source. `/learn repair-history [--dry-run]`
checks old evidence against its original source before planning reprocessing.
Missing, policy-excluded or forgotten source data remains unverified. Neither
format migration nor repair may mass-promote `verified` flags. Memory updates and
corrections use the managed mutation service; the next provider request refreshes
its resident-memory sections and preserves the old prompt receipt unchanged.

Retrieved experience enters the stable `learning.experiences` prompt section.
The post-hook prompt receipt stores each source identity, content, and digest, so
the provider request is reconstructable without consulting current projections.
`experience_search` provides explicit deeper FTS retrieval.

Every Skill candidate contains complete content, diff, evidence, exact source
identity, and source digest. Explicit human review runs an immutable offline
cassette suite: baseline and candidate execute actual bounded model attempts,
recorded tool calls are dispatched by exact arguments, and a separate grader
scores the real answer/trace. Starting a session only binds this evaluator.
Evaluation deadlines, ownership tokens and Drop guards protect cancellation and
concurrent-session recovery. Passing
evaluation does not apply the file. Apply is a separate CAS-protected effect;
source drift becomes `stale`, and restart reconciliation classifies before/after
snapshots without replaying an uncertain write.

`/memory` remains the resident Memory review surface. `/memories` controls
session use and generation. `/learn` and `/reflect`
manage experience, feedback, patterns, Skill candidates, evaluation, and
automatic Memory maintenance and separately reviewed Skill revocation. See the user-facing [Memory and learning](guide/memory-learning.md)
guide, [resident Memory](design/memory-learning.md), and the
[user learning flywheel](design/user-learning-flywheel.md).

## Durable inputs

Every model-visible external input is admitted to the session event log and durable inbox in one SQLite transaction before execution is attempted. The inbox is the source of truth across active turns, idle sessions, process restarts, and competing drivers.

Format 14 adds a native `InputAdmissionReceipt` alongside each input. `admitted`,
`recorded`, `applied`, and terminal `completed`/`failed`/`cancelled` are separate
facts: recording a background report is not provider application. The actual
post-hook request establishes application, and the owning logical execution
settles completion. Same-cycle recovery transfers receipt ownership explicitly;
an unrelated turn cannot complete a receipt. Optional client message IDs are
session-scoped and payload-validated, never derived by deduplicating text.

Standard ACP prompt requests observe that receipt until the associated outcome,
while `session/steer` returns immediate admission. Disconnect drops an observer;
it is not implicit withdrawal or evidence that an old owner has stopped.

Every multi-statement write transaction reserves SQLite's writer with `BEGIN IMMEDIATE`,
including transactions opened through a caller-owned turn connection. This lets the configured
busy timeout serialize concurrent parent and child writers before any read snapshot is taken;
Zuno does not use a deferred read-then-write upgrade that can fail with
`SQLITE_BUSY_SNAPSHOT` without invoking SQLite's busy handler.

An interactive `SessionChoice::New` is prepared without inserting a `session` row. The
process-local identity is stable across model, agent, MCP, and theme changes, but opening,
browsing, or leaving the welcome screen creates no durable session. The first model-bound
submission inserts the session and its user message in one transaction, then emits
`session.materialized` for clients. Existing and continued sessions still hydrate immediately.
The TUI `/new` command selects another prepared `SessionChoice::New` in the same
terminal activation. It opens an empty conversation shell directly instead of
returning to the launch welcome surface, and it does not bypass this lazy
materialization boundary.

Drivers promote inputs in FIFO order. Promotion is transactional and can target one input identifier for a live soft interrupt, or every pending settled report at once for a batched idle wake. A row a driver owns but cannot decode records a session error and does not strand later queue entries.

Admission never competes with the live-turn lease. One shared admission service commits the `session_input` row first and resolves how it reaches the model second, so every surface — TUI, ACP, HTTP, and the `run` host — reports one of three outcomes over an input that is already durable: the caller received the exclusive turn lease and drives the row itself; a running turn accepted the row as a soft interrupt and promotes it at its next safe point; or the row stays pending for the next FIFO promotion. Contending for the lease first and returning early when it is held is what loses a prompt with no durable trace, so a busy session is an outcome of admission rather than a failure of it. A caller whose own driver loop already owns every turn for the session never asks for the lease at all and is answered with the steered or pending outcome.

Each surface decodes only the payload shapes it can drive, and every published shape has exactly one decoder. A pending row a driver cannot render — a queued terminal submission met by the HTTP prompt driver, an HTTP body carrying its own agent and model overrides met by the terminal driver — is stepped over in FIFO order and left pending for the surface that owns it, instead of being promoted and then settled as `failed`. Settled asynchronous reports and answered human requests carry plain text, so every prompt driver runs them. A payload no writer publishes is left pending for the same reason a foreign one is: a driver cannot tell an unrecognized shape apart from a shape it simply does not own, so the row is preserved and stays visible in the queue rather than destroyed. Mixing surfaces on one session therefore cannot let one row a driver does not own break that driver.

User prompts and subagent reports share this protocol:

- An active parent receives a soft interrupt and promotes the report at the next tool-safe point.
- If the report misses the final safe point, the wake coordinator waits for the active lease to end and starts another turn while the input is still pending.
- An idle parent is claimed and driven immediately.
- A restarted process recovers pending reports from the durable inbox.

Settled reports are delivered as a batch, not one row at a time. A single wake
claims every report the parent has pending in one transaction and drives them in
one turn. Each report still becomes its own durable user message with its own
`session.input.promoted` and `session.input.consumed` events and its own
`message.data.taskReport`; only the provider request is shared. A wake that finds
the session busy offers the whole pending report batch to the running turn's next
safe point for the same reason, and a report that turn never reaches stays
pending for the next scan. Driving one turn per row instead made a fan-out that
settled together arrive as a stream of turns, each announcing a state a later
report in the same batch had already replaced. The HTTP prompt driver claims the
same batch in the same transaction, so a session holding three settled reports
produces one assistant turn on that surface too, while a typed prompt still runs as
its own request with its own agent and model overrides.

Admission never waits behind a delivery. A background command's completion is
admitted as a durable inbox row before any turn is requested, and the watcher
admits every settlement already waiting behind it in the same pass, so a group of
background commands that finished together is one batch and a restarted process
that finds several terminal commands delivers them as one batch too. A command
that settles while a delivery turn is already running is admitted immediately and
joins the next batch, because its report cannot exist before it settles.

Reports are grouped by the work they describe when a batch is rendered. Where a
batch carries several reports for one job or background execution, only the report
whose work completed last is presented as that work's current state; the earlier
ones are marked superseded in the text the model reads. The projection belongs to
the engine rather than to one client, so the batch a wake drives as a new turn and
the batch a wake offers to a running turn carry identical text: a replaced state
reads as superseded whether the parent was idle or busy when the newer report
arrived, and every present and future client surface reaches the same decision from
the same durable rows. Plan reconciliation is seeded from the newest report of a
batch the wake drives as its own turn; a batch admitted into a turn that is already
running enters as durable user input and that turn keeps the planning source it
started with. Grouping is a projection over durable rows: nothing is merged,
reordered, dropped, or given a new inbox state, and a delivery carrying one report
per unit of work is presented exactly as its writers wrote it. A promoted report
whose durable prompt carries no model-visible text is settled `failed` with that
reason rather than stalling the reports behind it.

The same boundary now covers every asynchronous continuation source:
`subagentReport`, `productAgentReport`, `workflowReport`, `councilReport`, and
`backgroundExecutionReport`. The provider-specific completion signal is never
the queue of record. A producer first commits a typed `session_input`; only then
may a process-local watcher nudge the session. A wake is successful only after
the input is promoted by an active turn or claimed by a newly started turn.
Persisting the row alone is not an acknowledgement.

This follows the public
[Codex app-server thread model](https://developers.openai.com/codex/app-server)
without copying an undocumented implementation: Codex keeps threads resumable,
exposes runtime status, and allows later turns after the prior turn returned.
Zuno makes the corresponding local invariant explicit in SQLite. If the process
remains resident, a completion starts the continuation immediately. If the
process exits, no model can run in the absence of a resident runtime; the
notification remains durable and is redriven when that session is activated.
An explicit session close first unregisters the resident watcher, interrupts and
joins any detached continuation, and then applies the surface lifecycle policy
instead of silently reopening the session.

Detached turns use the same engine events as request-owned turns. TUI routes root
events back to the mounted transcript, ACP sends ordinary root
`session/update` notifications, and the HTTP server commits and fans out the
same durable event projection. Child-session events retain their child observer.
After the detached event stream drains, ACP flushes the root session's
authoritative durable Plan through the same revision-aware projector used for
live changes and restoration. This terminal projection is best-effort and
cannot change the already committed turn outcome. No client owns a private
continuation loop.

Interactive TUI input uses the same durable boundary. When idle, `Enter`
starts a turn. During an active turn, `Enter` admits a FIFO `queue` item for the
next turn; leader+Return (also `Ctrl+Enter`) is the explicit `steer` override and requests a soft
interrupt at the nearest safe step boundary. `Shift+Enter`, `Alt+Enter`, and
`Ctrl+J` insert a newline. The UI reports an item as queued only after SQLite
commits it, and pending items can be edited or cancelled by revision and survive
a process restart. Submission transport is a typed envelope with independent
`payload`, `delivery`, and `origin` fields. The command palette's immediate-send
action and explicit Send Now in a running child session also produce `steer`; ordinary
busy input produces `queue`. Only text and typed rich-content payloads may steer.
Commands, Skills, Council requests, and host commands are queued even if their UI
gesture requested immediate delivery. The HTTP prompt API follows the same rule:
omitting `delivery` means `queue`, while `steer` must be explicit.

Queue Send Now compares the selected input revision and displayed turn id while holding
the inbox transaction, then queues a non-aborting live signal before commit. Rejection
rolls back both row and event; an idle send reserves the run guard before promoting only
that row. Other rows retain their original admission order. Consumption checks the
signal's revision and emits `InputConsumed` after durable user-message/consumed-state
commit. Completion closes input admission atomically and continues the same engine turn
if input won that race. A soft checkpoint is not an interrupted turn.
The HTTP display projection uses `turn.input.consumed`; the authoritative inbox
transition remains `session.input.consumed` and is not written a second time.

The TUI's `DraftRecovery` owns complete composer snapshots until admission receipts.
It restores refused drafts without overwriting newer text or automatically retargeting
a new turn. Queued text edits preserve image references; admitted steering snapshots
can be cancelled before consumption but are not rewritten by the TUI.

Composer arrows use whole-buffer endpoint history semantics; transcript scrolling has
its own focus. Paste aggregation precedes keybinding dispatch and never treats block
newlines as submits. Clipboard requests have asynchronous one-shot completion, and
selection/copying share rendered grapheme coordinates. Native Windows prefers `pwsh.exe`
then `powershell.exe`; unacknowledged OSC52 emission is not reported as confirmed copy.

Windows/macOS default to native only without explicit backend/fallback/network/path
constraints; Linux keeps automatic confinement discovery. `platform_native` is distinct
from `trusted_native` and `unavailable_fallback`. Execution authority writer version 4
retains readers for versions 2 and 3 and rejects future/invalid versions. `executionReady`
is independent of confinement `ready`. Agent/Plan/Work transitions preflight the target
before durable mode changes and guard a Start Work execution revision.

User input is typed rich content, not only a rendered string. Every new local or
client-supplied image is admitted before the durable inbox write through the
profile's `AttachmentStore`. Admission applies source, dimension, pixel, and
encoded-byte policy; orientation and metadata normalization; and atomic private
content-addressed publication under the current database identity. The durable
file part stores only an `ImageAttachmentRef`, never new base64 data.

Provider request assembly resolves and verifies the object late, optionally
caches a route-policy-derived encoding, and then reconstructs the existing
provider-neutral inline image block. A missing object or digest/reference
mismatch is a permanent durable-state failure and never falls back to the source
path. Historical inline `media_type`/`data` parts remain readable without an
automatic rewrite. Root sessions, attached children, direct sends, queued
inputs, steering, TUI, `zuno run --file`, ACP, and server ingress use this same
path. The visible `[Image #N]` token remains draft presentation state. See
[images and file references](reference/attachments.md).

A provider stream or provider-retry delay is wakeable for explicit steering:
Zuno checkpoints any partial assistant output with `finish: steer`, promotes the
durable input, and starts the next model step without emitting
`TurnInterrupted`. An executing tool is not abandoned merely to steer; its
result reaches the next tool-safe point first. Commands and ordinary active-turn
submissions target the FIFO queue. If the turn ends before a steer is consumed,
the already admitted item remains pending and is promoted in FIFO order on the
next turn; it is never lost or duplicated. The bounded in-process prompt channel
is only a wakeup and handoff path, not the queue of record. A steer never fires
the hard-interrupt signal and therefore cannot cancel a foreground `task`.

Tool-owned human input is projected separately from execution. A permission
prompt reports `awaiting approval`; a structured question reports `awaiting
answer`. Both surfaces replace the composer region rather than becoming another
transcript card. Permission choices support Left/Right, the existing Up/Down
aliases, Enter, and mouse selection; explicit expansion moves the prompt to the
larger overlay. Questions show `Question i/n`, the remaining unanswered count,
numbered choices, and a numbered `Other` input. They support Up/Down and `j/k`
within a question, Left/Right and `h/l` across questions, number-key selection,
Enter, Space for multi-select, and mouse selection. Per-question cursors and
custom drafts survive navigation. Merely highlighting a choice does not select
it. Ctrl+S defers a question, preserving partial answers; `/questions` reopens the
pending set. Deferral, cancellation and an empty submission never fabricate an answer.

`runtime.work_state` version 3 includes the session scheduling gate and bounded
pending-question summaries, including early approvals awaiting handoff. They are
restored after compaction/restart even when no Goal exists. These state summaries
do not manufacture answers or repeat answer content already delivered by inbox.

`QuestionPort` is shared by tools, TUI, HTTP and ACP. `QuestionService` commits
the request definition, revision, keyed partial answers, command receipt, FIFO
input and matching session/Goal transition together. `question_async` publishes
optional input without parking a tool; `question` may wait until answered or
deferred. Both return receipts, while response content is delivered only through
the inbox. `goal_request_input` uses the same provider and returns
`TurnOutcome::WaitingForHuman`. Requests outlive their originating turn and
client connection. Permission requests remain a distinct kind; closing a
question never supplies a permission approval.

Interaction registration depends on an actual QuestionPort consumer, not a
client-name or experimental Plan flag. Plan supports synchronous clarification
and deferred `plan_exit`; ordinary Work can register required human input even
without a Goal. Root turns support optional `question_async`; delegated children
report blockers to their parent. A headless host without the consumer advertises
none of these tools. `question_async` shares the `question` permission key.

`plan_exit` freezes the exact Plan/review revision and saved Work identity in a
host-created request. Only typed approve/decline actions are accepted, never
generic answer arrays. Early approval waits for successful source-turn handoff;
the handoff and queued Start Work control are transactional. Interruption or
changed Plan, review or identity invalidates stale consent. Draft risk acceptance
must come from the user. The tool itself cannot switch collaboration mode.

The conversation surface separates reply identity from transient work state. The
identity row contains the resolved agent, catalog model display name, and configured
reasoning effort. It follows the bottom of a short assistant reply; once transcript
content fills the available viewport, the same row becomes sticky immediately above
the composer. The final row also repeats the current agent/model/effort as a neutral,
prompt-adjacent badge so the next-turn selection remains visible while a turn is running;
Tab updates that badge immediately while host replacement remains deferred to the turn
boundary. It does not invent cost or speed multipliers when no authoritative runtime
metadata exists.

The frame's final row is the live control surface. During a turn it shows one
animation-clock-driven pulse, the resolved interrupt key, latest provider-prompt
occupancy, command-list key, and current neutral agent/model badge. The first interrupt press changes that same row
to its confirmation state, so the transcript and composer do not reflow. Permission
and question waits replace the pulse with their explicit reason. When idle, the row
returns to directory, context, and command discovery. Transient `working` rows are not
inserted into the transcript; durable activity, errors, interruption markers, and
assistant content remain reconstructable from session events.

Durably admitted user follow-ups are shown in FIFO order in a fixed dock directly
above the composer while a turn is active. Ordinary submit labels an entry for
the next turn; the dock resolves and shows the user's actual
`input_force_submit` binding for steering the active turn, plus the queue-manager
binding. Promotion removes the entry from the dock and adds it to transcript
history; cancellation removes it without fabricating a sent message.

Context occupancy is produced once by the native `ContextUsageSnapshot` tracker:
the most recent applicable provider confirmation plus an estimate of normalized
content not yet included in that request. A new request's lower whole-prompt
estimate must not replace this baseline. For example, a `73,948` estimate cannot
overwrite a provider-confirmed `149,501` input count. Confirmation includes its
accounting mode and request identity; partial frames merge instead of zero-filling
missing fields or adding repeated snapshots.

Cumulative disjoint usage, the last confirmation and the estimated tail remain
separate. Cache and reasoning subsets are not double-counted. Snapshots include
source, request/attempt identity, epoch, revision, freshness and update time;
unknown is not zero. Main, child, learning, compaction and auxiliary work cannot
overwrite each other's context window. ACP, TUI, HTTP and recovery consume this
same state. Estimates cover actual normalized messages, developer context and
tool schemas, not unread file sizes or duplicated raw tool arguments.

Compaction advances the durable `context_epoch`, recomputes the retained prompt
window, and restarts the same execution cycle as `TurnStartKind::Recovery`.
Recovery uses the frozen continuation identity and optional anchor instead of
requiring a retained human message, so ACP receives the lower post-compaction
usage update rather than remaining at 100% or failing with `NoUserMessage`.

Terminal background commands, subagents, workflows, and product Agents publish a
`CompletionEnvelope` with a deterministic `source_key`. Synchronous `bg wait`
and asynchronous callback delivery compete for one durable `inline` or
`callback` owner. The callback path wakes the parent automatically; a model-visible
wait is capped at 60 seconds and is appropriate only when the current step
synchronously depends on the result.

A transcript revert (`revert_commit`) discards the projected `session_message` rows and the legacy `message` rows after the staged message's `(time_created, id)` boundary, clears the session's context epoch, and retires every `queued`, `steering`, or `promoted` inbox input through the ordinary cancellation transition, so each retired input logs its own `session.input.cancelled`; consumed inputs are immutable history and are never touched. Inbox rows are never deleted by a revert. The commit then appends one `session.reverted` event whose properties are: `sessionID` (string), `messageID` (string, the boundary message that remains the transcript tail), `marker` (object, the staged revert JSON exactly as stored, e.g. `{"messageID": "...", "files": []}`), `boundaryTimeCreated` (i64 ms), `removedMessageCount` (u64, projected rows deleted), `removedLegacyMessageCount` (u64, legacy rows deleted), `cancelledInputIDs` (string[] in admission order), `contextEpochCleared` (bool), `timeUpdated` (i64 ms). Every key is always present.

## Plan and Work transitions

`/plan` and `/start-plan` enter Plan mode idempotently. `/plan` may use the TUI
confirmation surface; neither command doubles as an exit. `/start-work` is the
only Plan-to-Work authorization. It validates the exact current Plan revision and
its handoff-ready record, and the confirmation names its title, revision, and
completed-step count.

For an active Goal, entering Plan and recording `paused(plan_mode)` are one
transactional host transition. Re-entering Plan is idempotent. Agent selection
does not change collaboration mode: while planning it updates the saved Work
Agent used by a later handoff. Start Work atomically reads the Plan and bound
review gate, restores the saved Agent/provider/model/reasoning identity, records
the Work cycle, admits a `UserControl::StartWork` input, and resumes only the
eligible Goal pause. A bound Draft review blocks by default; an explicit
`--accept-draft-risk <reason>` persists the accepted review revision and reason.
The same checks run after restart, so Goal → Plan → restart → Work resumes at most once.

ACP publishes the same three names as native session commands.
`session/set_mode(build)` and `/start-work` call the same Start Work operation.
An idle session starts implementation immediately; a busy session persists the
control and runs it at the next safe point. The command itself never becomes a
user message, and the slash response identifies `planId`, `planRevision`,
`cycleId`, and `started` or `queued`.

Plan is enforced below the prompt by a deny-by-default capability overlay. It
allows repository inspection, read-only LSP and search, questions, Skills, and
typed Goal/Plan/Todo operations, while shell and file mutation remain denied.
The model can recommend Start Work but cannot select it for the user.
`session_execution_state` persists mode independently from Agent selection, so
compaction, process recovery, and in-process session switching restore the exact
boundary without inventing a user message.

## Native session commands, compaction, and hard interruption

The typed `SessionCommand` registry is shared by client surfaces and currently
contains `/compact`, `/goal`, `/plan`, `/start-plan`, `/start-work`, `/questions`,
and `/resume`.
Native discovery resolves before Markdown commands and Skills, so a same-named
user workflow cannot shadow a runtime control.

`/compact` and `/goal` invoke shared live-`TurnHost` handlers in both the TUI
and ACP. Compact runs the hidden compaction agent. Goal accepts either a direct
objective or show/history/create/edit/budget/pause/resume/block/complete/cancel
against the durable goal store. A direct objective creates a goal when none exists or
the previous goal is complete or cancelled; otherwise it updates the objective
while preserving lifecycle state, budget, and usage. Create, edit, and shorthand
objective changes also reconcile an active durable Plan: a multi-stage objective
archives the previous visible Plan and installs a new root bound to the current
`goal_id`; an atomic objective can terminalize stale unfinished work without
rebinding an already terminal historical Plan, and a terminal Plan that belongs to a
previous Goal is archived as completed history — at the objective change or, if it is still
visible, at the next host planning decision — so the completion audit never judges the
new Goal against it. Recognized action
words take precedence over the shorthand.
Neither surface sends the slash text to the model or synthesizes a private
client-only result. Goal output is a typed session-command output event, not
reasoning. A successful create or edit command also establishes the host-owned
idle edge that lets the shared Goal continuation driver run immediately. When
the session has no user message, the driver durably admits the objective itself
as the initial user turn anchor before the provider request. Invalid explicit
action arguments remain typed command failures; ACP maps them to JSON-RPC
invalid params rather than an internal session error.

The command acquires the session's exclusive run ownership and emits typed
started, output, completed, or failed lifecycle events. TUI and ACP consume
those events directly; the HTTP event service commits their stable
`session.command.{started,output,completed,failed}` projections before live
delivery.
The summary, retained tail, marker, prompt provenance, and usage are durable.
Proactive compaction uses the validated `compaction.threshold_percent` of the
usable model window and can be disabled with `compaction.auto: false`. A
provider-confirmed context-limit failure retains its bounded recovery
compaction, while a manual command is always eligible.

The prelude checks persisted history before the first provider request. A long
tool turn then checks the same proactive threshold before every subsequent
provider request, so one turn cannot keep growing until the provider's hard
limit. The check prefers the previous response's provider-reported context
usage; if the provider supplied no usable measurement, it uses the current
assembled prompt estimate. Reaching the threshold publishes an informational
notice with stable code `context.compact` and returns
`TurnError::CompactionRequired`. The host follows the existing typed
compact-and-retry recovery path. With `compaction.auto: false`, the host
attaches no proactive threshold and none of these between-request checks run.

Checkpoint creation and continuation use separate, task-neutral templates.
`compaction/summary.md` asks for the actual objective, constraints and decisions,
verified progress, active work, blockers, next steps, and references. It carries
forward still-relevant state from the previous accepted checkpoint and applies
newer explicit corrections. `compaction/continuation.md` becomes the native
`runtime.compaction` section whenever a provider request retains a checkpoint,
including manual compaction, a new user request, and restart. It treats summaries
as historical context and preserves the current task, mode, and permission
boundaries. This adapts Codex's checkpoint handoff and OpenCode's incremental
summary rules to Zuno's durable work state.

The host rebuilds Goal/Plan/Todo/Job context before the first request after
in-drive compaction. If the Plan changed during that driver invocation, a
one-time create-or-replace instruction becomes Plan maintenance. Otherwise it
remains unsatisfied. A compaction marker, including older markers without a
`mode` field, cannot replace the real user anchor, experience-retrieval query,
or completed-turn learning boundary. The engine handles continuation through native runtime context rather
than fabricating another human message.

Summary generation includes the resolved compaction agent's own system instruction.
The main agent's initial instructions and resident Memory stay outside the summary
input and are restored independently. Once a checkpoint is accepted, token usage
from the previous window cannot trigger another compaction; only subsequent main
responses describe the current window. Cache accounting and separately stored
reasoning tokens retain their original meaning.
The exact post-hook request, stable section sources, digest, and selected bounds
are committed as `session.compaction.prompt`, linked from the summary message.
This auxiliary receipt does not replace the foreground prompt receipt used to
restore selected Skills or execution provenance. Provider usage preserves split
frames and cache accounting; visible output excludes separately recorded
reasoning tokens.

The collector accepts only a nonempty, normally completed response. A provider
rollback discards that attempt's partial text and usage. EOF without a terminal
message, a length stop, an unexpected tool operation, timeout, or excessive
summary bytes cannot publish a successful checkpoint. The stream obeys
`compaction.timeout_seconds` and `compaction.max_summary_bytes`, and observes the
owning turn's interrupt signal. Cancellation remains resumable and does not
latch a permanent compaction failure.

Marker creation and successful summary publication are separate atomic writes;
no transaction spans a provider await. A newer failed or dangling attempt leaves
the previous successful boundary active. Request projection uses only the active
successful summary, while old summaries and failed attempts remain in storage.
The next compaction receives the previous accepted summary explicitly, so it
cannot accidentally omit earlier objectives merely because that summary was
inside the verbatim tail.

An automatic compaction consults the auto-continue hook only after the summary is
durable and the prompt cache has been reset. If that hook fails, the session stays
`Compacted`: the persisted summary stands, no continuation turn is synthesized because
a vote nobody cast grants none, and the failure travels with the compacted transcript as
`auto_continue_hook_failure` and is logged as a warning. The session is not marked
failed, so a later compaction is not refused as `AlreadyFailed`. Rewriting the durable
summary as a failure would discard work the model can already resume from.

Compaction changes the provider transcript boundary; it does not delete the
durable Goal, Plan, Todo/WorkItem, Job, inbox, event log, or prompt receipts.
The active Goal is regenerated from SQLite for every relevant provider request.
Selected Skill bodies remain in the mounted resolver and are restored from the
latest durable prompt receipt when a host is reopened. Pending subagent reports
remain durable inbox inputs and are recovered from the same row after restart.
Every relevant provider request also regenerates a bounded
`runtime.work_state` developer instruction from SQLite. It includes the current
Plan revision and steps, Todo/WorkItem identities and dependencies, active or
uncertain Jobs, terminal Jobs linked to a still-unfinished Plan step, terminal
Jobs with an unconsumed `nextStep` report, pending report identities and states,
and the latest prior prompt receipt id. Job entries expose their versioned
`workContext` when present. One deferred SQLite transaction reads all of those
tables from the same snapshot.
This includes the first provider request of an automatic Goal continuation; that
path uses the same projection as an ordinary user turn rather than relying on old
state-tool history.
Each collection is capped at 64 entries and the complete rendered section is
capped at 16 KiB. Verbose text is UTF-8-safely shortened before whole tail
entries are omitted; omitted counts remain explicit and authoritative identity
fields are retained. Assembly fails closed if even those identity fields cannot
fit. Typed tools remain the only mutation and full-detail query interface.

Historical image bytes are excluded from the compaction model request. The
summary input keeps a stable human label such as
`[Attached diagram.png (image/png)]`, while the original durable file part
remains unchanged for authoritative replay.

Tool-pair identities and real-user provenance are captured before historical
messages are converted to inert summary text. The boundary therefore does not
mistake a tool result for a human turn or split a call from its result; reused call
ids pair with their preceding call. Long tool arguments, results, and plaintext
reasoning keep bounded head-and-tail excerpts with explicit omissions. Signed
reasoning becomes labelled historical working notes, and encrypted reasoning
stays out of the summary request. These projections leave raw durable evidence
and the retained tail's normal provider protocol unchanged.

Optional current-session recovery is a native `zuno-continuity` component, not
a private client feature. Its interface, SQLite provider, and model-tool
consumers are separate services mounted through `ProfileBundle` and
`ToolContributions`, so CLI, TUI, ACP, server, and child turns consume the same
final registry. The component is disabled by default and contributes `history`
and `notes` independently.

History reads the durable message store through four actions:
`list_windows`, `list_items`, `read_item`, and `search_contents`. Only a
successful compaction creates a new window. Every opaque window, item, and page
cursor is bound to the current session. Normalization omits reasoning, encrypted
values, synthetic internal prompt text, tool input, and binary attachment bytes;
returned evidence is untrusted data rather than prompt authority.

Notes uses logical document names and the trusted `session_id + Agent` identity
from `ToolContext`. A scope is limited to 100 documents, 256 KiB per document,
and 1 MiB total. `append_to_file` and `write_file` require the exact
`expected_revision` (`0` only for creation). The trusted `call_id`, request
digest, and revision form an idempotency ledger, so duplicate delivery returns
the committed result while stale concurrent writes fail without mutation.
Notes reads are `Safe + ParallelSafe + ReadOnly`; Notes writes are
`Never + Exclusive + SideEffecting`. Missing or unknown actions take the strict
write policy before typed argument validation.

The component owns additive `session_note` and `session_note_operation` tables.
Session cascade deletion and prune include
both tables. Session export/import preserves notes and their operation ledger,
validates logical names and quotas on import, and sanitized export redacts note
identity/content while dropping the idempotency ledger.

A hard interruption is a typed `HardInterruptRequest` carrying both source and
reason. Sources distinguish TUI, ACP, HTTP API, and lifecycle teardown; reasons
distinguish user cancellation, request cancellation, exit, shutdown, and session
close. It is session-scoped and linearizable across turn handoff. If the previous
run guard has dropped but an already admitted follow-up has not yet acquired its
guard, the registry arms that next guard instead of discarding the interrupt.
The first accepted request remains authoritative, so later shutdown cannot
overwrite an earlier user action. The turn starts with its interrupt signal set,
emits the normal terminal interruption event, and issues no provider request.

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
an observed result and is never mechanically replayed. A running tool receives a
two-second cooperative cleanup window. Settling in that window produces a typed
`cooperative` cancellation and preserves the tool's terminal report; expiry
force-aborts the invocation and records `forced` plus `uncertain`, requiring
authoritative-state inspection before retry. A cooperative settlement is not
automatically certain. A tool that was stopped before its work reached a decided
outcome declares that on its settled result under the `cancellation` metadata
key; the dispatcher records `uncertain` for that call too and appends the
authoritative-state demand to the settled report the model reads, so the
requirement is model-visible and not only durable. A tool that declares nothing
keeps the certain cooperative reading, text included. `shell` carries both
readings, and what separates them is whether the service settled a status as the
command's own, not merely whether the process had exited. A run that completed
and reported its own verdict before the cancellation was serviced reports that
exit status with the receipt a completed run earns and is cancelled but not
uncertain. Every other cancelled run preserves its captured output, carries an
unresolved receipt with no exit authority, and is uncertain — including the runs
that do report a number: an `exit 125` that is the child-process guard's own
failure, a run killed at its hard ceiling, and a status the execution reported
but settled as something other than the command's own outcome. The reported code
stays in the result because it is what a terminal would have shown, so an
uncertain cancellation may still name an exit code; the receipt is what refuses
to certify it. Neither reading is ever mechanically replayed. The resolved
verdict travels on the `ToolDispatchInterrupted` runtime event and not only in
the durable record, so the SSE `tool.dispatch.interrupted` payload, the ACP
session update, and `zuno run` publish the same `uncertain` a replayed session
reconstructs from durable metadata, while `forced` keeps meaning only that the
grace window expired. Post-tool hooks may add diagnostics but cannot rewrite
either cancellation outcome as an ordinary failure.
`TurnInterrupted` adds a separate session-owned
`Conversation interrupted by user.` row to the live transcript. The source and
reason are persisted on `turn.interrupted` and, when an assistant checkpoint
exists, inside its typed abort error. TUI and ACP replay reconstruct cancellation
as session state rather than assistant prose or a normal task failure.

The same rule protects an ordinary settled tool call. `ToolHooks::after` is an output
post-processor, and the tool has already run by the time it fails, so whatever the tool
changed is real whether or not a plugin managed to post-process the output. The result
therefore keeps its own status and `is_error`; the hook failure travels with it as
`afterHookError` metadata and is logged as a warning. Rewriting a settled result into a
bare error would tell the model the effect never happened and invite it to repeat a side
effect.

Assistant checkpoints reconcile message usage and the session usage projection in the same
transaction. Repeated checkpoints subtract the previous message snapshot before adding the new
one. Provider accounting is persisted with each snapshot so cache tokens are counted exactly once;
stored assistant rows without a reliable accounting mode remain explicitly unavailable instead of
being reported as zero. The projection stores cumulative disjoint token buckets, the latest whole prompt,
the context limit, and the latest accounting mode.

Bedrock Converse and Anthropic Invoke use disjoint prompt buckets; OpenAI Responses
cache details remain inside its input count. Invoke's start and delta usage events
update one request snapshot, and an explicit thinking breakdown remains a subset of
output. A content-filtered empty response checkpoints its reported usage and becomes
a typed permanent provider refusal, never a retryable empty-answer failure.

The TUI keeps a request baseline and one replaceable usage snapshot. Partial usage
events preserve fields not re-reported; raw output is split into visible output and
reasoning before contributing to cumulative disjoint buckets. Request boundaries start
a fresh snapshot, retry rollback restores the baseline, and durable session restoration
clears the provisional accumulator.

## Durable goal recovery

An active goal uses two recovery layers. The provider request layer retries a bounded sequence in place and rolls back unpublished partial output before another request. Its recovery window starts only after the original request returns its first retryable failure. The original request remains governed by transport and stream-idle limits and does not consume that window, while rollback, locally jittered backoff, and every replacement attempt must finish before the resulting absolute deadline; expiry cancels an active replay and persists its attempt as a typed deadline failure. Before every wait Zuno commits a `provider_retry_backoff` checkpoint with the request id, turn id, failed and next attempt, typed reason, selected delay, and wait deadline. Its in-place backoff is interruptible by both hard cancellation and durable live steering; waking it does not replay the stale provider request. After a process restart, Zuno waits out any remaining checkpoint deadline and starts a new turn and provider request instead of attempting to revive the old transport. If the bounded sequence still ends in a recoverable error, the goal controller writes a `goal_retry` row before waiting and starts a fresh agent turn when its persisted deadline arrives. There is no cross-turn retry-count ceiling for recoverable failures: the delay grows exponentially, reaches the configured cap, and the goal remains active until it completes, is paused, reaches its token budget, or encounters a permanent failure.

Goal continuation is a first-class turn origin. A prepared continuation captures the exact
Goal id and revision, and a revision change invalidates it before provider work starts. The
turn's execution identity is captured independently from the current host: Agent, catalog
provider, and catalog model. Retained user history supplies only the causal transcript anchor
and grants no authority. It is never rewritten to make a reconfigured host look historical,
and its old Agent/model fields cannot route an automatic Goal turn. Ordinary user turns
continue to use their own message identity.

Interruption preserves a paused Goal. Like the inspected Codex Goal menu,
the host offers explicit Resume goal / Keep paused consent; skipping is inert.
Zuno persists that choice through `QuestionPort`, bound to `GoalResumeRequest`
(session, Goal ID, expected revision and optional existing input ID).
One transaction validates the binding, changes Goal/execution eligibility and
routes the existing input or a native control. Already processed text is not
submitted twice. Plan, approval, budget, authentication and uncertain-effect
barriers remain owned by their controls; a Goal choice does not clear them.
Background reports may be recorded while paused, but apply only on a legitimately
eligible request. The rendered Goal context names its actual status and pause
reason, rather than describing every existing Goal as active.

Goal completion reads Plan step statuses through the same shared type as the Plan
writer. `completed` and `superseded` are terminal; missing, unknown, or legacy
`cancelled` values fail closed as durable Plan corruption. A model update that settles
criteria and completes the Goal performs both in one transaction, so any audit refusal
rolls the checklist and Goal revision back together.

The engine appends one `session.turn.started.1` event after resolving the current identity and
before provider dispatch. It records `turnTrigger`, `anchorMessageID`, Agent, provider, and
model; Goal turns also record `goalID` and `goalRevision`. Provider-attempt events repeat the
Goal trigger and resolved identity so retry evidence remains self-contained. If Agent or model
resolution fails, `session.turn.rejected.1` records the requested identity and typed failure
before the Goal is blocked; no started or provider-attempt event is fabricated.

The retry row is tied to the exact `goal_id` and stores the attempt, typed reason, selected delay, schedule time, and next eligible time. Reopening the same session reconstructs the wait from SQLite. ACP load and resume first restore the durable session cold. If the root Goal is active, the stable session registry singleflights runtime activation and schedules the Goal through the detached continuation observer; otherwise no TurnHost, MCP, plugin host, or watcher starts until user-authorized work arrives. This recovery path is process-owned and uses the same per-session execution gate as a prompt, so it cannot race a second Goal turn. Queued user input has priority over an automatic turn, and long waits are split by `poll_interval_ms` so an interactive surface can notice that input promptly.

Local delays use exponential backoff with symmetric jitter and never collapse to zero. A valid provider `Retry-After` value is never shortened by jitter; it is clamped to the configured ceiling rather than replaced by an earlier local delay. The same-request recovery window starts after the first retryable provider failure. When the peer's requested delay is at least as long as what remains, the provider layer neither sleeps past its window nor substitutes a shorter local delay: the turn ends with the peer's own typed error, and the goal-level retry waits the peer's value clamped to `max_delay_ms`. A local backoff that would outlive the window ends the turn as `provider_retry_deadline`, retaining the last structured provider code plus recovery and total elapsed times for durable diagnosis.

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

- Transport failures, rate limits, incomplete streams, SQLite writer contention, and empty assistant messages schedule another goal turn.
- An explicit Agent `steps` limit normally produces a text-only finalization. `StepLimit` recovery is reserved for a provider that attempts to continue with tools after that finalization boundary.
- Context-limit failures compact retained history before retrying. Successful compaction is persisted as its own retry phase so a restart does not compact the same history twice.
- Authentication failures and user interruption pause the Goal with typed reasons. Human
  input and permission waits carry the durable request id in the pause row.
- A timeout or lost response around a non-replayable side effect pauses with
  `uncertain_side_effect`; recovery requires authoritative-state inspection and never
  automatically invokes the tool again. The obligation is durable on the tool record
  rather than on the pause: the dispatcher writes `state.outcome = "uncertain"` and
  `state.uncertain` with the tool id, the call id, the paths the call reported having
  applied, a typed `cause` of `lost_outcome` or `interrupted`, and `observedAtMs`, in the
  same statement that makes the result model-visible. A process that dies after that write
  and before the pause row is recorded still refuses to run the Goal: the next
  continuation consults the pending records of the current objective and pauses again.
  `state.uncertain.reconciledAtMs` is absent for exactly as long as the inspection is
  owed, and the Goal's `created_at_ms` scopes the query, so a new objective does not
  inherit the previous objective's obligations.
- A turn stopped by its own budget policy pauses with `turn_budget`. The allowance
  belongs to one turn, so the Goal keeps whatever token budget remains, but execution
  does not resume automatically: the next turn would spend the same allowance the same
  way. This is distinct from the `budget_limited` status, which is the Goal's whole
  budget being spent.
- A budget policy may ask for compaction instead of a stop. That is classified as a
  context-limit recovery and follows the same path: retained history is compacted,
  the exact durable summary is projected to live clients, and the turn is retried
  inside the same host drive.
- A proactive context-threshold crossing inside a multi-step tool turn follows that
  same typed path. Before every provider request after the first, the loop uses the
  previous response's provider-reported context usage when available and otherwise
  the current assembled prompt estimate. It emits `context.compact` and yields
  `TurnError::CompactionRequired` before writing an assistant checkpoint, prompt
  receipt, or provider-request row. The host consumes that internal signal, compacts
  the retained history, projects the persisted summary, and retries in the same
  driver cycle. A successful intervention does not emit a terminal turn failure,
  increment `failed_turns`, schedule a Goal retry, or wait for another user, report,
  or timer wake. Automatic recovery is bounded to five compactions in one host
  drive; exhausting the bound or failing compaction stops with the typed compaction
  failure instead of looping. `compaction.auto: false` disables the proactive
  threshold trigger.
- A provider-confirmed context-limit failure enters the same immediate host recovery
  with `CompactionTrigger::ContextLimit`, preserving the provider's used/limit
  fields and the existing five-attempt context-compaction budget. A process-restart
  Goal retry remains a fallback for a previously persisted context-limit failure,
  not the normal path for an in-process threshold crossing.
- The budget policy is consulted before every provider request and after every
  response. The profile-published `TurnAllowance` may add a tool-call ceiling and a
  wall-clock ceiling that apply with or without a Goal; a reached ceiling stops the
  turn with `tool_call_budget` or `time_budget`. Under a token budget the user set on the
  Goal, usage the provider did not report stops the turn with `usage_unknown`; under the
  host default the turn keeps going and the default binds on what was counted. A ceiling
  wins over compaction or continuation and yields to a Goal stop.
- The budget policy's own storage failures keep their type. Reading or accounting the
  goal budget against a database another writer holds locked, a `SQLITE_BUSY` that
  outlasted the pool's busy timeout, ends the turn with the same typed `DbError::Busy`
  as any other contended write, so the Goal persists a `database_busy`
  exponential-backoff retry and stays active instead of pausing with `turn_budget`. Any
  other database failure while reading or accounting the budget still stops the turn
  with `usage_unknown`, because the turn must not continue unmeasured, and the Goal
  pauses until the database is readable again. Durable goal state this build cannot read
  at all, such as an undecodable value, an unknown format, or a status outside the
  closed set, still blocks the Goal, because no retry makes it readable. The turn host
  classifies every durable-storage failure it meets through the same
  `GoalTerminalFailure::from_db_error` rule rather than a rendered message, so contention
  in the plan-reconciliation driver, in creating a human request, and in marking a
  retry's context compaction now schedules a `database_busy` retry where it previously
  blocked the Goal as `host_permanent`; every other `DbError` on those paths blocks as
  `database_permanent`.
- Invalid provider protocol, unsupported typed input such as an image sent to a text-only model, unavailable agent/model configuration, corrupt durable state, and other permanent failures block the goal. The same transaction stores a stable typed code and scrubbed explanation in `blocked_reason`; a permanent runtime failure never produces an unexplained blocked Goal.

OpenAI and Compatible Responses decoders treat `response.failed` as a typed
provider failure, not as an ordinary assistant `MessageEnd(Error)`. When the
event carries a structured error body, its type, code, and message remain in the
error source chain for diagnostics while recovery still follows the typed
`ProviderError` variant.

### Turn allowances

The standard profile publishes a typed `TurnAllowance` through
`zuno_harness::turn_allowance_bundle`; `default_profile_with_tools_and_allowance` is the
seam for a host with its own view. `DEFAULT_TURN_ALLOWANCE` is
`TurnAllowance::UNLIMITED`: the standard host invents no Goal-token, tool-call, or
wall-time ceiling. `goal.default_token_budget` makes the CLI publish a fallback token
allowance for Goals whose durable `token_budget` is unset; an explicit Goal budget
always wins. Every stop is a typed `TurnError::BudgetLimited` and is surfaced to clients
as a `notice` with code `budget.<kind>`; a compaction request is `budget.compact`.

Automatic Goal continuation checks the provider-retained history rather than the full
message table before deciding whether a user anchor exists. If compaction retained an
assistant-only tail, the host durably re-admits the Goal objective before the next turn;
the synthetic compaction marker is never mistaken for user authorization.

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
resource deny still wins. Explicit `danger-full-access` changes the effective
permission mode to `allow_all`; in that trusted native-execution mode no Zuno
approval request is emitted, even if the authored permission mode is `strict`.

The shell's destructive-command gate is independent of strict mode. A protected
target is denied, while a bounded deletion, a dynamic destructive target, or a
redirect that would replace an existing path marks the ordinary `shell`
permission request as human-only. Effective `allow_all` suppresses that
confirmable request, while the gate's catastrophic outcome remains a direct
denial. Permission rules still evaluate first, so an explicit deny remains
terminal; a model-authored argument cannot approve its own operation. A new
static redirect target inside the working directory or the OS temporary
directory is creation rather than overwrite and does not receive this extra risk
prompt. An exact, non-recursive forced removal of a statically named path that is
currently absent below the OS temporary directory is likewise a no-op cleanup.
This filesystem probe is advisory risk classification; actual confinement comes
from the separately selected sandbox mode and backend.

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

Tool execution is at-most-once by default. `ToolReplayPolicy::Never` is inherited by every tool unless the implementation explicitly declares `Safe`; current safe tools are read-only or idempotent inspection operations such as file reads, glob, grep, skill lookup, current-session History, Notes reads, job status, LSP inspection, goal status, and web search/fetch. Notes writes remain `Never`.

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

## File tool path authority

`read`, `write`, `edit`, and `apply_patch` resolve a path once, at authorization, and
then operate through the directory handles that resolution retained. Resolution
descends from the authorization boundary — the workspace root, or an explicitly
granted external directory — one segment at a time, opens each segment without
following symlinks and requires it to be a directory, and never resolves the name a
second time. The property is exact: the call reaches the directory object the user
approved, or it fails. A symlink as the final component is the one deliberate
exception. It is followed once, before authorization, so the user approves the
destination, and the link itself survives the write.

Two weaker repairs were rejected. Refusing to follow the final component alone does
nothing, because the substituted object is an intermediate directory. Re-canonicalizing
after authorization is still a check followed by a separate use, so the window only
moves.

The mechanism is per-platform, and the guarantees are not equal:

| Target | Mechanism | Window |
| --- | --- | --- |
| Linux | Each segment opened through `/proc/self/fd/{fd}/{segment}`, which the kernel resolves relative to the pinned descriptor | Closed |
| macOS | `root/relative` opened with `O_NOFOLLOW_ANY`, plus a file-identity re-check before publishing | Narrowed: `rename` cannot carry the flag |
| Windows | Segment walk with `FILE_FLAG_OPEN_REPARSE_POINT`, refusing any component carrying `FILE_ATTRIBUTE_REPARSE_POINT`, plus a volume-serial and file-index re-check before publishing | Narrowed |
| Other targets | The same segment walk with a `symlink_metadata` refusal and the same pre-publish re-check | Narrowed |

Only Linux closes the window completely, because only Linux offers a way to name a
path relative to an open descriptor without first-party `unsafe`, which this workspace
forbids. `openat2`, `renameat`, and `GetFinalPathNameByHandleW` are therefore not used.
Refusing a reparse point per component covers Windows directory junctions as well as
symlinks, which matters because a junction is the commoner attack shape there.

Publication distinguishes two failures, because they do not deserve the same receipt.
A failure before the rename that makes new content visible leaves the destination
holding exactly its previous bytes and is reported as an ordinary tool failure. A
rename that fails after partially completing, or whose result is lost, is reported as
`Uncertain` carrying the paths already applied. That outcome is not retryable and is
never replayed: it requires authoritative-state inspection, which is the same rule the
snapshot store follows for a half-applied restore. A replacement whose destination is a
symlink also refuses a chain longer than 40 links rather than following it, because at
that length the chain is a loop rather than a deliberate redirection.

## Native search and shell isolation

`glob` and `grep` use the official `rg` executable as their only search engine.
Zuno contributes a thin adapter for typed arguments, cancellation, bounded JSON
decoding, stable ordering, and result shaping; it does not maintain a second
ripgrep-compatible walker. Discovery is lazy, so unrelated commands and tools do
not require ripgrep. When either search tool is invoked, `rg` major version 14 or
newer must be available on `PATH` (or packaged beside Zuno by a distributor);
missing or unsupported ripgrep is a typed tool error, never a silent fallback.

Discovery is cached, but not symmetrically. A successful resolution is kept for the
process lifetime, so session remounts and child turns do not spawn `rg --version`
repeatedly. A failure is kept for five seconds and then re-probed. The asymmetry is
deliberate: ripgrep backs only `glob` and `grep`, so a user who installs it during a
session has to get those tools working without restarting Zuno, while a model looping
on `grep` with nothing installed must not spawn one probe per call. Concurrent first
callers make a single probe between them, and a probe that panics does not make
ripgrep permanently unavailable.

The Shell tool is admitted through tree-sitter command analysis, the deterministic
destructive-command gate, and permission checks, then compiled by the selected
sandbox backend before process-tree containment starts. Existing redirect targets and other confirmable destructive
operations require a fresh attached-user decision unless the effective
permission mode is `allow_all`; catastrophic targets remain hard-denied. Static
creation under the working directory or OS temporary directory and exact
non-recursive `rm -f` cleanup of an absent OS-temporary path do not require a
decision. Strict authorization adds HITL to every side-effecting shell call;
neither mechanism adds confinement.

`config.shell` is passed into the same resolver used by non-interactive command
execution. Selection is explicit configuration, then the operating-system account
shell, then inherited `SHELL`, then the platform fallback. An invalid explicit value
is a configuration error rather than a silent fallback.

The submitted command and the interpreter identity remain separate throughout the
runtime. Durable tool-output titles contain the exact command text, such as
`git diff --check`, so a client can display or copy it without inventing a different
invocation. The resolved interpreter remains available as typed `shell` metadata
(`zsh`, `pwsh`, and so on), and ACP publishes it as
`_meta.zuno.interpreter`. POSIX execution still invokes the resolved executable with
`-lc`; PowerShell uses its non-interactive `-Command` form. A client must not flatten
that relationship into `zsh git diff --check`: that text is neither the submitted
command nor an argv-equivalent representation of `zsh -lc 'git diff --check'`.

Terminal selection and model-command admission intentionally diverge after path
resolution. A PTY may start any executable login shell. A model command may start only
a POSIX shell or PowerShell because those are the syntax families the permission and
risk gates understand. Fish, Nushell, unknown interpreters, and `cmd.exe` fail closed;
they are not treated as Bash-compatible aliases.

On Linux, the backend resolves trusted system bubblewrap outside the workspace,
probes namespace support, mounts the host root read-only, overlays exact writable
roots, reapplies protected descendants, drops capabilities, sets `NoNewPrivs`,
and installs seccomp in a first-party helper. Network is denied by default.
The process layer accepts only `PreparedCommand`; it cannot spawn a
confinement-required Shell call from raw argv.

Bubblewrap discovery is cached for the life of the process. The cache key is the
canonical workspace plus the helper executable, the current `zuno` binary, which are
exactly the inputs a fresh discovery would use, so the first restricted-mode resolution
in a process pays for the `bwrap --help` and namespace probes and every later turn host
and child turn reuses that backend. A hit is served only after the trusted launcher, the
trusted no-op executable, and the helper are re-inspected on disk; if any of them
changed, the entry is evicted and the host is probed again, which is how a bubblewrap
upgrade or a replaced binary is picked up. A failed discovery is never cached, so a
transient probe failure cannot pin the process to unavailable, or under a trusted
`run-unconfined` policy to native execution, for the rest of its life. The cache does
not cross process boundaries: another process must prove the host again. The
policy-specific `verify_deployment` probe that the `SandboxResolver` runs per resolution
is kept, while `zuno debug sandbox` and the deployment report bypass the cache entirely
so a diagnostic describes the host as it is now.

`zuno debug sandbox --mode workspace-write --network deny --check` verifies the
same deployment path. It rejects a non-root-owned, writable, special-bit, or
file-capability launcher; checks every launcher ancestor; revalidates device and
inode before preparation; probes required namespaces; and executes
`/usr/bin/true` through bubblewrap, capability dropping, `PR_SET_NO_NEW_PRIVS`,
and seccomp. A metadata-only probe is not reported as deployment readiness.
`--check` remains strict: it fails when the requested confinement is not
deployable even if a trusted runtime fallback would be eligible.

The public modes are `read-only`, `workspace-write`, and
`danger-full-access`. `workspace-write` is the default. Trusted global, explicit,
managed, environment, and CLI sources define the maximum; project configuration
may only narrow it. An Agent's capability contract is intersected with that
maximum, so a read-only Agent remains read-only under a wider invocation.

`read-only` and `workspace-write` require a proved OS backend and fail closed by
default. The trusted `sandbox.onUnavailable` setting accepts `deny` (default) or
`run-unconfined`; the same value can be supplied by
`--sandbox-on-unavailable` or `ZUNO_SANDBOX_ON_UNAVAILABLE`. Project
configuration may set only `deny`. Global, explicit, environment, CLI, and
managed layers may enable fallback, and managed policy has final precedence.

The trusted `sandbox.backend` setting selects the execution backend
independently of the mode: `auto` (default) discovers the confined backend, and
`native` resolves every request, read-only contracts included, to the native
backend before any discovery as `resolutionKind` `trusted_native`, keeps the
configured permission mode, and records the requested authority as unenforced.
The same value can be supplied by `--sandbox-backend` or
`ZUNO_SANDBOX_BACKEND`; project configuration may set only `auto`, and managed
policy has final precedence. It is an explicit host declaration, not a
fallback, and the CapabilitySnapshot sandbox identity includes it so an
approval issued under confinement is not reused after a switch.

The `SandboxResolver` completes discovery, capability checks, and a real
`verify_deployment` before publishing Shell. Only unsupported platforms, a
missing trusted launcher, missing required launcher capabilities, and typed
namespace/container-policy unavailability may activate fallback. Untrusted
launchers, invalid policy or paths, seccomp/helper/internal errors, generic
process errors, and command preparation/execution errors remain terminal.
Read-only Agent contracts never fall back; their one native route is the
explicit trusted `sandbox.backend: native` selection.

An unavailable fallback and a trusted `native` backend selection both use the
existing native backend and the same `PreparedCommand`, permission review,
catastrophic-command denial, background, timeout, cancellation, and
process-tree lifecycle. It preserves the original permission mode; unlike
explicit `danger-full-access`, it does not imply `allow_all`. Requested network
denial, writable roots, and protected paths cannot be OS-enforced while the
effective authority is the host process user's authority. The host emits one
warning, and every model request receives the durable `runtime.sandbox` section
while either remains active; the section names its cause (the typed unavailable
reason, or `sandbox.backend: native`), the requested and effective authority,
and the permission mode that still applies.

Explicit `danger-full-access` skips restricted-backend discovery entirely,
retains host filesystem, process, credential, and network authority, and sets the
effective permission mode to `allow_all`. Explicit permission denies and
catastrophic Shell denials remain terminal.

Every path produces a `PreparedCommand` and persists execution-authority schema
version 3: `mode` and `network` are effective authority, while
`requestedMode`, `requestedNetwork`, `resolutionKind` (`confined`,
`explicit_native`, `unavailable_fallback`, `trusted_native`, or `legacy`), and
`fallbackReason` record resolution. Version-2 background records read as
requested equals effective with legacy resolution, so in-flight state remains
recoverable. Tool output mirrors requested/effective authority and fallback
metadata.

Confined macOS and Windows modes currently report unsupported. They remain
fail-closed unless a trusted `run-unconfined` policy is active for a write-capable
Agent; explicit `danger-full-access` remains available independently. The
invariants and E2E matrix are recorded in
[Shell sandbox roadmap](design/shell-sandbox-roadmap.md).

## Resident process containment

Local MCP and LSP servers, process extensions, product agents, PTY sessions, and
background commands share `zuno-process`, but their ownership shapes are
explicit. Local stdio MCP commands are direct process-group leaders, matching
Codex's ordinary MCP topology. They terminate with bounded `SIGTERM` to
`SIGKILL` escalation. Zuno inserts no `__zuno_child_guard` process in front of
or beside MCP.

An ACP session may add required stdio or Streamable HTTP MCP servers to its own
profile bundle. The complete declaration is validated on new/load/resume;
stdio commands are absolute and use the session directory as cwd, HTTP
headers are strictly validated, and SSE is unsupported. All required servers
connect and discover before their tools publish atomically. Partial startup,
session close, load failure, profile replacement, and ACP process exit dispose
the exact started set in reverse order. Client commands, environment values,
and headers remain process-local and redacted rather than durable session data.

Other resident and interactive hosts retain dedicated guards where a surviving
per-tree owner or Unix terminal foreground transfer is required. The direct
child returned by `guarded_argv` is the guard, not the payload; owners request
shutdown through `request_contained_process_shutdown` and reap it only after
the contained group settles. On Linux the guard uses the parent-death signal and
waits on the payload pidfd for immediate, race-free natural-exit observation,
with bounded lifecycle checks when pidfd is unavailable. Windows ConPTY is the
exception: `guarded_terminal_argv` launches the requested program directly
because nesting the resident Job Object guard inside ConPTY prevents reliable
input and natural-exit completion. The PTY owner closes its writer and master,
answers the backend's one inherited-cursor startup query before forwarding
terminal output, and explicitly terminates the direct child's tree on shutdown.
Direct MCP relies on owner close/Drop and therefore does not promise descendant
cleanup after an uncatchable owner `SIGKILL`. The pinned Codex comparison and the
split ownership decision are recorded in
[Resident process containment](design/process-containment.md).

On Windows the guard watches its parent through a real process handle rather than by
polling `tasklist` for the parent PID, which had two defects: a reused PID looked like a
living parent, and a `tasklist` that failed to run looked like a dead one and killed a
healthy payload. The workspace forbids `unsafe`, so the guard cannot open that handle
itself; it starts one Windows PowerShell helper from the absolute path
`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`, so a
workspace-controlled `PATH` cannot substitute it, and the helper's
`Process.WaitForExit()` holds a `SYNCHRONIZE` handle to the parent's process object.
Once the helper has reported that it is armed, PID reuse cannot impersonate the parent,
and the guard spawns nothing per poll. The helper is armed before the payload exists: if
it cannot start, including when PowerShell is absent, the guard fails closed with exit
code 125 and its named diagnostic before the payload starts, because a guard that cannot
watch its parent must not begin work it could never clean up. The helper's verdict is
trusted only when it is unambiguous. A clean exit after arming, or an arm-time report
that the PID named no process, means the parent has exited and the guard terminates the
payload's Job Object. Any other ending, such as a crashed or killed helper, a non-zero
exit, or an unobservable status, writes one diagnostic to the guard's stderr and leaves
the payload running, supervised for its own exit only; a lost helper is never read as a
dead parent. `tasklist` remains only in the idempotency check of `taskkill`-based tree
termination.

Starting `zuno` itself inserts nothing into that tree, and the CLI's own startup is not
a guarded process. A client that spawns `zuno acp`, `zuno serve`, or any other
invocation supervises exactly one process on every supported platform: the process it
spawned runs the command, ending that process ends the command, and the command's
`stdout` and `stderr` reach end of file when it exits. Startup resolves global options
and `ZUNO_*` variables into one in-process value; on Unix it additionally replaces its
own image once, keeping the same process id, so launched processes inherit the resolved
values, and a platform without image replacement dispatches in the process the caller
spawned rather than starting a second one. No `zuno` invocation starts a second `zuno`
to carry that environment, so starting Zuno never arms the PowerShell parent-watch
helper above, and Windows PowerShell remains a backend dependency of that guard rather
than of running the CLI. See
[One invocation, one process](cli/index.md#one-invocation-one-process).

## Background command execution

`shell` registers a command with the process-owned
`BackgroundExecutionService` before spawning it. Explicit background mode and a
foreground attention timeout therefore retain one execution identity and one
process tree; neither path adopts a detached task or starts a second command.
An ordinary foreground command is ephemeral: while it runs, its complete output
is spooled so the normal output policy can inspect it, but its state is hidden
from `/ps` and both the in-memory row and spool file are removed as soon as the
caller consumes the terminal result. A command enters detached background delivery
only when `background: true` was requested. Expiring a foreground observation
deadline returns the same foreground handle; it does not promote the process to
detached delivery. Existing `bg wait/output` can observe that handle without
relaunching it.

The native host keeps the logical foreground operation, its original cycle and
one budget alive across waits. It waits on process/control notifications before
asking the model to poll unchanged work. Accepted steering and interruption stay
responsive; a real completion makes the final output available to the next
request. Process observation timeout is not remote failure, and a missing process
is not proof of a successful exit. Original hard process ceilings and turn/Goal
budgets still apply.

Foreground terminal output, completion ownership and the original tool's
verification receipt commit together before the handle is consumed. It does not
also produce a detached callback. Serial CI/status dependencies therefore remain
foreground by default; independent parallel work or an explicit user request
can select detached execution.

Durable commands keep a bounded 2 MiB live tail, persist complete output
separately, and record status under `.zuno/background`. Each execution owns
four names there: `<id>.status.json`, the `<id>.status.json.tmp` it is staged
through, `<id>.output`, and `<id>.lock` — an advisory ownership claim the
owning process holds open for as long as it owns the row. The service retains
at most 32 terminal commands per workspace and removes the oldest row together
with its `.status.json`, `.status.json.tmp`, and `.output` files. Running
commands are never evicted, and neither is a row another process still owns.
Consequently ordinary `shell` calls no longer accumulate files, while `/ps`,
`bg`, and restart reconciliation keep the state they actually require. Other
tools such as `read`, `grep`, `glob`, and web search never use this directory.

A second Zuno process opening the same worktree reconciles only what it can
prove. A `running` row whose `<id>.lock` it can acquire had an owner that is
gone, so it is rewritten as `uncertain` and never replayed. A row whose claim a
live process holds, and a row that records having run without a claim at all,
are both left exactly as they are. Rows written by Zuno 0.6.6 and earlier carry
no claim marker and have no `<id>.lock`, so a claim can prove nothing about
them: such a row is rewritten as `uncertain` only when the process it recorded
is provably gone. Where that cannot be established — no usable pid, or a
platform with no process-existence query, which today means Windows — the row
stays `running`, stays readable, and stays out of retention rather than being
settled on a guess. Terminal rows written by the older always-durable format
are still discarded on first open.

The `bg` tool supports `list`, `output`, `wait`, and `cancel` for executions owned
by the current session, and `artifact` for output a size limit withheld from any tool
in that session. `output`, `wait`, and `artifact` take an optional `limit` beside
`cursor` and return the cursor the next window starts at, so a caller pages by handing
that cursor back instead of slicing a file with a shell command. `output` and `wait`
with no cursor return the newest window; `artifact` names the `outputPath` the withheld
result carries and starts at the beginning. A window is 16,384 bytes when the call
states no size, and the server clamps any window to 51,200 bytes — the default output
byte limit's number, fixed rather than a function of the configured
`tool_output.max_bytes` — so retrieval can never hand back more than an inline result
would have, and an oversized request is clamped rather than refused. A cursor that
predates the 2 MiB live ring falls through to a window of the execution's on-disk
`.output` file and reports `fromDisk`, which makes a discarded prefix reachable again
instead of clamping the request forward. `artifact` is the retrieval the withholding
notice names, and the only call that pages those bytes without re-running the call that
produced them and without passing the window back through the output limits that
withheld it: an agent profile that hides `bg` leaves its own withheld output reachable
only through `accept_large_output: true`, which re-runs that call. The complete tool
has `ToolReplayPolicy::Never` because one action cancels a process tree. Cancellation
reaches descendants through the shared process containment layer. A hard process
ceiling records failure; a process restart converts a running row this process
owned, or one whose ownership claim it can take, to `uncertain` and never
replays it.

Every execution the service launches runs behind the `__zuno_child_guard` process, so
the exit status the shell tool reads is the guard's, and three codes may belong to the
guard rather than to the command. The guard exits 125 when its own machinery failed.
That says nothing about whether the command ran or what it changed, so the shell tool
reports a typed uncertain outcome that requires authoritative-state inspection and is
never replayed, instead of a receipt claiming `Failed exit 125`. It exits 126 when the
program exists but could not be executed and 127 when the program could not be found;
nothing ran in either case, so the receipt records the code as the guard's with no exit
authority and a detail saying the command never started, and no reader can cite it as
the command's verdict. The codes alone are ambiguous with an ordinary program that
chooses to exit 125, 126, or 127, so a reserved code is read as the guard's only when
the guard's own diagnostic line, prefixed `child-process guard failed: `, is present in
the captured output, which both streams of an execution carry; a program that exits 125
of its own accord keeps its ordinary authoritative receipt. Signal death is not mapped
to a code at all: the guard re-raises the payload's signal on itself, so the execution
records no exit code, the receipt has no exit authority, and it reads as killed by a
signal rather than as `exit 1`. Windows native exit codes such as an NTSTATUS crash code
pass through verbatim.

`StartupEnvironment` shares one service per workspace across parent sessions,
child turns, and in-process session switches. Client projections and `/ps` use
that same service rather than maintaining a second process list. The TUI
subscribes to created and settled execution events, refreshes from the
authoritative service after lag, and updates the right-sidebar `Background`
section even while a model turn is active. Each row carries status, command,
pid, elapsed time, and failure context; the section advertises `/ps` for the
scrollable output view.

A terminal durable command also creates one deterministic
`backgroundExecutionReport` input (`msg_<background-execution-id>`). The report
contains terminal status and directs the model to inspect durable output through
`bg`; it does not inline an unbounded spool and never asks the runtime to replay
the command. Settlement events trigger immediate delivery, while a 30-second
reconciliation pass and workspace reopen scan cover lag and process loss.
Duplicate events reuse the same input id. A crash after promotion but before the
input became model-visible returns that row to its original delivery lane before
redrive. Completed, failed, cancelled, and uncertain outcomes are all reported;
an uncertain outcome explicitly requires authoritative-state inspection.

## Background subagents and product agents

Foreground `task` runs remain attached to the parent turn's hard interrupt. A
cancelled parent waits until the child runner has acknowledged cancellation and
shut down; it cannot return a successful child result from the same cancellation
tick. Every native `task`, foreground or background, is first admitted as a
durable internal `job_*` while retaining a separate child session identifier for
conversational continuation. A foreground Child Turn is owned by an independent
supervisor: dropping or force-aborting the outer `TaskTool` future only cancels
its token and cannot destroy Job settlement. If the child has not stopped ten
seconds after cancellation, the supervisor force-aborts it and settles the Job
as `uncertain`; it never leaves an owned Job permanently `running`. A foreground
Job remains attached to the current tool call and uses quiet delivery; a
background Job follows the selected `reportDelivery`.

Background Jobs are independent after admission. Steering or hard-cancelling the
parent turn does not stop them. They terminate only through an explicit Job
cancellation, closure of their owning session, or process lifecycle shutdown;
their supervisor then records the authoritative terminal status before report
delivery.

Enabled `productAgent` instances register independent static tools backed by a host-installed Codex or Claude Code process. A product invocation has a one-shot `run_*` id and, in background mode, a separate `job_*` id. It does not create a Zuno child session and cannot be resumed as one.

`reportDelivery` supports:

- `nextStep` (default): settle the job and admit the report to the parent inbox atomically, then wake the parent.
- `quiet`: settle the job without admitting a parent input.

For every native child, the host generates `TaskReportMetadata`; the child model
supplies only final prose. The metadata records schema version, optional job id,
optional host-captured Plan `workContext`, child and parent session ids, Agent,
terminal status, final text, usage, host-published report artifacts,
typed-written paths, typed verification records, uncertain side effects, and
evidence collection errors. Changed paths and verification records are derived
only from durable tool metadata, never parsed from prose or arbitrary Shell
output. Each Job persists an `evidence_start_rowid` captured at admission, so a
resumed child contributes only typed evidence created by that delegation rather
than evidence from earlier child turns. A background result is stored in
`agent_job.result`; a `nextStep` report
carries the same value under `subagentReport.metadata`, while `quiet` leaves it
available through the durable Job. Foreground `task` returns the same schema
under its subagent report metadata with its internal Job id. When a report is
admitted to the parent transcript, the same value is stored on the canonical
user message as `message.data.taskReport`. ACP history replay restores it as
`_meta.zuno.kind = "task_report"` plus `_meta.zuno.taskReport`; clients do not
have to fall back to raw tool JSON. TUI database replay carries the same object
as non-rendered replay data, allowing the subagent view to restore status, final
text, changed paths, verification records, uncertain side effects, and evidence
errors without parsing presentation strings.

`report_write` is a native output capability independent of workspace editing.
Its host writer accepts a bounded report and a portable filename, selects an immutable
path under `.zuno/reports/`, and uses the anchored file writer to refuse symlink escapes
and replacement of an earlier artifact. It grants no Shell or source-write authority.
The `runtime.reports` prompt section is generated only when the final tool snapshot
exposes the capability. `runtime.read_only` explains the attempt's Shell contract
without treating it as evidence of the host disk's mount mode.

Task report metadata schema 3 carries an `artifacts` array reconstructed from successful
`report_write` receipts after the Job's evidence boundary, with child ownership checked.
The same metadata flows through foreground results, background Job settlement, parent
inbox delivery, and client replay. Report files are retained as deliverables and are
not automatically deleted by session pruning.

Job settlement and `nextStep` inbox admission share one SQLite transaction.
Wake occurs only after commit. If a process exits after settlement or after an
input was promoted, restart recovery reuses the original report row and returns
it to its admitted lane; it does not create another report or rerun the child.
The recovery scan performs that Job transition before reading the ordinary
pending inbox, so a report stranded in `promoted` can itself trigger the next
turn without waiting for a new user prompt. Recovery holds a process-local
reservation for the session while repairing the row, so a live turn cannot own
the same promoted input concurrently. The watcher takes that reservation only
after a read-only probe confirms that a promoted Job report exists; an empty
restart scan never transiently rejects a foreground user turn.
Queued jobs that never started reconcile to `cancelled`; running jobs lost with
the process reconcile to `uncertain` and are not replayed. Concurrent
process-local wake attempts for one `(session_id, input_id)` are coalesced by an
in-flight lease. A failed wake releases that lease and may be retried against
the same durable input, so the guarantee is one logical report and effective
delivery, not a ban on retries across a crash. That lease stays per input even
though delivery is batched: the wake that wins the turn claims every pending
report, and the wakes that lose find their own row already claimed and return
without driving anything. Normal settlement and restart
recovery use the same bounded wake helper: at most three attempts, beginning at
10 ms and exponentially capped at 100 ms.

Goal completion uses the same transactional barrier for the model tool and
`/goal complete`. Completion is rejected while any Plan step or WorkItem remains
unfinished, any Job is queued or running, or any terminal `nextStep` report is
still queued, steering, or promoted. Consuming that report releases the Job
block. Any uncertain Job blocks regardless of delivery or report consumption
until typed authoritative reconciliation changes its state. A terminal `quiet`
completed, failed, or cancelled Job has no parent input and does not block
completion. A pending Goal-owned human request is also part of the barrier; the
Goal cannot complete while it is still asking the user for a decision or
permission.

The `job` tool reads durable status for jobs owned by the current parent session. `JobSubject` distinguishes child sessions, product agents, and workflow runs; status is `queued`, `running`, `completed`, `failed`, `cancelled`, or `uncertain`, together with delivery policy, result, error, and subject identity. Council execution remains stored through the workflow service, while the frontend-neutral projection types `council:<preset>` as a Council and attaches its durable child WorkItems. The same child projection represents ordinary workflow nodes and Council seats, including owner, status, elapsed time, and usage.

The right-sidebar `Jobs` section and `/subagent` consume that same projection rather than reconstructing state from tool output. The sidebar shows compact node or seat progress and the user's current `session_child_first` key binding, with `/subagent` as the command fallback. The detailed view keeps workflows and Councils as jobs, never fabricates child sessions, and shows each durable node or seat plus report-delivery and safety diagnostics.

`job_cancel` verifies parent ownership and requests cancellation from the live supervisor. It never pre-settles a job and has `ToolReplayPolicy::Never`; the executor records `cancelled` only after the child session or complete product process tree has stopped. Product protocol or process loss after work may have begun records `uncertain`. A restart reconciles still-running product jobs to `uncertain` and never replays them.

Every delegation also carries a host-derived `logical_key` over the child Agent
and typed `DelegationContract`. A parent cannot dispatch the same logical task
again while its prior Job is queued, running, uncertain, or owns an unconsumed
`nextStep` report, whether the requested execution is foreground or background.
For a new child, session creation and Job admission use one SQLite transaction;
a duplicate or failed admission rolls back both and cannot leave an orphan child
session. A terminal Job also remains duplicate-blocking for the provider Attempt
that created it, so serial tool execution cannot run two identical foreground
calls from one model response after the first call settles.

`job_reconcile` is the only model-facing path that can release an `uncertain`
Job. It is non-replayable, verifies parent ownership, accepts only
`completed`/`failed`/`cancelled`, and requires both an authoritative source and
concrete evidence. It never reruns the original operation. Reconciliation and
replacement of any unconsumed uncertain report are atomic; a `nextStep`
resolution produces one replacement report, while `quiet` remains non-waking.

Codex and Claude Code retain ownership of their native login, configuration, and model choice. Zuno inherits the session directory and proxy environment but never copies product tokens into `AuthStore`. See [Codex and Claude Code product agents](design/product-agents.md).

## Concurrent web search

`web_search` accepts only `queries: string[]`. The consumer deduplicates queries by first occurrence, runs the remaining requests concurrently through a single-query `WebSearchProvider`, and combines cancellation with the turn interrupt.

The first failed query cancels its siblings and waits for every request to settle before returning. Successful output is deterministic regardless of completion order: query content follows input order, sources are merged by rank round-robin, duplicate URLs are removed, and profile-owned query, result, and timeout limits are applied.

Provider adapters normalize transport output into `SearchResult` and `SearchSource`; they do not own batch scheduling or model-facing presentation.

Credential-bearing provider wire URLs are private implementation data. Search
diagnostics retain only provider, scheme, host, path, status, and an error
category. Reqwest errors remove their URL before entering a cause chain, and
query text, authorization headers, and API keys are forbidden from `Debug`,
`Display`, `ToolError`, tracing, response bodies, and retry notices.

## Network egress

`zuno-network` owns the outbound HTTP construction policy shared by model
provider requests, Zuno-managed authentication, catalogs, remote instructions,
remote MCP, and web tools. AWS credential discovery is the deliberate exception:
`zuno-aws-auth` delegates the standard chain, refresh, and SigV4 signing to the
AWS Rust SDK, while the signed Bedrock model request still travels through
`zuno-network`.
Session traffic uses `SessionNetworkPolicy::ProcessEnvironment`, which resolves
the standard HTTP, HTTPS, all-proxy, and no-proxy environment variables when a
connection pool is constructed. A capability that constructs reqwest directly
bypasses this product contract and is incomplete.

`SessionNetworkPolicy::Direct` is an explicit security boundary, not a fallback.
Its caller must declare `DirectPurpose::LoopbackControlPlane` or
`DirectPurpose::CloudMetadata`. Bedrock model traffic is proxy-aware through the
ordinary session client. Credential-provider traffic, including IAM Identity
Center, STS, web identity, container credentials, and IMDS, is owned by the AWS
SDK and follows that SDK release's HTTP configuration rather than a second Zuno
credential transport.

Public web fetch uses the separate `PublicHttpClient` security capability. It
accepts only credential-free HTTP(S) and disables automatic redirects. Each
request and each of at most five redirects resolves the origin locally, rejects
the whole answer if any address is non-public, handles mapped or embedded IPv4
forms including NAT64, and then connects to an already-validated IP through the
selected HTTP, HTTPS, SOCKS4, or SOCKS5 route while retaining the original
hostname for Host and TLS SNI. `NO_PROXY` is the only environment-level direct
selection when a proxy is configured; proxy connection failure never silently
falls back to direct. Redirect credentials are never forwarded. Literal and
resolved loopback, private, link-local, CGNAT, multicast, unspecified,
documentation, and reserved destinations fail before the request is sent.

`webfetch` keeps the existing 30-second default and clamps a requested timeout
to 120 seconds. Timeout diagnostics identify the credential-free route, phase,
and elapsed duration; durable Goal recovery, not a tight tool loop, owns any
safe replay after backoff.

Child processes inherit the process proxy environment unless their typed
configuration deliberately overrides a variable. The agent loop does not
rewrite process globals per session; deployment-specific proxy choices belong
in the environment that launches the Zuno process.

## Prompt workflow V2 acceptance

The repeatable user-facing acceptance procedure covers four scenarios with each
real target model: one atomic implementation, one `deep` root-cause task, one
parallel delegated task, and one Plan-only task. Each run records provider
requests, native tool calls, repeated reads/checks, Plan updates, child Jobs,
report admission and wake behavior, prompt receipts, and final evidence. It
must use a real authorized provider response; an unavailable account, model, or
service is reported as a blocker rather than replaced with a mock.

See [提示词与工作流用户指南](/zh/operate/prompt-workflow) for the commands, expected
observations, and current implementation boundaries.

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
successful unload. A component whose disposer needs longer than the runtime's stop
timeout declares that bound through `Component::stop_budget` instead of relying on the
timeout being raised for every component. Deployment choices belong in profile
configuration rather than hardcoded branches in the agent loop.

## Client surfaces

The TUI, headless CLI, server, ACP adapter, and future GUI consume the same
commands, durable events, inbox, and frontend-neutral projections.
`ActivityProjection`, `WorkStateProjection`, `SessionUsage`, and
`BackgroundExecutionProjection` prevent clients from rebuilding agent-loop state
privately. Cursor replay closes gaps after disconnects; live delivery is only a
wake/latency path. See [client interface architecture](design/client-interfaces.md).

Native file mutations likewise have one presentation policy for live delivery
and replay. `edit`, `write`, and `apply_patch` use an `Editing files` card.
Successful calls with typed state expose only structured add/modify/delete
diffs as visible content while retaining the original result in `rawOutput`;
successful calls without a diff keep a short fallback. Pre-write failures show
actionable text without inventing a diff. Partial or otherwise uncertain writes
remain failed, retain observed paths or diffs, and publish the typed
`uncertain` outcome.

The headless CLI drains its bounded event channel concurrently with turn execution
and closes both the detached observer and the producer on success or failure before
host shutdown. A failed turn therefore cannot leave the renderer waiting forever
for a sender retained only by the failed producer path.

`zuno run --show-reasoning` is an explicit presentation option. It writes only
provider `ReasoningDelta` content to stderr between stable start/end markers,
while final answer text remains on stdout. Signed thinking and encrypted
reasoning are never rendered. A missing start is opened lazily on the first
delta, and every error or stream end closes an open block. JSON mode rejects the
flag and retains the existing structured event output.

`zuno serve --browser-auth` is an explicit loopback-only HTTP surface. Each
process launch creates one 256-bit token and prints its bootstrap URI exactly
once outside tracing. Atomic exchange sets an authority-bound 30-day HMAC cookie
using the private persistent `$DATA/server/browser-auth.key`; the token is then
unusable. Basic Auth and the cookie compose with OR semantics, but unsafe
cookie-authorized methods require an exact current-authority Origin. The
bootstrap query is removed before access logging, and non-loopback resolution
rejects the mode even when Basic Auth is configured.

The design sources and explicit adopt/adapt/reject decisions are recorded in [the harness comparison](design/harness-comparison.md).
