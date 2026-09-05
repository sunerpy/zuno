# Zuno database lifecycle

Zuno owns its configuration and data roots. The current database format is 9.
Empty databases are created at the current format, and supported older formats advance
through guarded forward migrations. Format 5 is the first supported historical format
and upgrades in place to format 9 without rebuilding the database.

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

1. **Empty database.** The complete format-9 schema and the single `zuno_schema`
   marker are created atomically.
2. **Format 9.** The marker and required current tables are validated before
   application queries run.
3. **Format 8.** The revisioned `session_memory_policy` table and its index are added
   in place.
4. **Format 7.** The `verification_receipt` ledger, then session memory policy, are
   added in place.
5. **Format 6.** Durable Plan-stack columns, `work_plan_archive`, the
   `verification_receipt` ledger, and session memory policy are added in place.
6. **Format 5.** The additive learning schema, Plan-stack schema,
   `verification_receipt`, and session memory policy are migrated to format 9 in one
   transaction.
7. **Any other state.** An older unsupported format, a future format, a missing marker,
   or a marker whose required tables are absent fails closed without modification.

Two processes that open or upgrade the same database at the same time both decide from
the format they saw before taking SQLite's write lock. The one that loses the lock does
not fail: it re-reads the marker and validates, upgrades, or rejects what the winner
actually committed, making at most four attempts in total. An unsupported format is still
reported as a schema mismatch; a database whose format keeps changing under the opener
fails closed with a conflict on the `zuno_schema` marker. Neither path writes to the
database.

### Format 5, 6, 7, or 8 to format 9

The supported migration uses one SQLite `BEGIN IMMEDIATE` transaction:

1. Re-read the table inventory and require the marker to be exactly format 5, 6, 7,
   or 8.
2. Require the historical `session` and `work_plan` tables before changing anything.
3. From format 5, create all format-6 learning tables and indexes.
4. From format 5 or 6, add nullable `parent_plan_id`, defaulted `stack_depth`, and
   `work_plan_archive` without rewriting the active Plan row.
5. Create the `verification_receipt` ledger, which starts empty and rewrites no row.
6. Create `session_memory_policy`, which starts empty so every existing session keeps
   using the default supplied by its caller.
7. Update the singleton marker from 5, 6, 7, or 8 to 9 with a conditional update.
8. Commit only after every schema operation and the marker update succeed.

Any failure rolls the transaction back. The migration does not rewrite existing
`session`, `message`, `memory_candidate`, `learning_job`, `verification_receipt`, or
`work_plan` values. Tests use exact format-5, format-6, format-7, and v0.10.5
format-8 fixtures, compare every old row before and after, then query the new policy
table.

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
format-7, or format-8 database should open and migrate automatically.

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
