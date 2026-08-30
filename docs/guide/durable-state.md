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

Recovery is layered. Before every provider-request backoff, the request layer commits a
`provider_retry_backoff` checkpoint containing the request, turn, failed and next attempt,
typed reason, selected delay, and deadline. A restart never revives that old HTTP request.
It observes the remaining deadline, then starts a fresh Goal turn. If the bounded
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
| Authentication failures, user interruption, human input, permissions, or an uncertain side effect | Pause with a typed reason |
| Invalid provider protocol, unsupported typed input, unavailable agent or model, corrupt durable state | Block |

Inspect and manage it with `/goal` in the terminal application:

```text
/goal
/goal create <objective>
/goal edit <objective>
/goal pause
/goal resume
/goal block <reason>
/goal complete
/goal cancel
/goal history
```

The first token after `/goal` is always an action. To create a goal, use
`/goal create <objective>`; `/goal <objective>` treats the first word of the objective
as an action and is rejected. `/goal help` prints the compact command summary.

Goal status also shows the typed pause, cross-turn retry, provider backoff checkpoint,
and pending human requests. Completion is rejected while any Plan step, WorkItem, Job,
next-step report, or Goal-owned human request remains unfinished.

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
- A fully completed plan has no in-progress step.

Plan mode enforces the read-only side of this below the prompt: a deny-by-default overlay
allows inspection, read-only search and LSP, questions, Skills, and typed
Goal/Plan/Todo operations while denying shell and file mutation. Returning to Work mode
requires a durable plan to exist, and the confirmation names its title, revision, and
completed-step count.

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
dependencies, active or uncertain jobs, terminal jobs with an unconsumed report, pending
report identities, and the latest prior prompt receipt id. One deferred transaction reads
all of it from the same snapshot.

Each collection is capped at 64 entries and the rendered section at 16 KiB. Verbose text is
shortened UTF-8-safely before whole tail entries are omitted, omitted counts stay explicit,
and identity fields are retained. Assembly fails closed if even the identity fields cannot
fit, because a work-state section that silently lost an identity would be worse than none.

## Jobs

Background delegation produces durable jobs, and their lifecycle is part of this state.

A background job commits `queued` before waiting for delegation capacity and becomes
`running` only at admission. On restart, a still-queued job settles as `cancelled` because
its runner never started; a running job settles as `uncertain` and is never replayed.

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

## See also

- [Sessions and turns](/guide/sessions)
- [Orchestration](/orchestration)
- [Agents](/guide/agents)
- [Harness runtime](/harness-runtime)
