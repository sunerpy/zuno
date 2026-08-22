# Auditable memory and reflection

Zuno can carry small global preferences and project rules across sessions, but
learning is not permission to mutate the harness. Every proposed change enters a
durable review workflow before it becomes resident prompt content.

## Ownership boundaries

- `MemoryStore` owns the two capped resident files and exact snapshot replacement.
- `MemoryService` owns validation, durable candidates, promotion, apply, undo, and
  restart reconciliation.
- `memory_propose` is the only model-visible mutation entry point.
- `ProviderReflectionRunner` owns the isolated small-model request after a
  delivered turn.
- `WorkStateProjection` is the client-neutral view consumed by the TUI and future
  server, ACP, and GUI surfaces.

Foreground turns and reflection use the same proposal tool. Reflection receives
only an owned durable transcript and one whitelisted tool; it cannot invoke shell,
edit files, delegate, ask questions, or change foreground state.

## Candidate record

A `MemoryCandidate` records:

- global or project scope;
- add, replace, or remove;
- proposed content and unique locator;
- reason and confidence in basis points;
- reflection, foreground tool, or user source;
- source session and message;
- status, timestamps, and diagnostic;
- exact before and after entry snapshots once application starts.

The resident files remain the prompt source of truth. Candidate rows are the audit
and recovery record, not a second memory store.

## Promotion

`memory.promotion` has three values:

| value | behavior |
| --- | --- |
| `review` | Keep every valid proposal pending until the user approves it. |
| `high_confidence` | Apply proposals at or above `auto_confidence`; retain the rest for review. |
| `automatic` | Apply every proposal that passes the same validation and safety checks. |

The default is `review`; `auto_confidence` defaults to `0.9`.

## At-most-once apply and undo

Application persists `applying` together with both snapshots before replacing the
resident file. Undo persists `undoing` before replacing the applied snapshot with
the prior one. A successful file operation then records `applied` or `undone`.

If the process or filesystem response is lost, startup compares current entries
with the snapshots:

- apply: after means `applied`, before means `failed`;
- undo: before means `undone`, after means still `applied`;
- any third state means `uncertain`.

No branch replays the file mutation. External drift is preserved and surfaced for
manual reconciliation.

## Reflection

Reflection runs only after a final response is delivered. Periodic review uses
`nudge_interval`; a verified non-trivial recovery may trigger review earlier.
The model must be an explicitly configured reachable `small_model`. If it is
absent or unavailable, Zuno reports that reflection is disabled and does not fall
back to the session model.

Before the request, Zuno persists `memory.reflection.request` with the exact
prompt, digest, replayed transcript, model identity, compaction mode, and tool
schema. Completion or every terminal failure writes
`memory.reflection.outcome`. Truncated streams, malformed tool JSON, denied tool
names, and proposal failures therefore remain inspectable.

## Safety and user control

Validation rejects malformed operations, ambiguous locators, over-budget results,
prompt injection, known credential literals, unreadable files, and external
drift. Guidance excludes temporary environment failures, unresolved guesses,
negative claims about tools, task narration, and secrets.

Learning may retain durable user preferences, explicit corrections, repository
rules, and verified reusable recovery knowledge. It cannot automatically edit
source code, configuration, prompts, agents, extensions, or skills.

`/memory` shows candidates and current entries. Users can inspect provenance,
approve, edit and approve, reject, undo, or remove an entry. Remove also creates
and applies an audited candidate rather than bypassing the workflow.
