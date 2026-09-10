# Automatic resident memory

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
- `MemoryMaintainer` owns bounded no-tools consolidation, separate from raw
  extraction and executable Skill evolution.
- `MemoryEvidenceStore` revalidates source manifests/bytes for writes and reads.
- `MemoryMaintenanceStore` commits changes, provenance, job completion and the
  no-op watermark together.
- `memory_update` is the only model-visible mutation entry point.
- `memory_read` exposes current bounded entries and revisions without arbitrary paths.
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
- base revision and content-addressed evidence references;
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

The default is `automatic`; explicitly configured `review` and `high_confidence`
remain effective. `auto_confidence` defaults to `0.9`.

Extraction never applies memory. It stores verified Experience and raw hints.
After successful extraction and during periodic maintenance, the project worker
admits a `project_aggregation` job with typed `purpose: memory`. This purpose is
independent of pattern aggregation's interval/evidence-count gates. Unknown purposes
fail closed. The no-tools model sees capped current memory, up to 64 recent verified
experiences, and explicit correction/forget/undo signals. It returns up to 32
typed operations. Empty output is a valid success; invalid semantics get one repair.

Claiming a memory job requires the worker's canonical project-memory path to match
its payload. Another worktree binding, or a worker without Memory enabled, leaves it
queued without spending an attempt; it cannot misclassify that job as stale.

Automatic updates require current successful tool evidence or verified user
corrections/preferences. Global changes additionally require explicit user evidence.
Unresolved and unverified observations cannot support automatic memory. Memory/read
search tool results cannot feed back as new proof. The semantic input digest includes
actual source content, not just cached fingerprints; use counts and promotion
bookkeeping do not trigger another consolidation.

The service validates exact managed-entry locators, budgets and threats before the
store atomically checks both scope revisions, the lease, generation policy and
every cited source. Only then can the batch, ordinary candidate rows, provenance,
job completion and maintenance watermark commit. CAS/source drift rejects the entire
plan. Projection happens afterward. No-op watermarks survive restarts.

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

Deleting learning evidence does not blindly reverse earlier writes. Zuno creates a
source-linked retraction only for entries with no independent current support.
Explicit source forgetting and retractions share a transaction; source edits or
deletion make the read path withhold unsupported entries before maintenance runs.
Never invert a replacement to resurrect obsolete content. Direct user/imported
notes are not automatically overwritten. A direct reaffirmation removes automatic
source ownership. Explicit forget/undo decisions also fence later model proposals.
Disabling generation retains usable existing memory; it is not source revocation.

`ToolEffect::ManagedMemory` exempts only native bounded memory data updates from
generic strict side-effect approval. It is not an extension/MCP effect declaration.
Explicit tool deny/ask, read/use policy and generation policy remain enforced.
Read-only roles do not gain `memory_update`. Memory text is fallible recall, never
permission, tool authority or an enforced Agent/configuration instruction.
Before adoption/read and again before projection, the service rejects a linked
memory file or immediate managed directory, including Windows junctions. A
repository-controlled `.zuno/RULES.md` or `.zuno` link therefore cannot redirect
the no-approval capability to another file. Ordinary aliases in the selected
worktree/config-root prefix remain supported.

Format 12 adds candidate revision/evidence columns, `resident_memory_provenance`
and `memory_maintenance_state`. Formats 5–11 migrate forward atomically with the
marker last. The exact format-11 release fixture preserves resident revisions,
session, message and Experience values, and tests rollback on the final DDL step.
Existing linked automatic provenance is backfilled without rewriting old candidate
history; newer explicit user edits stay user-owned. Future/corrupt schemas fail closed.

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

Reference audit: local `../codex` at
`9ba1d9eb5bbbd87ba2fc528d91ad239eea975ee9`, particularly
`codex-rs/memories/write/src/{start,phase1,phase2,workspace}.rs`,
`codex-rs/core/src/context/memory.rs` and
`codex-rs/ext/memories/src/tools/ad_hoc_note.rs`. Codex's background consolidation
uses `AskForApproval::Never` with managed write scope, no MCP/recursive delegation,
lease ownership and completed-work watermarks. Zuno adapts those boundaries as a
no-tools typed service and SQLite transaction, not a child shell agent. Codex's
feature enablement is distinct from per-memory approval. Claude Code's
[auto-memory design](https://code.claude.com/docs/en/memory) similarly separates
automatic recall from enforced project instructions. Neither reference implies
that all tools or external imports should bypass permission checks.
