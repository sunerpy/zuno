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

Zuno is unreleased and intentionally has no incremental database migration
chain. Two states are recognized:

1. **Empty file.** The complete current schema and one `zuno_schema` format
   marker are created atomically.
2. **Non-empty file.** The format marker must equal
   `zuno_db::migration::CURRENT_FORMAT`; otherwise the unsupported pre-release format
   is refused without modification.

There is no ALTER, backfill, downgrade, or best-effort compatibility path. A
schema change bumps the format and development databases are rebuilt. This keeps
the current schema as the only source of truth and avoids carrying unreleased
history into the product.

When a format is rejected, preserve the file if its data matters, then select a
new database path or remove the old development database yourself:

```sh
ZUNO_DB=/tmp/zuno-current.db zuno
```

Zuno never deletes or rewrites a rejected database automatically.

## Provider configuration

Provider coverage is stated per **wire-protocol family**, not per vendor name.
SigV4 plus EventStream, Gemini's wire format with Vertex auth, and the
OpenAI-compatible family cannot share a request builder, so what is claimed is
the family.

The practical consequence: if your provider id is not claimed by any family, you
get an error naming it rather than a silent attempt to route it through the
OpenAI-compatible profile. A failure that names the id is the intended outcome —
it is faster to act on than a request that goes out in the wrong shape.
