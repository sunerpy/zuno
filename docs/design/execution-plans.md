# Execution plans

Status: implemented foundation.

## Decision

Zuno separates ordinary execution progress from explicit planning mode.

- In the default execution path (the persisted Work collaboration mode), `orchestrator`,
  `build`, and `deep` use `plan_update` for non-simple work with several dependent steps.
- A simple, isolated task does not need a formal plan.
- The execution plan is an advisory checklist. It records what the Agent intends to do and
  what it has completed, but it does not drive the Agent loop.
- The explicit Plan collaboration mode selects the read-only `plan` Agent. That mode is a
  capability boundary and a user-controlled handoff, not the presence of a checklist.
- A durable Goal is the only one of these concepts that may keep scheduling turns after a
  turn ends. An unfinished execution plan never auto-continues a session.

This follows the useful Codex-style separation between an `update_plan` progress surface in
normal execution and an explicit Plan mode, without making Codex behavior or wire formats a
compatibility target.

## Three independent concepts

| Concept | Purpose | May edit product files | Controls continuation |
| --- | --- | --- | --- |
| Execution plan | Show and persist current multi-step progress | Depends on the selected Agent | No |
| Plan collaboration mode | Research and prepare an implementation-ready plan | No | No |
| Durable Goal | Own a long-running objective across turns and recovery | Depends on the selected Agent | Yes |

An execution plan may carry a `goal_id` to describe which Goal it supports. That association
does not transfer Goal lifecycle semantics to the plan.

## Default execution behavior

The delivery Agents apply the following policy:

1. If the request is one clear action, execute it directly.
2. If the request has several dependent implementation or verification steps, create a concise
   execution plan with `plan_update` before substantial work.
3. Update the full snapshot when evidence changes the approach, a step starts, or a step
   completes. Do not leave completed work marked active.
4. Keep the plan proportional to the task. It is a progress contract, not a second design
   document and not a transcript of every tool call.
5. Finish or accurately terminalize the checklist before reporting completion, but do not use
   an unfinished checklist as a reason to manufacture another turn.

`orchestrator` remains responsible for integration and may delegate bounded lanes. `build`
owns a direct lane without delegation. `deep` can own one difficult cross-cutting lane as
the selected session Agent or as a delegated target, always without recursive delegation.
All three share the same execution-plan semantics.

## Snapshot invariants

`plan_update` replaces the complete plan snapshot and uses optimistic revision checks. A
candidate snapshot is accepted only when all of these conditions hold:

- The plan title is not blank.
- Every step has a non-blank ID and title.
- Step IDs are unique within the snapshot.
- At most one step is `in_progress`.
- If any step is `pending`, exactly one step is `in_progress`.
- A fully completed plan may contain zero `in_progress` steps.
- Existing step IDs remain stable across revisions. An update may add a new ID, but it may not
  remove or rename an accepted ID. Step titles and ordering may be refined without changing
  identity.
- A `completed` step cannot return to `pending` or `in_progress`.

Snapshots with no pending work do not invent an active step. A fully completed plan therefore
has zero `in_progress` steps. Blocking and cancellation belong to typed WorkItem or Goal
lifecycle state; they are deliberately not valid execution-plan step states.

Cross-revision checks run inside the same database transaction that reads the current plan and
writes the next revision. Rejected transitions leave the durable revision unchanged.

## Example

Initial snapshot:

```json
{
  "title": "Implement session export",
  "steps": [
    { "id": "inspect", "title": "Inspect durable state", "status": "in_progress" },
    { "id": "implement", "title": "Implement archive writer", "status": "pending" },
    { "id": "verify", "title": "Run round-trip tests", "status": "pending" }
  ]
}
```

Later snapshot:

```json
{
  "expected_revision": 1,
  "title": "Implement session export",
  "steps": [
    { "id": "inspect", "title": "Inspect durable state", "status": "completed" },
    { "id": "implement", "title": "Implement archive writer", "status": "in_progress" },
    { "id": "verify", "title": "Run round-trip tests", "status": "pending" }
  ]
}
```

The IDs are stable, exactly one step is active while pending work remains, and completion is
monotonic.

## Explicit Plan mode

Entering Plan mode remains an explicit user-controlled collaboration transition. The `plan`
Agent may inspect the repository, ask questions, load Skills, and update typed Goal/Plan/Todo
state, but its deny-by-default capability overlay prevents shell and product-file mutation.
Creating an execution plan in ordinary Work mode does not enter Plan mode, and calling
`plan_update` cannot switch collaboration modes.

## Lifecycle and client behavior

The durable plan is a shared projection for TUI, server, ACP, and future clients. Clients may
render plan revisions and status changes, but they must not add a private execution loop or
wake a session merely because pending steps exist. Goal state, inbox state, retry deadlines,
and typed runtime failures remain the authorities for continuation and recovery.

## Non-goals

- Automatically creating a durable Goal when a plan is created.
- Automatically continuing until every plan step is completed.
- Requiring plans for simple questions or one-step edits.
- Letting the model enter or leave explicit Plan mode through `plan_update`.
- Treating plan prose or UI state as a substitute for durable session events.
