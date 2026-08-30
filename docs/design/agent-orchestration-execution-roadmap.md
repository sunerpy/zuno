# Agent Orchestration and Execution Roadmap

Status: implementation brief, 2026-08-24.

## Decision

Zuno will finish its existing native concurrency, workflow, prompt, patch, and
lifecycle work as one coherent execution architecture.

The next implementation task will:

- keep bounded, opt-in parallel tool execution;
- add explicit limits for delegated agents, owner jobs, workflows, and provider
  requests rather than treating tool-call concurrency as a global resource
  limit;
- make `orchestrator` the default multi-agent entry point while preserving
  direct `build` and read-only `plan` modes;
- replace overlapping agent names with a small set of enforceable
  `AgentProfile` contracts;
- implement Council as a durable fan-out/fan-in workflow, not as a prompt
  convention;
- strengthen the existing `apply_patch` tool instead of adding a second routine
  editing path backed by `git apply`;
- ship a first-party orchestration pack containing agent profiles, workflow
  templates, and selected Skills while keeping scheduling, authorization,
  provider access, persistence, and cleanup in the native core;
- adopt Cordis lifecycle semantics only where they remove duplicate lifecycle
  ownership or enable transactional contribution replacement.

The earlier request used the name `doris`. This document treats that as a
reference to the already approved Cordis plan. A separate Apache Doris or other
`doris` integration is not in scope.

Zuno is not yet released. Redundant names and data structures should therefore
be replaced directly. The implementation must not add compatibility aliases,
migrations, deprecated fields, or parallel old/new control paths merely to
preserve unpublished behavior.

## Evidence baseline

The conclusions below combine the indexed Zuno working tree with pinned source
reviews:

| project | revision | relevant evidence |
| --- | --- | --- |
| Zuno | `b3df827` plus the current uncommitted working tree | tool concurrency policy, workflow runner, structured prompts, agent catalog, `apply_patch`, lifecycle kernel |
| OpenAI Codex | `4582c0a498158063760309c48214a0416a81488a` | `codex-rs/core/src/tools/parallel.rs`, `codex-rs/apply-patch`, orchestrator and model instructions |
| oh-my-openagent | `44c95e9` | agent profiles, model-specific prompt builders, background task manager, Skill resolution |
| oh-my-opencode-slim | `0661d71f` | seven-role roster, Council prompts, presets, background board, built-in Skills |
| DeepSeek Harness | `141eb6f` | bounded tool groups, stable result commit, Jobs, profiles, workflows, vendored Cordis |

These are design references, not dependencies. Prompt text, role names, and
workflow code must not be copied without checking provenance and license.

## Current Zuno baseline

The current working tree already contains substantial parts of the requested
system. They should be completed and simplified rather than reimplemented.

### Already present

- `ToolConcurrencyPolicy::{Exclusive, ParallelSafe, IsolatedBackground}`.
- A validated `concurrency.tool_calls` limit in the range `1..=64`.
- Ordered permission and argument preparation followed by bounded execution of
  explicitly parallel-safe tool groups.
- Stable persistence of tool outputs in model call order.
- Parallel-safe native read, glob, grep, LSP, job, and search operations.
- Concurrent MCP startup across different servers with serialization for
  repeated operations on the same server.
- Background task and product-agent execution with durable jobs, cancellation,
  report delivery, and `Uncertain` reconciliation.
- A workflow schema with `maxParallel`, `maxAgents`, DAG validation, stable
  output ordering, cancellation propagation, and durable work items.
- Per-agent model and reasoning-variant resolution with diagnostics rather than
  silent fallback.
- A structured prompt envelope, typed developer contexts, prompt receipts,
  collaboration modes, Skill selection, and native `AGENTS.md` precedence.
- A model-facing `apply_patch` tool using the `*** Begin Patch` grammar.
- Read-digest freshness checks for `edit` and `write`.
- A Tokio-native component kernel with async prepare/effect/dispose,
  transactional replacement, reverse cleanup, `Uncertain`, and a descriptor-only
  named capability registry beside typed native services.

### Remaining correctness gaps

1. `tool_calls` limits calls within one assistant step, but does not bound all
   active delegated agents accumulated across steps, parents, sessions, or
   workflow runs.
2. Background jobs and continuable child agents need explicit owner and parent
   quotas. A fast-returning task tool must not bypass resource admission.
3. Completion order, UI notification order, durable model-history order, and
   parent-consumption order are not yet modeled as distinct concepts
   everywhere.
4. The current roster merges Oracle into `advisor` and Fixer into `worker` while
   `deep` also covers complex implementation. The role boundaries are difficult
   to explain and easy to drift from actual permissions.
5. Council behavior is not a native execution primitive with quorum, deadline,
   retry, cancellation, and synthesis contracts.
6. Skill resolution, Agent capability selection, and prompt generation now use
   one immutable snapshot and publish it through the profile lifecycle. Restart
   reconciliation still needs a durable stale-generation refusal test spanning
   parent, child, and workflow recovery.
7. `apply_patch` now teaches direct invocation, generator/write selection,
   context-conflict recovery, and uncertain-outcome inspection. Multi-file
   application can still become partially applied if a later filesystem
   operation fails, after which Zuno can only report `Uncertain`.
8. Prompt receipts still lack a complete redacted manifest of final provider
   request ordering and the exact advertised tool-schema digests.
9. The first named-capability vertical slice now publishes extension Tools plus
   Agent Profile, Workflow Template, and source-isolated Skill descriptors.
   MCP/remote-host and final post-permission ToolRegistry descriptors remain.
10. Worktrees created for delegated implementation are not yet a
    lifecycle-owned, quota-controlled resource.

## Target architecture

```text
Session Turn
  |
  +-- PromptEnvelope
  |     immutable AgentProfile + collaboration mode
  |     capability snapshot + selected Skills
  |     Goal / Plan / Todo / Job state
  |
  +-- RunGraph / TaskScheduler
  |     tool-call budget
  |     delegated-agent budgets
  |     provider/model budgets
  |     workflow and owner-job budgets
  |     generation, lease, deadline, cancellation
  |
  +-- Durable execution state
  |     WorkflowRun -> Task -> Attempt -> Event -> Result -> ParentAck
  |
  +-- CapabilityPolicy
  |     tools + Skills + MCP visibility + filesystem/network/env scope
  |     one source for runtime enforcement and prompt description
  |
  +-- Orchestration pack
  |     AgentProfile contributions
  |     WorkflowTemplate contributions
  |     built-in Skill contributions
  |
  +-- zuno-runtime
        lifecycle authority
        typed services + named capability generations
        transactional publish / withdraw / dispose
```

The model may choose among advertised tasks and workflows. It does not own the
scheduler, semaphore, retry loop, deadline, lifecycle, or authoritative work
state.

## 1. Bounded concurrency and backpressure

### Proposed configuration

```json
{
  "concurrency": {
    "tool_calls": 8,
    "agent_tasks": 4,
    "agent_tasks_per_parent": 4,
    "jobs_per_owner": 8,
    "workflow_runs": 2,
    "mcp_connections": 8,
    "lsp_requests": 4
  }
}
```

All numeric limits use `1..=64`. Zero never means unlimited. An explicitly
supported `unlimited` mode, if ever needed, must be a separate unsafe
configuration value with a visible diagnostic.

Provider and model buckets are a second admission layer:

```json
{
  "providers": {
    "myopenai": {
      "maxConcurrentRequests": 4
    }
  }
}
```

The effective delegated-task limit is the minimum of:

1. global `agent_tasks`;
2. `agent_tasks_per_parent`;
3. workflow `maxParallel`;
4. provider/model request capacity;
5. the selected Agent Profile's optional limit.

### Scheduling rules

1. Permissions, argument validation, capability resolution, and durable
   admission happen in model order.
2. Unknown tools and tools without an explicit safety declaration are
   `Exclusive`.
3. `ParallelSafe` calls may overlap up to `tool_calls`.
4. `Exclusive` forms a barrier: earlier parallel calls drain before it starts,
   and later calls do not start until it completes.
5. `IsolatedBackground` means the tool may detach execution only after a durable
   job and quota lease exist. It does not mean unbounded.
6. A full quota queues work in durable FIFO order instead of spawning and hoping
   the operating system absorbs the load.
7. Cancellation is generation-scoped. Late events from a cancelled or replaced
   generation are retained for audit but cannot publish new visible state.
8. A quota lease is released only after terminal state is durable and required
   process/worktree cleanup has completed or become `Uncertain`.
9. No external tool, Agent turn, process, or provider request is automatically
   replayed after an ambiguous failure.

Current implementation status (2026-08-25): native child and product-agent jobs
persist `queued` before entering a fair process-local FIFO delegation queue and
transition atomically to `running` after admission. Restart reconciliation safely
cancels queued jobs and marks running jobs `Uncertain`. Workflow DAG execution is
work-conserving under `maxParallel`: a newly free slot admits the next ready node
in declaration order immediately, while durable results retain declaration
order. Parent cancellation is biased ahead of same-tick node completion and is
rechecked before publishing `Completed`. Foreground native `task` calls now pass
the parent interrupt through `ChildTurnHost`, abort the child control, and wait
for drain/shutdown. Tool concurrency tests prove an over-cap parallel group never
exceeds `tool_calls` and that `Exclusive` barriers both `ParallelSafe` and
`IsolatedBackground` calls. Durable cross-process leases, restart-preserved FIFO
admission, and per-parent/provider/model quotas remain roadmap work.

### Result ordering

Zuno must represent two orders explicitly:

- `settled_at`: the real completion order used for progress UI and diagnostics;
- `model_index`: the stable call order used for durable conversation history and
  the next provider request.

This preserves deterministic history without hiding useful early progress.
Background child results are addressable by durable task ID. Parent notification
does not mean the parent has consumed or acknowledged the result; `ParentAck` is
an explicit state transition.

## 2. Durable workflow and task state

All native task delegation, product agents, Council seats, and workflow nodes
should converge on:

```text
WorkflowRun
  id, template, generation, status, limits, timestamps

Task
  id, run, parent, agent_profile, objective, dependencies, status

Attempt
  id, task, model, variant, capability_snapshot, prompt_receipt, status

Event
  admitted, queued, started, progress, result, failed, cancelled, uncertain

Result
  structured output, artifacts, usage, elapsed time, diagnostics

ParentAck
  delivered, consumed, rejected, superseded
```

Updates use stable IDs and expected revisions. Restart reconciliation never
guesses that a missing process or provider request succeeded.

Workflows remain immutable DAG templates for the first release. Models may
instantiate a verified template and fill bounded inputs; they may not synthesize
arbitrary executable graphs.

## 3. Agent Profiles

### Canonical roster

| profile | mode | responsibility | delegation |
| --- | --- | --- | --- |
| `orchestrator` | primary, default | decompose, admit, track, integrate, and verify multi-agent work | approved subagents and workflows |
| `build` | primary | direct end-to-end implementation when the user wants one execution lane | none |
| `plan` | primary, read-only | investigate, clarify, and produce durable Goal/Plan/Todo state | read-only profiles only |
| `deep` | all | directly selected or delegated root-cause and cross-cutting implementation requiring sustained investigation | none |
| `fixer` | subagent | bounded local code change with focused verification | none |
| `general` | subagent | bounded miscellaneous work under an explicit capability envelope | none |
| `explorer` | subagent, read-only | repository and CodeGraph evidence | none |
| `librarian` | subagent, read-only | official documentation, upstream code, and web research | none |
| `oracle` | subagent, read-only | architecture, root-cause analysis, review, and trade-offs | none |
| `looker` | subagent, read-only | image, screenshot, and visual inspection | none |
| `council-synth` | internal | synthesize structured seat results without tools | none |

`advisor` is replaced by `oracle`; `worker` is replaced by `fixer`. They do not
remain as aliases. `deep` remains distinct from `fixer`: Deep owns a bounded but
cross-cutting implementation objective, while Fixer owns a narrow change whose
scope is already known.

`ui-design` now carries the shared visual and interaction method. Projects may
add a deny-by-default configured `designer` subagent when a separate model or
context is useful. A native `designer` profile remains deferred until real
workflows show that `general` plus the Skill cannot express the required boundary;
Zuno does not copy another project's nominal roster for name parity.

### One enforceable profile

```rust
struct AgentProfile {
    id: AgentId,
    role: AgentRole,
    prompt: PromptManifest,
    model: ModelPolicy,
    capabilities: CapabilityPolicy,
    skills: SkillPolicy,
    delegation: DelegationPolicy,
    output: OutputContract,
    provenance: ProfileProvenance,
}
```

The same `CapabilityPolicy` controls:

- the tools actually registered;
- Skill and MCP visibility;
- filesystem, network, and environment scopes;
- whether the Agent may mutate state or delegate;
- the capability summary rendered into its prompt.

A prompt must never be the only thing making an Agent read-only or preventing
recursive delegation.

Implementation checkpoint (2026-08-24): `AgentProfile` now freezes the resolved
catalog definition, final ordered permission rules, declared delegation targets,
and model-visible capability summary once per turn. Prompt assembly, model and
reasoning selection, tool visibility, dispatch authorization, and `agent list`
consume that snapshot instead of independently recomputing permissions. The next
profile commit still needs to collapse the remaining native routing metadata into
the same definition while introducing the canonical roster below.

### Model and reasoning policy

- Profiles inherit the session model by default.
- Any profile may configure a specific provider/model and reasoning variant.
- The orchestrator, plan, deep, and oracle profiles default to higher reasoning
  bands; explorer, librarian, looker, fixer, and general may use cheaper bands
  when configured.
- Missing models, unsupported reasoning variants, or provider limits produce a
  visible diagnostic. Zuno does not silently downgrade.
- The selected profile and preset are immutable for one Attempt. Runtime preset
  switching creates a new Attempt, explicit fork, or new session rather than
  mutating an in-flight execution.

Implementation checkpoint (2026-08-24): the canonical configuration schema now
owns typed `preset`/`presets` data. The production top-level turn and native
`task` delegation both consume one frozen `PresetLibrary`; Agent routes,
category shorthands, canonical reasoning levels, diagnostics, and precedence
therefore use the same policy instead of a second JSON parser or compiled model
table.

## 4. Prompt system

Each Agent prompt contains six typed contracts:

1. purpose and success condition;
2. evidence and inspection discipline;
3. actual capability and authorization boundaries;
4. delegation or non-delegation rules;
5. durable work-state and cancellation behavior;
6. required return schema and verification evidence.

Model-family variants may adjust wording or tool-use examples, but they do not
fork authorization, workflow semantics, Skill resolution, or completion rules.
Runtime-enforceable behavior must remain code, not a long prompt.

Required prompt improvements:

- give `orchestrator` an explicit routing matrix for `deep`, `fixer`, `general`,
  `explorer`, `librarian`, `oracle`, `looker`, and Council;
- require a bounded deliverable, owner, capability envelope, and expected output
  for every delegation;
- prohibit duplicate delegated research and recursive child spawning;
- require the parent to integrate and verify instead of pasting child output;
- describe uncertainty and cancellation without claiming that irreversible work
  was rolled back;
- align patch guidance with the actual model-facing tool;
- render selected Skills as typed prompt blocks from one capability snapshot;
- persist a redacted final provider-request manifest, ordered input-item hashes,
  and tool-schema hashes alongside the existing receipt.

Prompt tests must prove that the declared role and actual runtime capabilities
agree. A read-only profile that can see `shell`, `edit`, `write`, or
`apply_patch` is a test failure.

## 5. Patch and file-editing reliability

### Primary contract

Codex's current model-facing edit path is a custom `apply_patch` grammar, not a
raw Git unified diff. Zuno already implements the same high-level
`*** Begin Patch` envelope and should keep one primary structured patch tool.

The prompt should teach:

- use `apply_patch` for focused source edits and coherent multi-file changes;
- call the dedicated tool directly rather than routing it through `shell` or a
  heredoc;
- use `write` only for a new file best expressed as complete content or an
  intentional full replacement;
- do not patch generated output when the owning generator or formatter should
  run instead;
- use enough unique hunk context and keep patches narrow;
- after a context conflict, re-read the affected region and generate a new
  patch; never retry the same stale patch blindly;
- after a successful tool receipt, use its returned diff and content digests
  rather than re-reading files without a reason;
- verify behavior with the narrowest relevant test or check.

The production model surface now exposes `read`, `write`, and `apply_patch`.
The older exact-replacement `edit` implementation remains an internal runtime
capability but is not advertised as a competing model editor. The tool
description and context-conflict errors implement the selection and recovery
rules above, and tests pin those clauses. This does not complete the
transactional requirements below.

### Transactional apply

Before any side effect, `apply_patch` must:

1. parse the complete patch;
2. canonicalize and authorize every source and destination;
3. reject duplicate destinations, move cycles, path escapes, binary input, and
   conflicting operations;
4. require a current session read for Update and Delete operations;
5. preserve detected line endings unless explicitly converting them;
6. compute every resulting file and digest in memory;
7. acquire mutation locks in stable path order;
8. revalidate source digests;
9. prepare a rollback journal or equivalent atomic replacement plan.

Only then may it commit filesystem changes. If the platform cannot guarantee a
single atomic transaction across all paths, the tool must attempt bounded
rollback. A failed or ambiguous rollback reports `Uncertain` with exact affected
paths and never reports success.

### `git apply` fallback

Routine model-authored edits must not depend on Git or shell parsing. Repositories
may be non-Git workspaces, and shell-based `git apply` weakens tool-level
authorization and structured diagnostics.

An explicit user-supplied unified diff may later use a guarded `unified_diff`
input mode that performs dry-run validation, path authorization, and the same
transaction protocol. Until that parser exists, invoking `git apply` is an
ordinary shell action subject to permission policy; it is not an automatic
fallback.

## 6. Native Council

Council is an execution service plus an internal synthesis profile:

```text
CouncilRequest
  question
  seats[]
  quorum
  deadline              # end-to-end wall-clock bound
  retry_policy
  synthesis_policy      # reserved timeout and structured-input bound

SeatResult
  verdict
  confidence
  evidence[]
  risks[]
  recommendation
  status
```

Rules:

1. Seats run in isolated child contexts and consume normal delegated-task and
   provider quotas.
2. Council members are read-only by default.
3. The runtime owns concurrency, timeout, retry, cancellation, and quorum. A
   preset reserves an explicit synthesis budget inside the end-to-end deadline,
   so a timed-out non-quorum seat cannot make an already-reached quorum
   impossible to synthesize.
4. Empty or malformed seat output is a typed failure, not a prompt hint.
5. The synthesizer has no tools and receives bounded structured seat results or
   artifact references, not duplicated full transcripts.
6. Dissent is preserved. A synthesis cannot erase which seats disagreed.
7. The complete Council run is durable and visible as one Workflow with child
   tasks.
8. `/council`, an explicit workflow invocation, and a permission-gated
   `council_run` tool may share the same service.

## 7. First-party orchestration pack

Multi-agent composition can be packaged to minimize changes to the engine, but
it cannot be an untrusted plugin that owns the execution loop.

A first-party static `zuno-orchestration` component should contribute:

- Agent Profiles;
- Workflow Templates;
- Council presets;
- built-in Skill metadata and bodies;
- TUI labels and semantic presentation hints.

The core continues to own:

- provider clients and authentication;
- prompt-to-provider mapping;
- Tool manifests and permission decisions;
- scheduler and concurrency leases;
- sessions, jobs, usage, and work-state persistence;
- cancellation and process-tree cleanup;
- capability publication and lifecycle state.

This follows the useful DSH split between profiles/workflows and the lifecycle
kernel without treating a Profile or Bundle as a security boundary.

After the named-capability registry exists, external packages may contribute
descriptors through a transactional capability generation. Raw provider
objects, credentials, database handles, or `HarnessRuntime` are never exposed.

Implementation checkpoint (2026-08-28): the static
`zuno-orchestration` crate is data-only and contributes the nine Skill
descriptors below, including stable source ids, hashes, profile/tool gates, and
license provenance. It deliberately owns no scheduler, provider, permission,
session, or lifecycle service. The native profile composition root publishes
its immutable snapshot as a typed service and projects extension Tool, Agent
Profile, Workflow Template, and Skill descriptors into one transactional named
capability generation. Same-name Skills remain independently addressable by a
source-derived isolation scope.

## 8. Built-in Skills

The first orchestration pack should adapt, not copy, these workflow ideas:

| Skill | Zuno behavior |
| --- | --- |
| `deepwork` | create or validate durable Goal/Plan/Todo state, choose bounded lanes, and define verification gates |
| `codemap` | use the native CodeGraph index and return a scoped structural map; do not run a duplicate JS scanner or rewrite `AGENTS.md` automatically |
| `verification-planning` | define evidence, commands, fixtures, and acceptance surfaces before implementation |
| `reflect` | use a typed, redacted, bounded session-query API and propose reviewable memory candidates |
| `customize-zuno` | explain native configuration, providers, auth, permissions, agents, workflows, Skills, MCP, and plugins |
| `develop-zuno` | route a Zuno customization to configuration, Agent Markdown, a user-owned Skill or command, an extension package, or a native component |
| `worktree` | guide explicit user-authorized worktree creation, isolated work, integration, and conservative cleanup without pretending to own leases or Shell authority |
| `git-workflow` | preserve user changes, scope commits, inspect history when needed, and verify the staged diff |
| `ui-design` | align with the existing design system, separate visual observation from implementation, and require real runtime visual evidence |

`git-workflow` is the adapted Zuno name for the useful portion of OMO's
`git-master` guidance. The `worktree` Skill may guide ordinary user-authorized
Git operations, but product-owned leases, quotas, and automatic cleanup still
require a runtime lifecycle service rather than hidden prompt prose.

`dual-review` and `auto-release` are deliberately not built in. The named workflow's
reviewer topology, release gates, remote authority, artifact targets, and rollback
policy are owned by the user or project. They may be authored as global or project
Skills; because a unique Skill already receives a direct slash entry, a second
Markdown command is still unnecessary. The built-in `balanced-review` council is a
generic synthesis primitive, not an implementation of either user-owned workflow.

`ui-design` is the shared method source for UI work. A project may add a configured
`designer` subagent with a dedicated model and context, but the native roster remains
unchanged until real workflows prove that `general` plus this Skill is insufficient.

Every built-in Skill records source inspiration, license review, version,
content hash, allowed profiles, required capabilities, and tests.

Skills do not grant tools. Selecting a Skill can only narrow or explain the
capability snapshot already authorized for the Agent.

The built-ins are registered before disk discovery with distinct
`builtin://zuno-orchestration/...` identities. They are available through the
same prompt catalog and typed `skill` tool as external Skills. When a name is
unique and does not collide with a real command, the TUI also exposes
`/<skill-name>` and loads that exact source before the next provider request.
Same-name sources remain independently addressable and never acquire a hidden
precedence winner.

## 9. Worktree resource management

Delegated code work may request:

```text
WorktreeLease
  id
  repository
  base revision
  branch
  path
  owner task
  byte quota
  created_at
  expires_at
  cleanup state
```

The service must:

- serialize conflicting branch/path operations;
- refuse broad or unresolved deletion targets;
- track disk use and configured global/per-workflow quotas;
- prevent two tasks from unknowingly writing the same worktree;
- stop owned processes before cleanup;
- retain an `Uncertain` record if process or filesystem cleanup cannot be
  proven;
- expose explicit promote, merge, keep, and cleanup outcomes;
- never commit, merge, delete user branches, or remove a worktree merely because
  an Agent prompt said to do so.

Temporary reference clones and CodeGraph indexes also need an owner and cleanup
policy so parallel research cannot fill the disk.

## 10. Cordis Phase 0 and Phase 1

This work adapts Cordis lifecycle semantics into the existing native component
kernel; it does not add an independent Cordis runtime.

### Phase 0: contracts

- dependency disappearance, replacement, restoration, and stale-generation
  tests;
- reverse async cleanup and `Uncertain` tests;
- single-step tool, owner-job, parent-Agent, global-Agent, workflow, and provider
  quota tests;
- stable public result order with independent settlement order;
- no late publication from an invalidated generation;
- no automatic replay of non-idempotent work.

### Phase 1: named contributions

Add a named plane beside typed Rust services:

```rust
struct CapabilityKey {
    namespace: String,
    name: String,
    version: CapabilityVersion,
    scope: CapabilityScope,
}
```

Each contribution records owner, generation, contract, availability, and
provenance. Candidate contributions are fully prepared and conflict-checked
before atomic publication. Withdrawal happens before old cleanup.

The first vertical slice publishes tool, Agent Profile, Workflow Template, and
Skill descriptors. Native executable objects remain owned by the core.

Model-authored executable packages are a later isolated-process or WASI phase.
Because useful agents and workflows need network, files, and environment data,
the future guest contract must expose scoped file facades, allowlisted network
proxies, copied environment keys, deadlines, quotas, audit, and explicit
withdraw/cancel/dispose/quiescence. It must not expose unrestricted host
filesystem, sockets, environment, credentials, or a Rust dynamic-library ABI.

## 11. TUI and observability

The existing durable projections should expose:

- active and queued delegated Agents;
- Agent Profile, model, reasoning variant, objective, parent, elapsed time, and
  token usage;
- active workflows and Council seats;
- background terminals and product agents;
- quota use such as `agents 3/4` and `provider myopenai 2/4`;
- queued, running, waiting, cancelling, completed, failed, and `Uncertain`
  states;
- whether a child result is delivered and whether the parent acknowledged it.

The sidebar remains a projection, not the source of truth. `/jobs`, `/ps`, the
subagent view, and Council details consume the same durable state.

The main timeline shows compact summaries. Full task output and transcripts stay
behind the existing detail view. Internal hidden reasoning is not copied into
parent history or the TUI.

## 12. Delivery sequence

Implementation should proceed through small, independently reversible commits.
Do not combine the existing large dirty working tree into one commit.

### Commit 1: contracts and configuration

- finalize the concurrency schema;
- add scheduler and lifecycle contract tests;
- define `AgentProfile`, `CapabilityPolicy`, and durable run-state types.

### Commit 2: scheduler and backpressure

- add delegated-Agent, parent, owner-job, workflow, and provider leases;
- queue on saturation;
- separate settlement order from stable publication order;
- verify cancellation and restart reconciliation.

### Commit 3: patch reliability

- replace the terse tool description;
- add current-read and digest revalidation;
- precompute complete patches;
- implement rollback/`Uncertain` behavior and line-ending preservation.

### Commit 4: profiles and prompts

- add `orchestrator`, `fixer`, `general`, and `oracle`;
- redefine `build` and `plan`;
- remove redundant `worker` and `advisor`;
- align prompt manifests with enforced capabilities;
- persist final provider/tool-schema manifests.

### Commit 5A: preset routes and first-party Skills

- define typed, schema-generated Preset configuration with no legacy flat form;
- route top-level and delegated Agent model selection through one frozen policy;
- add the static first-party Skill pack and direct unambiguous slash loading.

### Commit 5B: immutable orchestration snapshot and templates

- freeze parent, child, workflow, model, reasoning, Skill, and tool capability
  identity for one Attempt;
- move first-party Profile and Workflow Template descriptors behind that
  snapshot without moving runtime authority into the pack;
- persist enough identity for restart reconciliation and diagnostics.

### Commit 5C: Council and worktree resources

- implement Council as durable quorum-aware fan-out/fan-in over existing child
  sessions and workflow jobs;
- add the lifecycle-owned worktree lease, quota, cleanup, and `Uncertain`
  contracts;
- keep synthesis tool-free and preserve stable seat/result order.

### Commit 6: named capabilities

- implement Cordis Phase 0/1 contracts;
- transactionally publish orchestration descriptors;
- validate one real replacement/unload path.

### Commit 7: TUI, E2E, and documentation

- render Agent, Workflow, Council, quota, and ParentAck state;
- complete end-to-end orchestration and cleanup tests;
- update configuration, prompt, Skill, plugin, and lifecycle documentation.

Each commit runs its targeted tests before broader workspace gates. Before every
commit, verify the exact staged diff and preserve unrelated user changes.

## 13. Acceptance tests

### Concurrency

- eight parallel-safe calls overlap but never exceed the configured limit;
- an exclusive call drains earlier work and blocks later work;
- delegated Agents respect global, per-parent, workflow, owner, and provider
  limits across several model steps;
- saturation queues durably and restart preserves FIFO order;
- completion notifications may arrive early while provider history remains in
  stable call order;
- cancellation releases no lease before durable terminal state and cleanup.

### Agents and prompts

- `orchestrator` is the default and can delegate only to allowed profiles;
- `build` performs direct delivery without recursive delegation;
- `plan`, `oracle`, `explorer`, and `librarian` cannot see mutation tools;
- every Attempt records model, reasoning variant, capability snapshot, selected
  Skills, prompt receipt, and tool-schema hashes;
- unavailable model or reasoning settings fail visibly without silent fallback.

### Council

- independent seats truly overlap;
- quorum, all-pass, dissent, empty response, timeout, retry, and cancellation are
  deterministic;
- synthesis receives bounded structured results and has no tools;
- restart never reruns an ambiguous seat automatically.

### Patch

- update/delete without a current read is rejected;
- stale reads identify the conflicting path and hunk;
- a late multi-file write failure either leaves no change or reports exact
  rollback uncertainty;
- moves, cycles, duplicate destinations, line endings, Unicode, path escape, and
  cancellation are covered;
- generated files use their generator or formatter rather than model patches.

### Skills, worktrees, and lifecycle

- parent and child use the same immutable Skill/capability snapshot;
- Skills cannot expand permissions;
- worktree quota, process cleanup, keep/promote, and uncertain cleanup paths are
  tested;
- replacing an orchestration pack cannot publish events from the old
  generation;
- unload withdraws routes before reverse async disposal.

### End-to-end

Run one real TUI scenario using configured `myopenai` models:

1. the default orchestrator creates several independent research, review, and
   implementation tasks with different reasoning variants;
2. tasks overlap within configured limits and appear in the sidebar;
3. one task is cancelled and one Council seat fails;
4. the remaining results are integrated into a minimal web example in a managed
   temporary worktree;
5. the example is started and accepted through browser automation;
6. Zuno exits with no orphan child process, active job, leaked worktree, stale
   capability generation, or unbounded background artifact.

## Non-goals

- depending on or forking `cordis-rs`;
- a second lifecycle, scheduler, or persistence authority;
- Rust `dylib` hot reload or unsafe ABI bridging;
- prompt-driven concurrency, retry, timeout, or cleanup;
- unbounded concurrency represented by zero;
- recursive delegation by implementation or advisory subagents;
- wildcard authorization of future Skills, MCP servers, or tools;
- copying every OMO/Slim Agent solely to match a marketing roster;
- treating a background completion notification as parent consumption;
- exposing raw credentials, provider clients, database handles, or host runtime
  objects to plugins;
- automatic `git apply` shell fallback for ordinary edits;
- automatic commit, merge, branch deletion, or worktree deletion.

## Next-task entry condition

After Commit 5A, the next implementation task is Commit 5B only:

1. preserve the verified Preset and first-party Skill behavior as contract
   tests;
2. define one immutable orchestration snapshot shared by the parent, native
   child sessions, workflow nodes, restart reconciliation, and prompt receipts;
3. include the resolved Agent, model, reasoning, selected preset, selected Skill
   identities, tool-schema hashes, permission/capability generation, and owner
   lineage without copying credentials or provider clients;
4. reject stale or mismatched snapshots visibly and never replay an ambiguous
   child or workflow operation;
5. produce a small verified commit before implementing Council or worktree
   mutation services.
