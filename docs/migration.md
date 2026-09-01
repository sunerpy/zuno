# Zuno database lifecycle

Zuno owns its configuration and data roots. The current database format is 6.
Empty databases are created at the current format, and supported older formats advance
through guarded forward migrations. Format 5 is the first supported historical format
and upgrades in place to format 6 without rebuilding the database.

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

1. **Empty database.** The complete format-6 schema and the single `zuno_schema`
   marker are created atomically.
2. **Format 6.** The marker and required current tables are validated before
   application queries run.
3. **Format 5.** The additive learning schema is migrated to format 6 in place.
4. **Any other state.** An older unsupported format, a future format, a missing marker,
   or a marker whose required tables are absent fails closed without modification.

### Format 5 to format 6

The supported migration uses one SQLite `BEGIN IMMEDIATE` transaction:

1. Re-read the table inventory and require the marker to be exactly format 5.
2. Require the format-5 `session` table before changing anything.
3. Create all format-6 learning tables and indexes.
4. Update the singleton marker from 5 to 6 with a conditional update.
5. Commit only after every schema operation and the marker update succeed.

Any failure rolls the transaction back. The migration does not rewrite existing
`session`, `message`, or `memory_candidate` rows. Tests compare representative row
values before and after migration and then query the new learning tables.

Changing only `zuno_schema.format` is never a valid repair: application queries require
the matching tables and indexes. Do not manually advance or downgrade the marker.

### Unsupported, future, or corrupt formats

Zuno refuses an unsupported schema format before serving application queries and
never deletes or rewrites the rejected database. Preserve the original file and take a
copy before any operator-led recovery.

For important data, use the exact older binary to export it or implement and validate an
explicit forward migration. Do not guess the schema, silently drop rows, or require a
rebuild for a format that the current binary supports. A valid format-5 database should
open and migrate automatically.

## Rules for future schema changes

Once a database format has shipped, a schema change must include:

- a guarded forward migration from every format still declared supported;
- one atomic transaction with the format marker updated last;
- exact old-format fixtures rather than a current schema with only its marker edited,
  except where the change is proven purely additive and the fixture removes every new
  table and index;
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
