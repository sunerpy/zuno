# Resident memory and reviewed promotion

Resident Memory carries small global preferences and project rules across
sessions. It is distinct from the user learning flywheel:

- Memory is compact prompt-resident state;
- Experience is concrete durable evidence;
- a Skill candidate is a reusable method that requires review and evaluation.

For the commands and review workflow, start with
[Memory and learning](/guide/memory-learning). This page owns implementation
boundaries and recovery rationale.

See [user learning flywheel](user-learning-flywheel.md) for extraction,
retrieval, pattern mining, and Skill evolution.

## Ownership boundaries

- `ResidentMemoryStore` owns SQLite documents, revision history, and atomic
  candidate settlement.
- `MemoryStore` validates capped entry operations and publishes guarded file
  projections.
- `MemoryService` owns validation, durable candidates, promotion, apply, undo,
  and restart reconciliation.
- `memory_propose` is the only model-visible mutation entry point.
- `WorkStateProjection` exposes current candidates and resident entries to every
  client.

Neither foreground models nor the learning extractor receive direct write access
to resident files.

## Session policy

The global `memory` and `learning` configuration remains the capability ceiling.
Each durable session also freezes a revisioned policy:

- `use_memories` controls both resident `memory.global` / `memory.project`
  sections and automatic `learning.experiences` retrieval;
- `generation=enabled` permits explicit and automatic learning;
- `generation=disabled` stops new generation and skips queued automatic
  extraction while retaining existing Memory and Experience;
- `generation=excluded` is the fail-closed state used when configured external
  context makes the session ineligible. It cannot be changed back to enabled in
  the same session.

The policy lives in `session_memory_policy`, not opaque session metadata. New
sessions freeze the current configuration default in the same transaction that
materializes the session. Later changes use revision compare-and-set and append
`session.memory.policy.changed` in the same transaction.

`/memories` edits this policy for the current session. `/memory` remains the
candidate and resident-entry review surface. Disabling use changes subsequent
prompt assembly only; it never deletes resident files, Experience, or audit
records. The Server `PUT /api/session/{sessionID}/memory-policy` route goes
through a TurnHost-owned mutation while holding the session run lease; it cannot
persist an enabled value when the resolved Memory or extractor capability is
absent.

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

`resident_memory_document` is the prompt source of truth, with immutable
`resident_memory_revision` history. Existing files are adopted once without
discarding their entries. Candidate rows retain the approval and mutation audit.
Every new foreground turn selects a current immutable Memory snapshot and records
its source revision in the prompt receipt.

## Promotion

`memory.promotion` has three values:

| value | behavior |
| --- | --- |
| `review` | Keep every valid proposal pending until the user approves it. |
| `high_confidence` | Apply proposals at or above `auto_confidence`; retain the rest for review. |
| `automatic` | Apply every proposal that passes the same validation and safety checks. |

The default is `review`; `auto_confidence` defaults to `0.9`.

Learning extraction deliberately uses a narrower automatic path: only
project-scoped Memory with confidence at or above `0.9`, validated source citations,
and authoritative successful execution evidence can auto-apply. Global
and lower-confidence learning proposals remain pending regardless of the general
Memory promotion setting.

## At-most-once apply and undo

Application compares the expected resident revision, writes the new document and
history, and records the candidate as `applied` in one SQLite transaction. Undo
uses the same boundary and records `undone`. A candidate-state conflict rolls
back the document update too.

File projection happens after the authority commit. Its recorded revision and
error distinguish accepted entries from a pending projection. A projector holds
a stable OS file lock while checking and replacing the path, and a stale
projector cannot mark a newer document as projected. Missing derived files can
be restored without repeating the logical mutation. Different external contents
remain untouched.

For historical `applying`/`undoing` rows from earlier releases, startup still
compares the file with the snapshots:

- apply: after means `applied`, before means `failed`;
- undo: before means `undone`, after means still `applied`;
- any third state means `uncertain`.

No historical uncertain operation is replayed. External drift is preserved and
surfaced for manual reconciliation. If no authoritative document exists yet, an
unresolved historical write is quarantined rather than adopted. Explicit
`/learn inspect-memory` and `/learn import-memory` expose and resolve that state.
Imports keep immutable revision history and apply the same entry validation.
No-op entries and unchanged reconciliation do not advance document versions or
emit spurious change notifications.

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
are rejected. Post-task extraction belongs to the default-enabled `learning`
subsystem. An explicit `learning.extractor_model` wins; otherwise the active
provider's `small_model` and then the session model are used.


## Auxiliary learning ownership

`LearningSupervisor` belongs to `StartupEnvironment`, with one replaceable binding
per project and a process-wide concurrency limit. Closing a foreground host does
not own its cancellation. `ProjectLearningService` owns job execution;
`LearningIngestion` owns bounded source collection and startup catch-up;
`LearningModelClient` owns provider budgets, structured decoding and exact request
receipts. The CLI adapter supplies active-session and change-notification handles.

Every claim has a fresh token. Heartbeats recheck durable policy and input activity;
Memory commits and semantic pattern proposals check that token inside their write
transaction. Process shutdown propagates cancellation and bounds joins. Explicit
command interruption drops the request and settles manual reflection/evaluation
state; read-only model work can be retried with persisted capped, jittered backoff.

Codex supplies design references for staged memory extraction, consolidation,
source attribution and ownership fencing. Zuno's reviewed Skill-candidate and
cassette-evaluation pipeline is a native Zuno capability, not an imported Codex
Skill-evaluation feature.
