# Resident memory and reviewed promotion

Resident Memory carries small global preferences and project rules across
sessions. It is distinct from the user learning flywheel:

- Memory is compact prompt-resident state;
- Experience is concrete durable evidence;
- a Skill candidate is a reusable method that requires review and evaluation.

See [user learning flywheel](user-learning-flywheel.md) for extraction,
retrieval, pattern mining, and Skill evolution.

## Ownership boundaries

- `MemoryStore` owns the capped global and project files and exact snapshot
  replacement.
- `MemoryService` owns validation, durable candidates, promotion, apply, undo,
  and restart reconciliation.
- `memory_propose` is the only model-visible mutation entry point.
- `WorkStateProjection` exposes current candidates and resident entries to every
  client.

Neither foreground models nor the learning extractor receive direct write access
to resident files.

## Candidate record

A `MemoryCandidate` records:

- global or project scope;
- add, replace, or remove;
- proposed content and unique locator;
- reason and confidence in basis points;
- user, foreground tool, or learning-extraction source;
- source session and message;
- status, timestamps, and diagnostic;
- exact before and after entry snapshots once application starts.

The resident files remain the prompt source of truth. Candidate rows are the
audit and recovery record, not a second memory store.

## Promotion

`memory.promotion` has three values:

| value | behavior |
| --- | --- |
| `review` | Keep every valid proposal pending until the user approves it. |
| `high_confidence` | Apply proposals at or above `auto_confidence`; retain the rest for review. |
| `automatic` | Apply every proposal that passes the same validation and safety checks. |

The default is `review`; `auto_confidence` defaults to `0.9`.

Learning extraction deliberately uses a narrower automatic path: only
project-scoped Memory with confidence at or above `0.9` can auto-apply. Global
and lower-confidence learning proposals remain pending regardless of the general
Memory promotion setting.

## At-most-once apply and undo

Application persists `applying` together with both snapshots before replacing
the resident file. Undo persists `undoing` before replacing the applied snapshot
with the prior one. A successful file operation then records `applied` or
`undone`.

After process loss, startup compares current entries with the snapshots:

- apply: after means `applied`, before means `failed`;
- undo: before means `undone`, after means still `applied`;
- any third state means `uncertain`.

No branch replays the file mutation. External drift is preserved and surfaced
for manual reconciliation.

## Safety and user control

Validation rejects malformed operations, ambiguous locators, over-budget
results, prompt injection, known credential literals, unreadable files, and
external drift. Guidance excludes temporary environment failures, unresolved
guesses, task narration, and secrets.

`/memory` shows candidates and current entries. Users can inspect provenance,
approve, edit and approve, reject, undo, or remove an entry. Removing an applied
entry is itself an audited candidate operation.

Deleting learning evidence never silently removes applied Memory. Zuno creates a
pending-review inverse candidate and retains the evidence needed to review it.

The retired `memory.reflection` and `memory.nudge_interval` configuration fields
are rejected. Post-task extraction now belongs to the explicit `learning`
subsystem and uses `learning.extractor_model`.
