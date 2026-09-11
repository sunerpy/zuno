# Durable invocation waits

The bounded driver can suspend an unfinished tool phase using
`ToolDispatchOutcome::Pending(WaitRef)`. Ordinary dispatch still returns a
completed `ToolDispatchResult`. A wait does not write a successful `running`
message, finish the step, or start another provider request.

This is an internal runtime capability. Enterprise launch assembly, distributed
child/Workflow producers, human approval execution and public client projection
remain separate deliveries. No new CLI command or public HTTP route is enabled.

## Identities and execution

`WaitRef` binds a stable wait ID to the original turn, invocation, canonical
argument digest, target and continuation kind. Targets distinguish approval,
user input, child Job, external operation and database-clock timer. The bounded
driver consumes `current_turn` results; a `next_turn` reference cannot silently
become a current-turn continuation.

Checkpoint schema 3 retains the original provider-visible declarations,
orchestration snapshot, tool calls and positions, next unprepared call, pending
references, and accumulated result effects. It also retains the existing prompt,
usage, time and recovery state. No Future, provider object, cancellation handle,
lease credential or control-plane directory is serialized.

Preparation stops at the first durable wait in a dispatch batch. Earlier
independent calls still execute together. Later calls have not run their hooks
or requested permission yet; they are prepared after continuation under current
authority. Completed calls are not dispatched again.

The producer owns its durable prepared operation, actual argument approval,
post-processing and authoritative receipt verification. Merely returning a wait
reference does not supply those capabilities. An uncertain external operation
stays under inspection and cannot be published as a decided completion.

## Atomic transitions

1. The provider response and original pending calls are durable before tool
   preparation. A deferred call retains its original part and has no tool result.
2. The driver commits the wait markers and checkpoint together. PostgreSQL also
   registers the owner-scoped waits, updates the Job checkpoint/version, releases
   the execution lease and marks the Job `waiting` in that transaction.
3. An installed `WaitCompletionStore` publishes an immutable completion fact.
   SQLite and PostgreSQL derive the fact's event identity from ownership and
   logical wait coordinates. An identical notification returns the original
   event; changing its reference or result conflicts.
4. Registration checks already published completions. Publication wakes a
   waiting Job only after its dependencies are ready. A paused/cancelled parent
   retains the fact without being resumed. PostgreSQL due-timer scanning uses
   database time and bounded owner-scoped queries.
5. On the next claim, driver admission consumes completion facts, writes the
   original tool results in order, advances the checkpoint and releases the
   lease in one transaction. This transition performs no provider/tool effects
   and has no independently committed `started` marker.
6. A compatible Worker resumes the remaining tool phase from that checkpoint.
   Another model request can begin only after the original step is complete.

The completion fact, notification opportunity and durable consumption are
distinct. Client disconnection or a dropped success response does not acknowledge
consumption. The next claimant reconstructs the committed checkpoint from its Job.

## Recovery and authority

History repair recognizes only the exact set of unfinished parts protected by a
valid checkpoint. A new ordinary turn cannot rewrite them as interrupted results
or overtake the bounded turn. A wait-shaped metadata field alone is insufficient.

PostgreSQL may reclaim an expired claim when the latest driver event is exactly
the Job's committed checkpoint and every unfinished part is explained by it.
A newer started advance, changed reference or unexplained in-flight invocation
retains the conservative uncertain outcome. Stale leases cannot publish new turn
state; authoritative external completion sources may still record late results.

Every reclaimed turn rechecks organization authority. Argument approval does not
become a lease credential. The original wall-clock allowance includes waiting;
before remaining tools run, the driver checks that allowance again. Consumption
preserves provider usage, tool counts, ordered context refresh and recovery
obligations without spending another provider attempt.

`WaitCompletionStore` is a host service, not an API for models or arbitrary
clients. Its provider must share the turn store's transaction domain. The producer
must verify the actual operation, child or answer before publishing its result.
Human approval readiness is integrated with the PostgreSQL state owner and shared
driver. Public approval endpoints, production gateway dispatch, user-input
adapters and cancellation/resume APIs still require their own consumers.

## Approval readiness

`WaitOutcome::RecheckInvocation` carries no execution result or transferable
permission. It is valid only for an approval target; an approval target cannot
publish a `ToolResult`. Consumption retains the exact original pending part and
provider metadata, removes its wait marker and rewinds only the final undispatched
call. Earlier completed calls remain settled. The tool-call count advances only
when the resumed call actually returns a result.

The approval writer holds the same session lock as wait registration. Its answer,
request receipt, readiness fact and wakeup commit together. Registration checks
an already answered approval, preventing a lost wakeup. A paused parent retains
the fact under the ordinary wait rules. The next claimant consumes readiness at
a separate checkpoint boundary and then prepares the original call under current
authorization and budgets. Approval cannot revive a superseded lease.

Completion payload schema 2 stores the tagged outcome. Earlier preview result
facts remain readable and deduplicate against the same logical completion without
rewriting the stored event. An old fact that confused approval with a tool result
is refused. The driver checkpoint remains schema 3 and the Worker protocol remains
version 3; completion publication is a separate state-owner port.

## Storage and verification

SQLite reuses indexed immutable session events. Stable core format 14 is separate
from preview runtime overlay 1. PostgreSQL preview format 5 adds `runtime_wait`, forced owner RLS, timer/Job
indexes and a typed waiting Job phase. Current PostgreSQL format 7 also stores browser authentication, canonical context and input execution
receipts; formats 1–6 migrate forward atomically.
The format-4 fixture retains captured turn DDL, its original source digest,
messages, signed metadata, usage, Job budget and lease state.

The focused engine tests cover executor replacement, no premature result,
repeated completion and lost consumption response, atomic rollback, and elapsed
budget refusal before remaining tools. The real PostgreSQL contract covers
competing claims, independent sessions, early completion, timers, paused parents,
cross-owner denial, failed consumption followed by checkpoint takeover, and
failed/successful migration. The authenticated HTTPS contract carries the same
wait and consumption through the separately versioned Worker protocol (version 3).

These tests do not certify independently launched Workers, gateway-to-tool
assembly, distributed child/Council orchestration or a complete enterprise UI.
