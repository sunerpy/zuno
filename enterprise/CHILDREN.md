# Durable native child dispatch

The enterprise child path reuses native `agent_job`, `ChildTurnHost`, completion
envelopes and the shared driver's typed wait/consumption boundary. It does not
create an independent Agent loop.

`ChildTurnHost::dispatch` returns either a ready native result or
`ChildTurnDispatch::Pending(WaitRef)`. `TaskTool::dispatch` validates the native
contract, roster, model routing, depth and permission before forwarding that
outcome. A foreground `running` result is rejected. Ordinary local task execution
still returns its completed result; remote composition uses `ChildToolDispatcher`
to forward pending work directly to the shared driver.

## Atomic admission and delivery

A `ChildDefinitionCatalog` belongs to the control plane. It resolves the exact
parent/child configuration, model selection and delegation limits. Wire intent
contains the stable invocation, arguments digest, logical key and bounded native
task data. Workload identity and the current parent Job grant authenticate the
internal request; callers cannot grant themselves a different scope or limit.

Foreground dispatch first stores an idempotent intent. It creates no executable
child until the parent commits its waiting checkpoint. That transaction creates
the child session, inherits its session Memory restrictions, admits its native Job
and input, registers the wait and releases the parent Worker slot. Audit failure
rolls the entire boundary back. Staged intents left behind by a parent boundary
are retired without reserving their logical key forever.
One partial unique constraint also reserves an existing child session for only
one staged/active continuation, so competing resumptions fail at admission.

Background dispatch admits the child immediately and returns a distinct Job and
session handle. `nextStep` uses a durable outbox; `quiet` retains the result
without creating a parent input. A cancelled or paused parent is not automatically
continued by completion.

The child commits its terminal native Job, actual assistant text, typed result,
completion envelope and delivery marker together. It does not hold the parent
session lock while settling. Publication later acquires the parent lock, closes
the completion-before-wait race and may admit a next-turn report through the same
root admission code as user requests. Notifications may repeat; completion
consumption, tool-part write and checkpoint progression are atomic and deduplicated.

The model receives the resumable child session and Job IDs with the terminal
result. Reasoning parts are excluded. Input materialization verifies a delegation
against its owning child relation and an automatic report against its stored
completion envelope. Their origins remain durable and are not recast as user
statements for Memory evidence.

## Storage and verification

Preview PostgreSQL format 10 separates the native Job's parent from its executing
session, adds owner-forced RLS on `runtime_child`, and preserves formats 1–9.
The frozen format-9 fixture includes runtime and Memory values and tests rollback
before the marker update.

The real PostgreSQL suite checks staged admission, duplicate/conflicting calls,
atomic wait rollback, separate execution slots, Worker replacement, duplicate
completion and consumption, quiet/next-step delivery and stopped parents. The
authenticated HTTPS fixture runs the native `TaskTool` through remote Workers:
parent waiting, child execution, replacement parent, one original tool result and
no duplicate provider request.

The handler is mounted by `WorkerStateService::with_children` only when a real
catalog is supplied. Standalone definitions with validated `delegation.targets`
now install this catalog and the native task dispatcher. Child model and gateway
selection are fixed by the referenced definitions, and workspace preparation gates
execution. Job/tree cancellation is described in [control](CONTROL.md). Approved merge, cross-gateway transfer and Workflow/Council
remain P4 work. See [workspace configuration](WORKSPACES.md).

See [中文](CHILDREN.zh.md), [waiting](WAITING.md), [PostgreSQL](POSTGRES.md)
and [implementation status](STATUS.md).
