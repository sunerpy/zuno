# Zuno database lifecycle

Zuno reads only its own config and data roots. Zuno does not import an opencode database,
restore opencode sessions, or fall back to an opencode directory. The
migrations on this page evolve a database already selected by Zuno; they are not
a cross-product import mechanism.

## The channel database

This is the first thing that surprises people, and it looks exactly like a
compatibility failure: **you run the binary, and the session list is empty.**

Nothing is lost. The two builds selected different database files.

The filename is chosen by build channel:

| condition | file |
|---|---|
| `ZUNO_DB` is `:memory:` | in memory |
| `ZUNO_DB` is an absolute path | that path, verbatim |
| `ZUNO_DB` is relative | joined onto the data directory, **not** the working directory |
| channel is `latest`, `beta`, or `prod`, or `ZUNO_DISABLE_CHANNEL_DB` is exactly `1` or `true` | `zuno.db` |
| otherwise | `zuno-<channel>.db` |

A build from source has no channel define, so its channel is `local` and it
resolves `zuno-local.db`. An installed release resolves `zuno.db`. Both filenames
and their data root belong to Zuno.

To read the Zuno release database from a source build, pick one:

```sh
ZUNO_DISABLE_CHANNEL_DB=1 zuno session list
ZUNO_DB="${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db" zuno session list
```

`ZUNO_DISABLE_CHANNEL_DB` is matched **case-sensitively** against exactly `1`
or `true`. `TRUE`, `yes`, and `on` do nothing —
`crates/zuno-paths/src/files.rs::disable_channel_db_forces_the_unsuffixed_name_case_sensitively`
pins that.

Confirm which file you are on before concluding anything:

```sh
zuno debug paths
```

The session-prune report also warns when it cannot attribute artifacts to the
database it opened. See
[session-retention.md](session-retention.md#reading-the-artifact-warning).

## Pre-rename Zuno database filename

An earlier unreleased Zuno build used `opencode.db` or
`opencode-<channel>.db` inside the **Zuno** data root. When the computed default
`zuno*.db` is absent but its corresponding old filename exists, Zuno refuses to
open or move either file and reports both paths. Move the file explicitly after
checking it:

```sh
mv "${XDG_DATA_HOME:-$HOME/.local/share}/zuno/opencode.db" \
  "${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db"
```

If both names exist, the `zuno*.db` file is authoritative. Explicit file APIs
such as `open_at` do not reinterpret a basename; this diagnostic is confined to
the computed default database path.

## Opening an existing Zuno database

Three states, and this binary handles all three:

1. **Empty file.** The current schema is created and the journal is pre-filled
   with every migration id.
2. **A database with a `session` table and a `migration` journal.** Only
   migrations whose id is not recorded are run, each in its own transaction.
3. **A database older than the `migration` table** — it has a `session` table and
   a Drizzle journal, `__drizzle_migrations`. The `migration` table is created,
   seeded from the names Drizzle recorded, and the remaining migrations run.

State 3 preserves the schema lineage represented by a Zuno database created
before the current journal existed. Its regression fixture is historical
verification data; it does not make an opencode database a supported import.

A database with a `session` table and *neither* journal is refused without being
modified. That shape cannot be migrated safely by either implementation, and the
test asserts the data is left untouched.

A non-empty database with no `session` table is refused outright:

```text
database is not empty and has no session table
```

### Before schema migration

Migration is forward-only. There is no downgrade. Copy the file first:

```sh
cp "${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db" "${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db.backup"
```

The migration journal is forward-only. If it contains an id above this binary's
known ceiling, Zuno refuses to open the database and names both the ceiling and
the observed id. Compatibility tests against historical fixtures remain useful
verification assets, but they do not establish a supported cross-binary rollback
contract. Keep the backup before allowing any version to migrate the file.

## The 38 migrations

Generated from `zuno_db::migration::MIGRATION_IDS`, in execution order.
`CURRENT_VERSION` equals this list's length; the documentation gate asserts that,
so adding a migration without documenting it fails.

<!-- generated:BEGIN migration-journal -->
| # | migration id |
|---:|---|
| 1 | `20260127222353_familiar_lady_ursula` |
| 2 | `20260211171708_add_project_commands` |
| 3 | `20260213144116_wakeful_the_professor` |
| 4 | `20260225215848_workspace` |
| 5 | `20260227213759_add_session_workspace_id` |
| 6 | `20260228203230_blue_harpoon` |
| 7 | `20260303231226_add_workspace_fields` |
| 8 | `20260309230000_move_org_to_state` |
| 9 | `20260312043431_session_message_cursor` |
| 10 | `20260323234822_events` |
| 11 | `20260410174513_workspace-name` |
| 12 | `20260413175956_chief_energizer` |
| 13 | `20260423070820_add_icon_url_override` |
| 14 | `20260427172553_slow_nightmare` |
| 15 | `20260428004200_add_session_path` |
| 16 | `20260501142318_next_venus` |
| 17 | `20260504145000_add_sync_owner` |
| 18 | `20260507164347_add_workspace_time` |
| 19 | `20260510033149_session_usage` |
| 20 | `20260511000411_data_migration_state` |
| 21 | `20260511173437_session-metadata` |
| 22 | `20260601010001_normalize_storage_paths` |
| 23 | `20260601202201_amazing_prowler` |
| 24 | `20260602002951_lowly_union_jack` |
| 25 | `20260602182828_add_project_directories` |
| 26 | `20260603001617_session_message_projection_indexes` |
| 27 | `20260603040000_session_message_projection_order` |
| 28 | `20260603141458_session_input_inbox` |
| 29 | `20260603160727_jittery_ezekiel_stane` |
| 30 | `20260604172448_event_sourced_session_input` |
| 31 | `20260605003541_add_session_context_snapshot` |
| 32 | `20260605042240_add_context_epoch_agent` |
| 33 | `20260611035744_credential` |
| 34 | `20260611192811_lush_chimera` |
| 35 | `20260612174303_project_dir_strategy` |
| 36 | `20260622142730_simplify_session_context_epoch` |
| 37 | `20260622170816_reset_v2_session_state` |
| 38 | `20260622202450_simplify_session_input` |
<!-- generated:END migration-journal -->

Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs
```

## Configuration that is rejected rather than reinterpreted

A configuration carrying a deprecated form is refused with a message naming the
replacement and the offending file. This is deliberate: quietly accepting a
deprecated key leaves a configuration that behaves differently from what it says.
All eleven forms, with the exact messages, are in
[rejected-inputs.md](rejected-inputs.md) — including a config file still under its
pre-rename name, which Zuno reports rather than silently ignoring.

## Provider configuration

Provider coverage is stated per **wire-protocol family**, not per vendor name.
SigV4 plus EventStream, Gemini's wire format with Vertex auth, and the
OpenAI-compatible family cannot share a request builder, so what is claimed is
the family.

The practical consequence: if your provider id is not claimed by any family, you
get an error naming it rather than a silent attempt to route it through the
OpenAI-compatible profile. A failure that names the id is the intended outcome —
it is faster to act on than a request that goes out in the wrong shape.
