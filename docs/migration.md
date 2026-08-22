# Zuno database lifecycle

Zuno reads only its own config and data roots. The migrations on this page evolve a database already selected and created by Zuno.

## The channel database

This is the first thing that surprises people, and it looks like lost state:
**you run the binary, and the session list is empty.**

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

## Opening an existing Zuno database

Three states are recognized:

1. **Empty file.** The current schema is created and the journal is pre-filled
   with every migration id.
2. **A database with a `session` table and a `migration` journal.** Only
   migrations whose id is not recorded are run, each in its own transaction.
3. **Any existing session database without Zuno's `migration` journal.** It is
   refused without modification as an unsupported pre-release format.

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
the observed id. Keep the backup before allowing any version to migrate the file.

## The 40 migrations

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
| 39 | `20260821160000_agent_job` |
| 40 | `20260822170000_generalize_agent_job_subject` |
<!-- generated:END migration-journal -->

Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs
```

## Provider configuration

Provider coverage is stated per **wire-protocol family**, not per vendor name.
SigV4 plus EventStream, Gemini's wire format with Vertex auth, and the
OpenAI-compatible family cannot share a request builder, so what is claimed is
the family.

The practical consequence: if your provider id is not claimed by any family, you
get an error naming it rather than a silent attempt to route it through the
OpenAI-compatible profile. A failure that names the id is the intended outcome —
it is faster to act on than a request that goes out in the wrong shape.
