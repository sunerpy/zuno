# Sessions and turns

A session is a durable unit of work backed by SQLite. A turn is one pass through the
agent loop within that session. Both are runtime concepts with persisted state, not
presentation details, which is why a Zuno session can be resumed, replayed, and inspected
after the process that created it is gone.

## The model

| Concept | What it is | Where it lives |
| --- | --- | --- |
| Session | One durable conversation and its work state | A `session` row plus its events |
| Turn | One user input driven to a terminal outcome | Turn-scoped runtime state plus durable events |
| Event | Anything model-visible: prompts, tool results, reports, retries | The session event log |
| Inbox | The FIFO queue of admitted-but-not-yet-promoted inputs | Durable inbox rows |
| Prompt receipt | The exact assembled prompt for one provider request | Durable receipt with section ids and a digest |

Child sessions are real sessions. Delegation creates one per child, with its own events,
usage, and lineage. `zuno session list` hides them by default; `--no-roots` includes them.

## Lazy materialization

An interactive new session is prepared without inserting a row. Its process-local
identity is stable across model, agent, MCP, and theme changes, but opening, browsing, or
leaving the welcome screen creates nothing durable. The first model-bound submission
inserts the session and its user message in one transaction.

`/new` in the terminal application selects another prepared session in the same
activation. It opens an empty conversation shell directly and does not bypass this
boundary.

## Durable inputs

Every model-visible external input is admitted to the event log and durable inbox in one
SQLite transaction *before* execution is attempted. The inbox, not an in-process channel,
is the source of truth across active turns, idle sessions, restarts, and competing
drivers.

Drivers promote inputs in FIFO order. Promotion is transactional and can target one input
identifier for a live soft interrupt. A malformed input records a session error and does
not strand later queue entries.

User prompts and subagent reports share the same protocol:

- An active parent receives a soft interrupt and promotes the report at the next
  tool-safe point.
- If the report misses the final safe point, the wake coordinator waits for the active
  lease to end, then starts another turn while the input is still pending.
- An idle parent is claimed and driven immediately.
- A restarted process recovers pending reports from the durable inbox.

This is why a background delegation with `reportDelivery: nextStep` cannot lose its
report to a polling race: the settle, the admission, and the wake are one transactional
sequence.

Product-agent, workflow, council, and durable background-command completions use
the same continuation rule. The previous assistant reply may already have ended
with a message such as “the background task is still running; I will wait.”
That prose is not execution state. When the task settles, Zuno admits a typed
durable input and starts another turn if the process still owns a runtime. TUI,
ACP, and the HTTP server observe that turn through their normal event path.
After a process restart, the same input is redriven when the session is
activated; the command or subagent itself is not replayed.

An explicit close is different from an ended turn. Ending a turn leaves the
session eligible for background continuation. Closing a session unregisters its
resident notification watcher, interrupts and joins any detached continuation,
then applies the surface's lifecycle policy to owned work. It does not promise
that a non-resident process will continue generating.

## Prompt provenance

Prompt assembly uses stable section identifiers, an exact source, ordered content, and a
content digest. The post-hook prompt is persisted before the provider request, so what
the model saw is a durable fact rather than a reconstruction.

```sh
zuno debug prompt --session ses_1a2b3c --step 2
zuno debug prompt --session ses_1a2b3c --step 2 --show-sensitive
```

`--show-sensitive` prints instruction, AGENTS, skill, and memory content verbatim. Treat
it as sensitive output.

## Compaction

When history approaches the model window, Zuno compacts older conversation rather than
failing the request.

```json
{
  "compaction": {
    "auto": true,
    "threshold_percent": 80,
    "tail_turns": 2,
    "reserved": 12000
  }
}
```

`threshold_percent` accepts `1..=100` and defaults to `80`, applied to the usable window
after the model's output allowance and the configured reserve are removed. `auto: false`
disables proactive compaction while leaving `/compact` available. A provider-confirmed
context-limit failure still uses the bounded recovery path, which is recovery from an
already failed request rather than the proactive threshold.

Compaction changes the provider transcript boundary. It does not delete the durable
Goal, Plan, Todo, Job, inbox, event log, or prompt receipts. Those are regenerated from
SQLite for the next relevant request, including a bounded `runtime.work_state` section
capped at 64 entries per collection and 16 KiB overall.

Historical image bytes are excluded from the compaction request. A stable label such as
`[Attached diagram.png (image/png)]` stands in, while the original durable file part is
untouched for authoritative replay.

## Optional continuity

Configuration examples and switching rules are documented in
[History and Notes continuity](/config/continuity).

When `continuity.history` is enabled, the model can inspect normalized evidence from this
session through successful-compaction windows. The tool never crosses a session boundary
and excludes reasoning, encrypted content, synthetic internal prompt text, and binary
attachment bytes.

When `continuity.notes` is enabled, logical working documents are stored by
`session_id + Agent`. They are part of session lifecycle rather than filesystem state:
session deletion cascades through them, prune counts and removes them, and session
export/import preserves both notes and their idempotency ledger. Sanitized exports redact
note identity and content and remove that ledger.

## Interruption

A hard interruption is session-scoped and linearizable across turn handoff. If the
previous run guard has dropped but an admitted follow-up has not yet acquired its guard,
the registry arms that next guard instead of discarding the interrupt: the turn starts
with its interrupt signal set, emits the terminal interruption event, and issues no
provider request.

After confirmation the interface keeps the stopping state visible and suppresses late
provider or tool output until a terminal event establishes the boundary. Durable
persistence still runs. A side effect that completed before cancellation remains an
observed result and is never mechanically replayed.

## Retry and recovery

Recoverable provider, network, stream, SQLite-contention, Agent step-limit, and eligible
tool failures persist an exponential-backoff retry before waiting, so a process restart
reconstructs the deadline from SQLite. A turn that spends its own token, tool-call, or
wall-clock allowance is not retried: it pauses the Goal with `turn_budget` and waits for a
person, because the next turn would spend the same allowance the same way. A Goal whose
own `token_budget` is what ran out ends as `budget_limited` instead.

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

Delays are positive, capped, jittered, and interruptible by user input. A valid peer
`Retry-After` is clamped to the configured ceiling and is never replaced by an earlier
local delay. Retry decisions come from typed errors, never rendered messages:
authentication failures and user interruption pause, while invalid protocol, corrupt
durable state, and permanent configuration failures block.

Durable storage follows the same typed split. SQLite contention met while a Goal's budget
is read or charged, or while the host writes Goal-owned state such as a Plan reconciliation
or a human request, persists a `database_busy` retry and the Goal stays active, because a
lock another writer holds is a condition that clears by itself. Any other database failure
while the budget is read or charged stops the turn with `usage_unknown` and pauses the Goal
so a person can inspect the database; durable state this build cannot read at all blocks
the Goal. The persisted `goal_retry` row is what a restarted process reads back, so a
restart neither loses nor shortens the wait.

Clamping also decides what happens when a peer asks for more time than the same-request
recovery has left. The provider layer retries one request for at most 180 seconds; a
`Retry-After` longer than what remains of that window is neither slept through nor replaced
by a shorter local backoff. The turn ends with the peer's error, and the Goal retry waits
the peer's value clamped to `max_delay_ms`.

## Continuing a session

```sh
zuno run --continue "now cap the page size at 100"
zuno run --session ses_1a2b3c "what changed?"
zuno run --session ses_1a2b3c --agent plan "what would a safe migration look like?"
```

Forking a session is not part of this binary, so exploring an alternative without
polluting the session you intend to keep means starting a fresh one: name neither
`--continue` nor `--session`, and give it a `--title`.

## Retention

The store grows. Listing, previewing, and cleanup are all `zuno session`:

```sh
zuno session list
zuno session list --no-roots --archived --format json
zuno session prune --older-than 90
zuno session prune --older-than 90 --archive
zuno session delete ses_1a2b3c
```

With neither `--archive` nor `--delete`, prune is an inert preview and its counts match
the delete that would follow. `--archive` sets a reversible marker; `--delete` is
irreversible and has no undo in this binary. Read
[Session retention](/session-retention) before running either at scale.

## See also

- [Goals, plans and todos](/guide/durable-state)
- [History and Notes continuity](/config/continuity)
- [Session retention](/session-retention)
- [zuno session](/cli/session)
- [Harness runtime](/harness-runtime)
