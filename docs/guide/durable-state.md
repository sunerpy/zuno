# Goals, plans and todos

Three durable structures track work, and they answer different questions. Goal is why the
work continues. Plan is how it is staged. Todo is what concrete items sit under a stage.

All three live in SQLite, which is the point: durable state, not prose, controls
continuation. Text such as "next I will run the tests" is not progress. A goal that is
still active, a plan step still in progress, or a job whose report has not been consumed
is what keeps a session working.

## The three layers

| Layer | Question | Lifetime | Typical size |
| --- | --- | --- | --- |
| Goal | Why is this session still running? | Until complete, paused, blocked, out of budget, or permanently failed | One per session |
| Plan | What are the stages, dependencies, and acceptance gates? | Revised as work proceeds | A handful of steps |
| Todo | What concrete work sits beneath a stage? | Created and closed within a plan step | Optional, as many as useful |

They are not required to mirror each other. Plan steps are stages; Todo items are optional
concrete work beneath them. Creating one Todo per Plan step mechanically adds bookkeeping
without adding information.

## Goal

A goal is the continuation authority. An active goal continues until it completes, is
explicitly paused or blocked, reaches its budget, or hits a typed permanent failure. That
is what makes long work resumable instead of stopping whenever a turn ends.

Recovery is layered. The provider recovery deadline is anchored when the original request
starts. Its initial request remains transport-governed, while locally jittered backoff and
every replacement attempt must complete before that absolute deadline; expiry cancels and
records an active replay. Before every provider-request backoff, the request layer commits
a `provider_retry_backoff` checkpoint containing the request, turn, failed and next attempt,
typed reason, selected delay, and wait deadline. A restart never revives that old HTTP
request. It observes the remaining wait, then starts a fresh Goal turn. If the bounded
provider sequence still ends in a recoverable error, the Goal controller writes a
`goal_retry` row before waiting and likewise starts a fresh turn when its persisted deadline
arrives.

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

There is no cross-turn retry ceiling for recoverable failures: the delay grows
exponentially, reaches the cap, and the goal stays active. The retry row is tied to the
exact goal id and stores the attempt, typed reason, selected delay, schedule time, and next
eligible time, so reopening the session reconstructs the wait.

Queued user input has priority over an automatic turn, and long waits are split by
`poll_interval_ms` so an interactive surface notices input promptly.

Recovery is selected from typed errors:

| Class | Outcome |
| --- | --- |
| Transport failures, rate limits, incomplete streams, SQLite writer contention, empty assistant messages | Schedule another turn |
| Context-limit failures | Compact retained history, then retry |
| Authentication failures, user interruption, human input, permissions, a spent turn allowance, or an uncertain side effect | Pause with a typed reason |
| Invalid provider protocol, unsupported typed input, unavailable agent or model, corrupt durable state | Block |

Permanent runtime failures store a stable typed code and scrubbed explanation in
`blocked_reason`; a blocked Goal is never left without a diagnosis.

Inspect and manage it with `/goal` in the terminal application:

```text
/goal
/goal <objective>
/goal create <objective>
/goal edit <objective>
/goal pause
/goal resume
/goal block <reason>
/goal complete
/goal cancel
/goal history
```

`/goal` shows the current state. `/goal <objective>` creates a goal when none exists or
the previous goal is complete or cancelled. Otherwise it updates the current objective
without resetting its status, budget, or accumulated usage. The explicit action forms
remain available for lifecycle management. Create, edit, and shorthand objective changes
also reconcile an active Plan: a multi-stage objective archives the previous visible Plan
and installs a new root bound to the current `goal_id`. An atomic objective may terminalize
stale unfinished work without rebinding an already terminal historical Plan.

Creating or editing an active Goal is an execution command, not a status-only write. After
the native command reaches its terminal event, the shared Goal driver immediately prepares
the next autonomous turn. If the session has no earlier user message, Zuno first admits the
user-provided objective through the durable FIFO inbox and persists it as the initial user
turn anchor. The literal `/goal ...` control text never enters provider input.

ACP `session/load` and `session/resume` also inspect the durable Goal after rebuilding the
session runtime. An active Goal resumes in the background without requiring a sacrificial
prompt. This also repairs sessions created by older releases that persisted an active Goal
but stopped before admitting the first user turn.

Known action names take precedence when they are the first token: `show`, `get`, `status`,
`history`, `create`, `edit`, `pause`, `resume`, `block`, `complete`, `cancel`, and `help`.
If an objective itself starts with one of those words, use `/goal create <objective>` or
`/goal edit <objective>` to disambiguate it. `/goal help` prints the compact command
summary.

Goal status also shows the typed pause, cross-turn retry, provider backoff checkpoint,
and pending human requests. Completion is rejected while any Plan step, WorkItem, Job,
next-step report, or Goal-owned human request remains unfinished.

### Success criteria and evidence

A goal that changes the workspace cannot be completed on assertion alone. "The tests pass"
in prose is a claim about the workspace, and the whole reason a goal exists is that claims
made mid-run are the ones most likely to be wrong. So a change goal carries success
criteria, and each one closes only against a recorded exit status.

`goal_propose` takes `success_criteria`, a list of concrete checks. Each becomes a row with
a short id such as `c1`, echoed once in the result. The model cannot rewrite them later:
criteria that could be edited to match whatever happened are not criteria.

Two things close a criterion, both through `goal_update`:

| Field | Meaning |
| --- | --- |
| `satisfy_criteria` | `criterionId` plus the `receiptId` of a check that ran and passed |
| `waive_criteria` | `criterionId` plus a `reason`, recorded verbatim |

Receipt ids come from the tool results themselves. A tool that ran a command appends a line
naming the receipt it recorded:

```text
[verification rcp_01HQ...] passed: cargo test --workspace (exit 0, authoritative).
Cite this id as evidence that the check passed.
```

An exit status that was inferred rather than observed says so instead, and states that it
cannot be cited. That distinction is the point: a shell pipeline whose failing stage was
swallowed by a later `grep` produces a zero exit status that proves nothing, so it is
recorded and refused rather than quietly accepted. See
[Shell exit status](tools.md#what-a-shell-exit-status-proves).

Evidence expires. Every tool call that writes files stamps the goal with the time of the
change, which retires any criterion whose receipt is older than that stamp. The retirement
is reported in the tool result, at the moment of the write:

```text
[goal evidence] 2 satisfied criteria went back to open, because this change came
after the check that satisfied them. Verify again after your last edit and cite the
new receipts.
```

The refusals name what is wrong rather than asking for a retry. A cited receipt that does
not exist in this session, one whose outcome was failure or was undecidable, and one that
predates the last write each produce a different sentence, and the stale case prints both
timestamps so the mismatch is visible. Completing with criteria still open reports which
ids are unproven.

A goal that only answers a question is not gated: it has nothing to verify. The first tool
call that writes a file turns a question goal into a change goal, so the gate applies to
the run that turned out to modify the workspace even though it did not start out planning
to.

The rendered goal document lists the criteria with their state, so a human reads the same
gate the model is held to.

### Capability claims

Some claims are not about the workspace at all. Enabling a provider feature because a
related model is documented to have it, and then reporting success, leaves nothing durable
that says the belief was inferred rather than observed. The `capability_claim` tool records
one claim per capability and subject, and answers plainly whether it may be relied on.

| State | What it requires |
| --- | --- |
| `documented` | At least one cited source |
| `probed` | A receipt from this session that proves success and is newer than the last write |
| `inferred` | Nothing, and it blocks completion |
| `unknown` | Nothing, and it blocks completion |

The completion audit refuses a change goal while a claim recorded under it is `inferred` or
`unknown`, and re-checks a `probed` claim's receipt against the mutation mark at audit time,
so a later write retires the probe without anyone rewriting the ledger. Re-recording a claim
updates the row and reports its previous state, which makes a retraction to a weaker state a
recorded event rather than a refusal or a silent overwrite. Claims outlive goal replacement
as provenance, but only claims recorded since the current goal instance began gate it.

The `bedrock-model-capability-review` Skill says what counts as evidence for an Amazon
Bedrock model: a vendor document naming that exact model id and region, or an observed
probe. It also says to record the claim before writing configuration, not after.

### Generated state stays out of the commit

The goal document, spilled tool output, and background execution records are all generated,
so their directories go into the repository-private `.git/info/exclude` rather than a
tracked `.gitignore`. One registry supplies both that exclude block and the staging refusal
below, so the two cannot disagree about which paths are generated.

The exclude block only keeps them out of `git status`. A `git commit` that would deliver
them anyway is refused before it runs, with the paths named and `git restore --staged` as
the remedy. The check reads the index, and also the tracked working-tree changes when the
command stages as it commits. Outside a repository there is nothing to check and nothing is
refused.

This exists because generated state that reaches the index is how an agent reports a dirty
tree as evidence of a change it did not make, or delivers its own scratch output as part of
the work.

### Token budget

A goal's `token_budget` is enforced around every provider request inside a turn, not at the
turn boundary. A single long turn can otherwise spend an entire allowance before anything
reads a counter. Each response is recorded against the goal first, and the decision is read
back from the row that write produced, so what stops a run is exactly what a human sees
afterwards.

| Condition | Outcome |
| --- | --- |
| Allowance spent | The turn stops and the Goal pauses with `turn_budget` |
| Provider reported no usage | The turn stops; a budget you set that cannot be counted cannot be honoured |
| Only the last tenth of the allowance is left | Compaction is requested, then the turn continues |

The last tenth is held back deliberately. Compaction costs a request of its own and has to
leave room for the summary plus the next real request, so asking for it exactly when the
budget runs out would be asking when there is nothing left to pay with.

### The host's default allowance

A goal whose `token_budget` is unset is not unbounded. `None` means "the host's default",
and the harness's default profile publishes one: forty requests at a 200,000-token window,
or 8,000,000 tokens. Every request re-sends the whole prompt and cache reads are charged, so
that is close to the most one runaway turn can cost.

An explicit goal budget always wins, and the default is never written into the goal row. A
host that genuinely wants unbounded autonomy says so with `TurnAllowance::UNLIMITED`, not by
leaving a field unset. The stop kind is `token_budget` either way, but the remedy differs: a
user who set a budget is told to raise it, and a user who never set one is told to set one.

One rule from the table above does not carry over. A provider that reports no usage stops a
turn under a budget you set, because a budget that cannot be counted cannot be honoured and
continuing on unreported numbers would quietly make it advisory. Under the default the turn
continues: an endpoint that withholds usage is its own choice and not a runaway, and ending
every such run on a limit nobody asked for leaves no remedy but to set one. The default still
binds on whatever was counted, so a floor that crosses it stops above.

Two further ceilings bound the turn rather than the goal. Both are off unless the host sets
them, and both apply whether or not a Goal is active, because a turn without a goal can loop
just as well as one with. A host that wants a bound no provider can withhold uses them.

| Ceiling | Stops the turn when |
| --- | --- |
| Tool calls | That many calls have been dispatched inside one turn |
| Wall time | The turn has run that long, at one-second resolution |

Tool calls are counted from the dispatch groups the loop actually ran, so a call that a stop
or an urgent human request kept from running is never counted. A reached ceiling overrides a
compaction request or a continue, because either would spend a request the ceiling no longer
allows, and it defers to a stop the Goal already produced. A request already in flight when
the clock passes a ceiling completes first.

A session with no goal is unaffected by the token default. There is no durable counter to
charge it against, and a default enforced from an in-memory turn total would reset every turn
and never bind.

### Human requests and autonomy

An active Goal is autonomous by default and does not receive the ordinary synchronous
`question` tool. When a missing fact or decision truly blocks the objective,
`goal_request_input` atomically creates a `human_request` row and pauses the exact Goal with
`human_input`. Permission waits use the same store with kind `permission`. The turn ends as
`WaitingForHuman`; it is not held open by a process-local receiver.

TUI, HTTP, and ACP list and answer the same rows. An answer transaction settles the request
and admits its model-visible response to the durable FIFO inbox together. Goal resumption is
a later idempotent step, so a crash after the answer commit cannot lose the response or
duplicate it. On restart, clients re-present pending requests from SQLite. Their in-process
channels only wake already-running consumers.

Ordinary non-Goal Work likewise does not receive the synchronous question tool. It uses
evidence-backed reversible defaults and continues. If an undiscoverable choice has no safe
default and materially changes the result, the Agent finishes the turn with one direct
question before performing the affected side effect. Plan retains structured question
forms for decision-complete planning.

A side-effecting tool whose response is lost has an uncertain outcome. The Goal pauses with
`uncertain_side_effect`, requires authoritative-state inspection, and never mechanically
replays that invocation. Only tools explicitly marked read-only or idempotent may use safe
retry behavior.

## Plan

A plan carries stages, their dependency order, and their acceptance status. It exists so
that progress visibility, interruption recovery, and verification survive a restart or a
context compaction.

Use one for normal research-modify-verify work. Keep one current for anything
cross-component, anything involving delegation, anything with multiple acceptance gates,
and anything likely to be interrupted. A direct answer, one bounded read, or a genuinely
atomic operation does not need a plan.

Rules that matter in practice:

- Step ids stay stable across revisions. An update carries the revision returned by the
  last read, and a stale revision is rejected without changing anything.
- While steps remain pending, exactly one step is in progress.
- Completed and superseded steps are terminal and cannot regress.
- A fully completed plan has no in-progress step.
- The host classifier decides whether a strategic Plan is required but never installs a
  generic skeleton. The model creates the first root with `action=create`; a substantial
  new objective uses `create` plus the current `expected_revision`, archiving the previous
  root instead of appending generic steps.
- `patch` sends only changed ids, `append` sends only new step definitions, and the host
  generates ids for `create`, `append`, and `push`.
- A focused temporary workflow uses `plan_update` with `action=push`; the parent is
  suspended durably and the child becomes the visible Plan. After every child step is
  terminal, `action=pop` carries only `expected_revision` and restores the exact parent
  once. Update the active Plan when work starts, completes, is superseded, or changes
  scope, and reconcile it before the final answer.
- Verification is scoped to the exact commit, build, tag, deployment, configuration, and
  inputs inspected. When any of them changes, append a new gate instead of reusing an older
  completed result.
- Task jobs are host-linked to the Plan step that admitted them. A step cannot complete while
  a linked job is queued, running, uncertain, or still has an unconsumed report. An explicit
  new Goal may supersede that step, but it does not settle or cancel the Job implicitly.

Plan mode enforces the read-only side of this below the prompt: a deny-by-default overlay
allows inspection, read-only search and LSP, questions, Skills, and typed
Goal/Plan/Todo operations while denying shell and file mutation. Returning to Work mode
requires a durable plan to exist, and the confirmation names its title, revision, and
completed-step count.

The default host owns classification and final reconciliation through typed planning
services; the model owns strategic step creation through operation-based `plan_update`.
Disabling that tool prevents new model mutations, while existing Plan persistence, client
projection, and restart recovery remain intact.

Entering Plan while a Goal is active atomically records `paused(plan_mode)`. Start Work
resumes only that exact pause and does so once, even after a process restart. It deliberately
does not clear pauses owned by authentication repair, a pending human request, permission,
manual interruption, or uncertain side effects.

```text
/plan
/start-plan
/start-work
```

## Todo

Todo items are the concrete work beneath a plan step. They carry stable ids, revisions,
goal and plan links, parent and dependency ids, owner, status, priority, timing, and token
usage.

The constraints exist to keep the graph meaningful:

- At most one item is in progress in a session.
- Every referenced parent, dependency, and plan step must exist after a batch.
- Parent and dependency graphs are acyclic.
- A whole batch rolls back on any validation or revision error; partial updates are never
  committed.

Preserve an existing id rather than deleting and recreating an item, and when one atomic
batch adds dependent items, assign explicit stable ids before referencing them.

## How this survives compaction

Compaction changes the provider transcript boundary. It does not delete Goal, Plan, Todo,
Job, inbox, event log, or prompt receipt state.

Instead, every relevant provider request regenerates a bounded `runtime.work_state`
developer section from SQLite: the current plan revision and steps, Todo identities and
dependencies, active or uncertain jobs, terminal jobs linked to an unfinished Plan step,
terminal jobs with an unconsumed report, pending report identities, and the latest prior
prompt receipt id. Job entries include their versioned Plan `workContext` when present. One
deferred transaction reads all of it from the same snapshot.

Each collection is capped at 64 entries and the rendered section at 16 KiB. Verbose text is
shortened UTF-8-safely before whole tail entries are omitted, omitted counts stay explicit,
and identity fields are retained. Assembly fails closed if even the identity fields cannot
fit, because a work-state section that silently lost an identity would be worse than none.

## Jobs

Background delegation produces durable jobs, and their lifecycle is part of this state.

A background job commits `queued` before waiting for delegation capacity and becomes
`running` only at admission. On restart, a still-queued job settles as `cancelled` because
its runner never started; a running job settles as `uncertain` and is never replayed.

Native Task admission records the optional Goal id, Plan id, admission revision, and active
Plan step id in the Job's `workContext`. Completed or failed child evidence remains
model-visible while that step is unfinished, including after compaction or restart. This
keeps a failed investigation from disappearing and being delegated again without a changed
hypothesis.

Do not complete a parent while active jobs or unconsumed reports remain. See
[Orchestration](/orchestration) for report delivery.

Durable background commands keep their authoritative process state and output
under `.zuno/background`, but their model-facing completion is a deterministic
session inbox row. The filesystem event is only a wake hint. Terminal state is
scanned on session activation and periodically while the process is resident,
so these crash points are recoverable without replaying the command:

- status persisted, completion input not yet admitted;
- input admitted, wake not yet attempted;
- input promoted, process lost before model-visible consumption.

The last case returns the original row to its admitted lane. Job-backed child,
product-agent, workflow, and council reports perform that transition before the
pending inbox scan, so the recovered report can wake an idle parent without a
new user prompt. A consumed, cancelled, or failed input is terminal and is
never synthesized again.

For a long-running CI watcher or release observer, start one background Shell
execution with `backgroundPurpose: "remoteObserver"` and let its durable terminal
report resume the session. Prose such as "the task is still running; I will wait"
does not create continuation state. The observer's process exit is only a wake
signal: use `bg output`, then re-query the remote workflow or release by stable
run/attempt, ref, or release id. Do not launch overlapping watchers or hand-written
poll loops, and do not treat an overall green run as proof that skipped, cancelled,
missing, or unexpanded required jobs executed.

## See also

- [Sessions and turns](/guide/sessions)
- [Orchestration](/orchestration)
- [Agents](/guide/agents)
- [Harness runtime](/harness-runtime)
