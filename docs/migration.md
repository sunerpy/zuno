# Zuno database lifecycle

Zuno owns its configuration and data roots. The current database format is 14.
Empty databases are created at the current format, and supported older formats advance
through guarded forward migrations. Format 5 is the first supported historical format
and formats 5 through 13 upgrade in place to format 14 without rebuilding the database.

## The channel database

An empty session list after switching binaries often means the two builds selected
different database files, not that history was deleted.

The filename is chosen by build channel:

| condition | file |
|---|---|
| `ZUNO_DB` is `:memory:` | in memory |
| `ZUNO_DB` is an absolute path | that path, verbatim |
| `ZUNO_DB` is relative | joined onto the data directory, **not** the working directory |
| channel is `latest`, `beta`, or `prod`, or `ZUNO_DISABLE_CHANNEL_DB` is exactly `1` or `true` | `zuno.db` |
| otherwise | `zuno-<channel>.db` |

A source build has no channel define, so its channel is `local` and it normally resolves
`zuno-local.db`. An installed release resolves `zuno.db`.

Linux and macOS:

```sh
ZUNO_DISABLE_CHANNEL_DB=1 zuno session list
ZUNO_DB="${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db" zuno session list
```

Windows PowerShell:

```powershell
$env:ZUNO_DISABLE_CHANNEL_DB = "1"
zuno session list

$env:ZUNO_DB = Join-Path $HOME ".local\share\zuno\zuno.db"
zuno session list
```

`ZUNO_DISABLE_CHANNEL_DB` is matched **case-sensitively** against exactly `1` or
`true`. `TRUE`, `yes`, and `on` do nothing. Run `zuno debug paths` before diagnosing
missing state.

The session-prune report also warns when it cannot attribute artifacts to the database
it opened. See
[session-retention.md](session-retention.md#reading-the-artifact-warning).

## Opening an existing Zuno database

Database opening recognizes these states:

1. **Empty database.** The complete format-14 schema and the single `zuno_schema`
   marker are created atomically.
2. **Format 14.** The marker, tables, constraints, indexes and triggers are validated
   before application queries run.
3. **Formats 5–13.** Every remaining supported migration runs in a single
   transaction: learning (6), Plan stack (7), verification receipts (8), session
   memory policy (9), execution/inbox state (10), versioned memory and search (11),
   automatic-memory provenance and watermarks (12), then durable questions and
   session scheduling (13), then input processing receipts, canonical Context
   snapshots and revision-bound Goal resume choices (14).
4. **Any other state.** An older unsupported format, a future format, a missing marker,
   or a marker whose required tables are absent fails closed without modification.

Two processes that open or upgrade the same database at the same time both decide from
the format they saw before taking SQLite's write lock. The one that loses the lock does
not fail: it re-reads the marker and validates, upgrades, or rejects what the winner
actually committed, making at most four attempts in total. An unsupported format is still
reported as a schema mismatch; a database whose format keeps changing under the opener
fails closed with a conflict on the `zuno_schema` marker. Neither path writes to the
database.

### Formats 5–13 to format 14

The supported migration uses one SQLite `BEGIN IMMEDIATE` transaction:

1. Re-read the table inventory and require the marker to be exactly format 5, 6, 7,
   8, 9, 10, 11, 12, or 13.
2. Require the historical `session` and `work_plan` tables before changing anything.
3. From format 5, create all format-6 learning tables and indexes.
4. From format 5 or 6, add nullable `parent_plan_id`, defaulted `stack_depth`, and
   `work_plan_archive` without rewriting the active Plan row.
5. Create the `verification_receipt` ledger, which starts empty and rewrites no row.
6. Create `session_memory_policy`, which starts empty so every existing session keeps
   using the default supplied by its caller.
7. Add `source_key`, `trigger_kind`, and `cycle_id` to `session_input`; create
   `session_execution_state` and `completion_delivery` plus their indexes. Existing
   inputs retain `trigger_kind = 'legacy'`.
8. From formats before 11, add resident document/revision state, source validation,
   ownership tokens, retrieval snapshots and incremental Unicode/CJK search indexes.
9. Add nullable candidate `base_revision`/`evidence`, `resident_memory_provenance`,
   `memory_maintenance_state` and their indexes. Backfill exact linked automatic
   provenance without reclassifying newer user-authored memory.
10. Add `question_interaction`, `question_action_receipt` and their index. Valid
    legacy questions gain stable item IDs in companion rows; their original
    `human_request` payloads and responses are not rewritten.
11. Add nullable `session_execution_state.scheduling`. Repair only an exactly
    evidenced legacy `running` row whose latest structured driver event is
    `paused/no_progress`; preserve its cycle and progress.
12. Preserve the published question definition bytes while widening the purpose
    constraint to include native `goal_resume`. Create `session_input_receipt` and
    `session_context_usage` and their indexes. Old consumed inputs become
    `recorded`, never `applied` or `completed` without provider evidence.
13. Conditionally update the singleton marker from the exact observed old format to
    14, last. Commit only after every operation succeeds.

Any failure rolls the transaction back. The migration does not rewrite existing
`session`, `message`, `memory_candidate`, `learning_job`, `verification_receipt`, or
`work_plan` values except the documented source-validation/lease backfills.
Tests use exact format-5 through format-13 release fixtures, compare representative
session/message/memory values and preserved rows, then verify new objects and marker.
Format-11 tests include previously published resident revisions and rollback of the
entire additive change if the final index creation fails.

The format-14 migration never runs a model, extracts Memory, invents Context
counts, or resumes a paused Goal. Legacy source verification and optional
reprocessing are resumable learning jobs, not migration side effects.
Use `/learn repair-history --dry-run` to inspect repair eligibility before
`/learn repair-history`; missing evidence stays unverified.

### Session execution and completion delivery

`session_execution_state` stores collaboration mode independently from the selected
Agent, the saved Work identity, exact authorized and handoff-ready Plan revisions,
continuation cycle and context epoch, and any explicit Draft-review risk acceptance.
`/start-work` reads the Plan and review gate, updates Goal state, writes Work authority,
and admits its `UserControl` input in one `BEGIN IMMEDIATE` transaction.

`completion_delivery` is the exactly-once ownership ledger for background commands,
subagents, workflows, and product Agents. Terminal `bg output`, synchronous `bg wait` and an asynchronous
callback compete for one `inline` or `callback` owner. The losing path cannot admit a
second turn. `session_input.source_key` makes producer admission idempotent across
restarts, while `trigger_kind` distinguishes user, control, automatic, and recovery
turns without inventing user history.

Scheduling distinguishes ready, exact human/external waits, paused and completed
sessions, including ordinary sessions without a Goal. Automatic callbacks never
clear an unrelated gate or invent an origin cycle. Validated explicit answers and
user controls use the same transactional inbox promotion boundary.

Question rows separate confirmed answers from `draftAnswers`; Defer and empty
answers do not generate model input. Request revisions and command receipts make
retries idempotent. Plan consent is bound to the exact Plan/review/Work identity and
source cycle; only a successful logical handoff can apply early approval.

`session_input_receipt` separates acceptance, recording, model application and
terminal completion. A callback recorded while a Goal is paused is not evidence
that the model processed it. A native Goal resume choice binds the exact Goal
ID/revision and existing input ID; answering commits Goal/execution/input changes
together. Skipping never authorizes a resume or replays an old user message.

`session_context_usage` stores a source-scoped tracker and its revision, epoch
and update time. Main, child, learning and compaction requests cannot overwrite
one another's context window. The application reconstructs known usage from
durable request evidence; unknown counters remain unknown.

### Per-session memory policy

`session_memory_policy` is a one-to-one session sidecar. It is never stored in the
opaque `session.metadata` column.

- `use_memories` controls whether the session may use resident or retrieved memories.
- `generation` is `enabled`, `disabled`, or `excluded`.
- `reason`, `source`, and the update time make the current choice auditable.
- `revision` is a compare-and-set revision. Revision zero means no row exists yet.

A missing row on a migrated session is not written merely by reading it. The reader
returns the exact default its caller supplied. New sessions freeze that default when
their first durable row is created, and child sessions inherit the parent's policy in
their creation transaction. `set` and `exclude` update the policy and append
`session.memory.policy.changed` to the durable session event stream in the same
transaction. A stale revision writes neither row nor event.

`disabled` is a reversible generation setting and marks queued automatic extraction
for that session `skipped`. `excluded` performs the same queue settlement and records
that the session cannot be re-enabled after configured external context. Running or
already terminal jobs are not replayed or rewritten.

Changing only `zuno_schema.format` is never a valid repair: application queries require
the matching tables and indexes. Do not manually advance or downgrade the marker.

### Unsupported, future, or corrupt formats

Zuno refuses an unsupported schema format before serving application queries and
never deletes or rewrites the rejected database. Preserve the original file and take a
copy before any operator-led recovery.

For important data, use the exact older binary to export it or implement and validate an
explicit forward migration. Do not guess the schema, silently drop rows, or require a
rebuild for a format that the current binary supports. A valid format-5, format-6,
format-7, format-8, format-9, format-10, format-11, format-12, or format-13 database should open and migrate automatically.

## Rules for future schema changes

Once a database format has shipped, a schema change must include:

- a guarded forward migration from every format still declared supported;
- one atomic transaction with the format marker updated last;
- exact old-format fixtures rather than a current schema with only its marker edited,
  with no exception for changes that look purely additive: a migration that reaches the
  new shape through `ALTER TABLE` leaves columns in a different order than a freshly
  created database, so a fixture reverse-engineered from the current schema does not
  exercise the path a real user's database takes;
- comparison by structural equivalence, meaning tables, columns, types, indexes, foreign
  keys, and the marker, rather than by `sqlite_master` text, which legitimately differs
  between a migrated and a freshly created database;
- row-level before/after assertions for durable user data, including representative
  sessions, messages, and memory;
- validation that future, unmarked, and structurally corrupt formats fail closed without
  mutation.

Downgrades and best-effort compatibility are not supported.

## Provider configuration

Provider coverage is stated per **wire-protocol family**, not per vendor name. SigV4
plus EventStream, Gemini's wire format with Vertex auth, and the OpenAI-compatible
family cannot share a request builder.

If a provider id is not claimed by any family, Zuno returns an error naming it rather
than silently trying the OpenAI-compatible route. A named failure is the intended,
actionable outcome.
