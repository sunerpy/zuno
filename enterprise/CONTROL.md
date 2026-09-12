# Durable job cancellation

`RuntimeControl` separates authenticated user intent from Worker execution
authority. PostgreSQL implements the port in the data owner; API and BFF callers
use the same service. A browser or ACP client never supplies a Worker lease.

`POST /api/v1/jobs/{job}/cancel` (or `/app/api/v1/...`) accepts:

```json
{
  "requestId": "stop-investigation",
  "expectedTurnId": "turn-from-the-job",
  "reason": "This investigation is no longer needed"
}
```

The service checks current organization membership/application policy, ownership
and the exact turn. It atomically records the stop intent, fences every active
Job in the existing child tree, retires unadmitted child intents, settles logical
child results and queues external operation cancellation. Session locks follow
the parent-to-child direction used by admission. New children cannot appear
behind the traversal; unrelated sessions remain available.

The receipt distinguishes `stoppedJobs` from `pendingOperations`. The first is
logical execution authority; the second needs gateway inspection. An identical
request retrieves the original receipt. Reusing its ID for another reason,
application or target conflicts. `GET /jobs/{job}` reports `stopRequested` and
the currently unresolved operation IDs, including cancelled descendants.

Completed results remain completed. Existing uncertain outcomes remain uncertain
and continue holding their conservative session boundary. Cancelling a completed
parent suppresses a delayed `nextStep` report and fences any continuation Job
that the report has already admitted. Explicit user inputs retain separate identity.
Completion facts are still retained. A user may explicitly submit a new task
after cancellation; no old Worker may commit more state or gain a new ticket.

## Gateway delivery

The gateway uses its own authenticated service identity to poll the data-owner
cancellation outbox. Polling returns only that gateway's immutable admitted
operations. Owner RLS remains in force when operation bodies are read. Poll
timestamps rotate unresolved entries so a lost environment cannot starve later
work. Ordinary Worker tickets do not expose an administrative stop endpoint.

The gateway compares operation identity and arguments against its local ledger,
then records the stop before requesting Docker termination. If admission
committed before the ledger insert, a durable never-startable record prevents a
delayed submit from starting the command. Stopping does not require the original
Worker lease or user membership to remain active.

A completed command wins over a later stop request: its real completion receipt
is kept. Lost stop responses are resolved by inspecting the original container.
Missing or uninspectable state remains `Uncertain`; no side effect is replayed.
The existing completion delivery/acknowledgement path persists actual terminal
receipts and removes them from pending cancellation. Repeated delivery and gateway
restart do not erase or duplicate those facts.

## Verification and scope

The native suites cover running/queued descendants, retired staging, exact-turn
checks, duplicate request IDs, other-user rejection, old-lease refusal, delayed
child notification, preserved completed results, stopped commands after Worker
revocation/gateway restart, and cancellation before local operation admission.
PostgreSQL format 12 adds owner-scoped control receipts, stop records, explicit
automatic-continuation ancestry and a gateway delivery cursor. The frozen format-11 fixture verifies data preservation
and rollback before the format marker advances.

This is logical Job/child-tree cancellation. Workflow/Council coordinator controls,
operator-wide drain, automatic permission-revocation policy and approved workspace
merge use their respective later adapters. No unimplemented control is registered.
Enterprise service roles remain Linux amd64/arm64; personal clients keep their
existing platform support.

See [application API](APPLICATION.md), [operation results](OPERATION_RESULTS.md),
[child jobs](CHILDREN.md) and [中文](CONTROL.zh.md).
