# zuno acp

`zuno acp` speaks Agent Client Protocol over stdin and stdout. Editors that support ACP
launch the executable as a child process and exchange framed messages on the pipes, so
there is no port to bind and no HTTP surface to secure. This is the integration path for
Zed and other ACP clients.

Because the protocol owns stdout, do not read that stream as human output. Use `--check`
when you only want to confirm the adapter is present, and `--print-logs` to route
diagnostics to stderr where they will not corrupt the protocol stream.

The editor launches one process and keeps it. That process is the one serving the
protocol, so terminating it ends the session and its pipes reach end of file — see
[One invocation, one process](/cli/#one-invocation-one-process).

Opening Zuno in an Agent Panel calls ACP `session/new`, which reserves an
in-process session id and resolves configuration, commands, Skills, and MCP. An
ordinary unused panel remains ephemeral. A durable native collaboration control,
such as entering Plan mode, materializes the Session because its mode, Goal pause,
and future Work identity must survive restart.

## Agent, Mode, Plan, and file projection

Mode and Agent are independent durable choices. **Mode** owns the Plan/Work
boundary. The Agent selector contains only implementation Agents; while Plan is
active it updates the saved Work Agent without leaving the read-only `plan`
host. Model and reasoning changes update the same future Work identity.

`/plan` and `/start-plan` enter Plan idempotently. `/start-work` and
`session/set_mode(build)` invoke the same atomic authorization: the exact Plan
revision must be handoff-ready, a bound review must be Ready unless the user
supplies `--accept-draft-risk <reason>`, and the saved Agent/provider/model/
reasoning identity is restored. Idle sessions start immediately; busy sessions
queue the durable control for the next safe point.

Idle Agent, model, Mode, and reasoning changes atomically replace the turn host.
The session keeps its connected MCP runtime when the resolved MCP server set and
connection concurrency are unchanged, avoiding an unnecessary network or
subprocess handshake. Structural MCP changes still reconnect. Reconfiguration
logs include phase timings but omit selected values and credentials.

A `session/prompt` that arrives while the session is already running is committed
to the durable inbox, then steered at a safe point or left queued. Its RPC waits
for that input's associated processing outcome; a normal completion returns a
legal `stopReason`. Accepted content is no longer reported as a `-32001` busy error.
Session-owned execution outlives an individual RPC observer. Responses include
`_meta.zuno.receipt`, separating admitted, recorded, applied and terminal state.

Normal prompts may set `_meta.zuno.messageId` (1–256 bytes). Retrying the same ID
in the same session observes the original receipt, not a second execution;
conflicting content is rejected. Identical text with different IDs remains
different user input.

Clients that support Zuno extensions can use `session/steer` instead. The
initialize response advertises `_meta.zuno.steering`; turn-scoped
`session/update` values carry `_meta.zuno.turnId`. A steer supplies that id as
`expectedTurnId` and receives an immediate success result with `turnId`,
`inputId`, `admittedSequence`, `admission: "steered"`, and `delivery: "steer"`.
Rejections use `-32002` with `reason` equal to `noActiveTurn`,
`expectedTurnMismatch`, `activeTurnNotSteerable`, or `emptyInput`.
The expected turn is checked before the inbox transaction commits. If that turn
ends or changes while admission waits for SQLite, the input and its admission
event roll back together, so a rejected steer cannot reach a later turn.
Stop uses `session/cancel`; follow-up input uses prompt admission or
`session/steer`, never cancellation followed by resubmission.

A slash command cannot be steered and is refused with
`reason: "commandRequiresIdleSession"` and nothing durable written; only text
that resolves to a real command, Skill, or native control counts as a slash
command, so a prompt that merely starts with `/` is admitted as ordinary content.

Withdrawing a pending prompt with `$/cancel_request` retires only the input
contributed by that request. If its selected native input is already running,
cancellation checks that input's identity at the signal boundary. It cannot erase
processed history or withdraw the original on behalf of a duplicate retry observer.
Disconnect is not withdrawal: accepted input and processing receipts remain durable.
See [Zed ACP integration](/reference/zed-acp) for the full shape.

Background completion is different from a slash command. Terminal commands,
subagents, workflows, and product Agents publish a deterministic completion
envelope and wake the parent automatically. Synchronous `bg wait` is capped at
60 seconds and competes with callback delivery for one durable owner, preventing
duplicate turns.

Plan projection is driven by durable work-state revisions, not by recognizing a
`plan_update` tool call. Each session reads and publishes the authoritative
complete Plan after a change, deduplicates by `(plan_id, revision)`, flushes the
final revision before prompt completion, and emits empty entries when the Plan
is removed. Load, resume, detached Goal continuation, and host remount share the
same projector.

`edit`, `write`, and `apply_patch` use edit cards whose standard `title` names
the files, for example `Editing main.rs, lib.rs`. Complete native arguments
update the title before dispatch; partial JSON keeps the `Editing files` fallback.
Titles show up to three filenames and 160 Unicode characters, with `(+N more)`
for additional files. Standard `locations` retain absolute paths, while relative
patch targets can name a card without guessing the session directory. Patch
targets use the native parser; hunk text is never scanned as a filename.
Resolved mutation results take precedence over requested path aliases.
Permission cards and history replay also retain filenames; replay still filters
unopenable/out-of-worktree paths. These fields do not require Zuno-specific `_meta`.

A successful
typed mutation shows only its structured add/modify/delete diff in visible
content while preserving the complete original result in `rawOutput`.
Pre-write failures show actionable text without a fabricated diff. Partial or
otherwise uncertain mutations remain failed, preserve observed paths or diffs,
and carry `_meta.zuno.outcome: "uncertain"`. Live delivery and replay use the
same policy.

If a provider retry fails after input admission, the failed processing receipt
retains the last captured HTTP status, provider code, request ID and redacted
reason in its error text. `admission: accepted` still means the input was saved;
it does not mean the provider completed it. Retry configuration is taken from
the selected provider's complete resolved model on the main ACP path as well as
internal requests. Missing upstream facts remain unknown, and old receipts are
not backfilled.

Operational notices — a remote rule file that could not be fetched or an intact local
rule file skipped because it did not fit the prompt budget (its rules are not in force
while the turn proceeds), a turn stopped by its
token, tool-call, or wall-clock allowance, or a compaction requested by the budget or
context policy —
are projected as `agent_thought_chunk` updates tagged `_meta.zuno.notice` with
`severity` (`info`, `warning`, or `error`) and a stable `code` such as
`instruction.not_in_force`, `budget.compact`, or `budget.token_budget`.
A proactive threshold crossing during a long turn uses the separate
`context.compact` code. The tag is how a client distinguishes them from model
output; they are never part of the transcript the model sees.
Internal replay diagnostics are logged with diagnostic audience and are not
projected as thought chunks.

After compaction succeeds, ACP receives the exact persisted summary as an
`agent_message_chunk` tagged `_meta.zuno.kind: "compaction_summary"`. Automatic
mid-turn compaction publishes it before retrying inside the same host drive, so
the editor does not first receive a terminal prompt failure or wait for another
wake. Historical load/resume projects the same durable summary with the same
tag.

ACP `usage_update` consumes the native `ContextUsageSnapshot`: the latest
provider-confirmed baseline plus estimated content not yet counted by that provider.
A smaller request estimate cannot replace a confirmed baseline. Partial usage
frames merge as snapshots; compaction changes the epoch. `_meta.zuno.contextUsage`
carries source, request identity, freshness and update time. Cumulative disjoint
usage is separate from current occupancy; unknown values remain unknown.

## Exact cancellation and legacy clients

Initialize advertises `_meta.zuno.cancellation` with `version: 1`,
`method: "session/cancel"`, `expectedTurnIdPath: "_meta.zuno.expectedTurnId"`,
`legacySessionIdOnly: "currentTargetAtDispatch"` and `armsNextTurn: false`.
Use the turn ID from a live `session/update`'s
`params.update._meta.zuno.turnId`:

```json
{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "ses_example",
    "_meta": { "zuno": { "expectedTurnId": "turn_example" } }
  }
}
```

The expected ID must be a non-empty string of at most 256 bytes. Zuno validates
the target under the same native lock that fires cancellation. A delayed T1
cancel cannot interrupt T2. Inactive named targets, mismatched IDs and malformed
exact metadata do not fall back to cancelling the current turn. This is a
notification, so there is no JSON-RPC response; rejected notifications produce
stderr diagnostics. Observe the original prompt's durable receipt and updates
for the actual processing outcome.

A legacy notification containing only `sessionId` captures the live target
once when handled. Idle cancellation is a no-op; it never arms a future turn.
The protocol provides no evidence of network-stale intent: a session-only T1
cancel that arrives while T2 is live can cancel T2. Clients requiring an exact
target must send the extension.

`$/cancel_request` identifies the original client RPC by `requestId`, including
its string/number type. Reusing a wire ID after its response creates a new
internal request identity; an idempotent `_meta.zuno.messageId` retry still only
observes the original durable input. Withdrawal cannot cancel an unrelated
Agent-to-client RPC. `-32800` reports request withdrawal, not rollback of tool
effects; use the durable receipt to observe execution.

## Ordinary completion and question delivery

A genuine ordinary provider final ends the current cycle even when Plan/Todo
steps remain unfinished or runnable, unless a typed wait or protected gate takes
precedence. The Plan projection keeps their actual
status; an `end_turn` is not evidence that every step completed. Foreground tool
waits and real follow-up are handled before final reconciliation. Only the
current cycle's active owned Goal authorizes Goal continuation.

Work/Goal optional questions stay deferred. In ordinary Work, a required user
choice that blocks progress is one clear plain-text final question ending the
turn, without a synthetic persistent pause. Required Goal input and Plan approval
retain their typed controls and identity/revision checks. Older pending forms and
unfinished Plans alone neither authorize continuation nor prevent new input:
the runtime execution wait reference is the authority.

Ordinary deferred clarifications expose `_meta.zuno.autoDefer` with
`deadlineAt` and `state` (`armed`, `snoozed`, or `deferred`). After 120 seconds,
the native service defers the form without selecting an option, submitting an
answer, or stopping the Agent. A late reply is still accepted; the adapter only
rebases across the exact automatic-deferral revision, never another user's
answer or draft. Blocking questions, required input, Plan approval, and Goal
resume do not auto-resolve. No reply means no consent.

`QuestionView.delivery` is derived from input receipts, with phases
`WaitingAnswer` (`waiting_answer`), `AnsweredPendingDelivery`
(`answered_pending_delivery`) and `Applied` (`applied`). Question lists may include
closed forms whose associated inputs still await application; display those as
delivery status without reopening the form. The pending/applied counts and last
input receipt distinguish answer acceptance from model application. Neither a
closed form nor an applied input proves a successful provider response.

Settled controls that create no model input, such as Keep paused, have
`delivery: None` (omitted in serialization), not `WaitingAnswer`; do not reopen
them. Early Plan approval still awaiting handoff remains
`AnsweredPendingDelivery`, even before its control input is admitted.

Failure settlement uses the frozen cycle/turn/Goal scope. Exhausting bounded
provider retries or their recovery deadline closes only the failed ordinary
cycle; it cannot block a later independent input or mutate an unrelated Goal.
Recoverable failures schedule backoff only for the exact active owned Goal.
Typed approval, required-input, authentication, turn-budget, uncertainty and
permanent-block gates remain effective.

## Saved input and execution gates

An input can be consumed into history while native execution is still gated.
Its `InputAdmissionReceipt` remains `recorded`, with an optional `executionGate`;
`appliedAt`, `completedAt`, and `turnId` are absent. The gate does not turn that
saved, unapplied input into a `failed` receipt or prove that sampling started.

Before a resumed provider request, the engine scans the durable FIFO for eligible
answers, reports and steering even without a live wake hint. Resume controls do
not skip earlier eligible answers; explicitly queued next-turn prompts remain
queued. `InputDeliveryBatch` and `session.input.delivery_batch.1` record actual
consumption, not application. The input receipt at post-hook provider dispatch
establishes application, and the eventual execution outcome establishes completion.

For a saved input that is execution-gated, `session/prompt` returns JSON-RPC
error `-32005`. Its `error.data`
contains `admission: "accepted"`, `reason: "executionGated"`,
`recoveryRequired: true`, and the authoritative `receipt`. The message is
already saved: do not resend it as a new input. Reconnecting and retrying the
same `_meta.zuno.messageId` observes the original receipt.

`executionGate` contains:

| Field | Meaning |
| --- | --- |
| `reason` | `user`, `authentication`, `turn_budget`, `uncertain_side_effect`, `blocked`, `waiting_human`, `waiting_external`, `no_progress`, `no_executable_work`, or `execution_unavailable` |
| `recovery` | `resume_work`, `resume_goal`, `start_work`, `resolve_human_request`, `wait_for_event`, `reauthenticate`, `inspect_outcome`, `review_budget`, or `inspect_session` |
| `executionRevision`, `cycleId` | The native execution revision and cycle at the gate decision |
| `requestId`, `sourceId` | Optional identity of the human request or external source being awaited |

Recovery values are hints, not permission or a promise that one command clears
the gate. `start_work` points to Plan authorization through Start Work, which
`/resume` cannot grant. Ordinary `/resume` must pass the existing Work, Plan,
Goal, wait, authentication, budget, blocked-state, and uncertain-outcome checks. Only then
does it bind the matching gated, unapplied anchor to a new cycle without
inserting the original text again. Until a real turn binds that input, duplicate
observers can still receive the same gate; application and completion then
advance through the normal receipt lifecycle. Existing `failed`, `cancelled`,
`applied`, and `completed` receipts are not reset.

When ordinary `/resume` is submitted through `session/prompt` and passes these
checks, the RPC observes the native resume control's durable receipt. Queuing
the control does not return `stopReason: "end_turn"`: the request stays pending
until its associated native execution completes, waits for human input, is
cancelled, or fails. Clients can keep the panel busy and retain Stop while the
resumed work runs. Losing the RPC observer does not cancel session-owned
execution; this applies to an individual observer, not closure of the ACP
connection, runtime, or process. Connection EOF allows up to 25 ms to drain ready
requests before runtime shutdown cancels a running resume control. Its data and
`cancelled` receipt remain durable; `session/load` does not replay that control.
Explicit `$/cancel_request` withdraws only that request's control;
`session/cancel` still follows the exact-turn or legacy target rules above.

This recovery does not change ordinary Stop: the next new message can run
normally. An interrupted Goal still requires its explicit Goal recovery control,
and old-cycle callbacks cannot revive stopped work.

Reconnect, ordinary input and upgrades do not automatically unlock legacy pauses
or false blocks. Existing failed bridges remain failed, and unknown provenance
remains gated. Ordinary `/resume` retains the checks above. Database format 15 is
unchanged; no new migration or automatic legacy rewrite implements this behavior.

### Offline repair of one proven false block

The standalone command targets the configured existing database. Use exact
session/input IDs and replace `N` with the positive `expectedRevision` from the
read-only inspection:

```sh
zuno session repair SESSION --input INPUT --dry-run
zuno session repair SESSION --input INPUT --apply --expected-revision N
```

Inspection is the default. `--apply` conflicts with `--dry-run` and requires
`--expected-revision`; that revision is the execution revision, not a question or
input revision. Before apply, close every ACP/TUI/server and other process holding
the database, including idle connections. Apply requires SQLite exclusive access
and the native recovery lease, then rechecks the complete proof atomically.

Only an exact, provably false-blocked real user input that is already `consumed`,
has a `recorded` receipt and was never applied/bound to a provider turn is eligible.
Ambiguous or missing native evidence, stale revisions, competing work, real
protected gates and uncertain effects are refused. Old `failed`, `cancelled`,
`applied` or `completed` receipts are not reopened.

Do not treat every `blocked` receipt as the same defect. A Goal paused with
`uncertain_side_effect` may represent an MCP mutation that timed out after it
changed remote state. New gate diagnostics report `inspect_outcome` and the
Goal's identity when that typed legacy pause is available; no safety gate is
cleared. `/inspect-outcome` lists recorded obligations, or explicitly reports
`legacyUncertaintyWithoutCallRecords` when old records cannot identify them.
The native file inspector does not verify Penpot, browser or other remote state.
Neither restart nor `session repair` substitutes for that inspection.

New user messages may receive text-only discussion while an uncertainty gate is
retained. Zuno emits `discussion.tools_disabled`; all tool execution is disabled
in the engine, including calls invented by the model or introduced by hooks.
This does not resume the Goal. On reopen, the latest saved, never-applied user
question can be claimed once through the same native path; do not resend it.
Other authentication, approval and budget gates are not bypassed.

Apply queues one audited recovery control and preserves the original consumed
input and its visible gate until real execution binds it. It does not requeue or
reinsert that input, replay tools, change a Goal, create a database or migrate
format 15. Success returns `control_queued`, which proves control admission rather
than provider application or success. A repeat returns `already_queued` only while
the exact control is still `queued` and the complete evidence and execution
snapshot remain unchanged; it performs no new write or admission. Once the control
advances or the original input is bound/applied, repair rejects resubmission.
Follow the later native control/input receipts.
The repair command itself does not start a provider request.

## Goal continuation

`/goal <objective>` is a native control followed by autonomous execution. Zuno
persists the typed command result, then immediately advances the active Goal
through the shared driver. On a fresh session the objective is admitted through
the durable inbox as the first user turn; the literal slash command is never
sent to the provider.

An automatic Goal turn uses the Agent and model selected on the current ACP
host. The newest real user message remains only its causal transcript anchor and
grants no authority. Reconfiguring from `deep` to `orchestrator`, or selecting
another model, does not require another prompt and does not rewrite that
message. The exact trigger, Goal revision, anchor, Agent, provider, and model are
persisted in `session.turn.started.1`.

`session/load` and `session/resume` rebuild the session runtime and automatically
resume an active root Goal. No extra prompt is required, including for a session
written by 0.6.0 that contains an active Goal but no user message, or for a
compacted session whose retained tail starts with an assistant message.

`/goal budget <positive tokens|none>` changes one Goal's explicit ceiling.
`goal_update` accepts `in_progress` and `active` only as idempotent confirmation
of an already-active Goal; a paused or blocked Goal still requires the user-owned
`/goal resume` control or the native **Resume goal / Keep paused** choice.
Interrupt keeps the Goal paused. A new ordinary prompt, skipped choice or recorded
background report never resumes it. The resume transaction validates Goal ID and
revision and preserves Plan, approval, authentication, uncertainty and budget
gates. Already processed user input is not submitted again.

## Session MCP servers

Zuno advertises standard ACP MCP support for stdio and Streamable HTTP;
legacy SSE remains unsupported. `session/new`, `session/load`, and
`session/resume` must provide the complete `mcpServers` list for that session.
Load and resume never reuse process resources from an earlier request.
Configuration changes inside one active session do reuse its connected MCP
runtime when the effective MCP configuration is unchanged.

Declarations are validated before the session is published:

- names must match `[A-Za-z0-9_-]{1,32}` or are normalized to a stable slug
  with an eight-character digest; duplicate normalized names are rejected;
- a stdio command must be absolute and runs with the session directory as cwd;
- HTTP endpoints must be absolute HTTP(S) URLs;
- environment and header entries are strictly validated, including
  case-insensitive duplicate header names.

Every ACP session owns an isolated profile bundle. All requested servers must
connect and complete tool discovery before any of their tools are published.
Partial startup is shut down in reverse order. Session close, load failure,
process exit, and profile replacement use the same exact disposer path.

Client MCP commands, environment values, and HTTP headers remain process-local:
they are not written to the session database or diagnostics. Tool schemas and
actual tool attempts continue through the ordinary durable tool rules.

## Synopsis

```sh
zuno acp [OPTIONS]
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--check` | Validate that the production ACP adapter is available, then exit | |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | platform-dependent |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Confirm the production ACP adapter is available in this build, then exit.

```sh
zuno acp --check
```

Serve the protocol on stdin and stdout, the way an editor launches it.

```sh
zuno acp
```

Serve the protocol while mirroring diagnostics to stderr, so protocol framing on stdout
stays intact.

```sh
zuno acp --print-logs --log-level DEBUG
```

## See also

- [Global options](/cli/global-options)
- [zuno serve](/cli/serve)
- [Zed ACP integration](/reference/zed-acp)
- [Logging](/logging)
