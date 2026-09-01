# Session retention

Retention is a Zuno-owned capability exposed by the two `/api/session/prune`
operations and the matching CLI command.

## The one thing to get right

`--archive` is **reversible**. `--delete` is **irreversible**.

- `--archive` writes a single column: `session.time_archived`. Nothing is
  removed. The reverse operation exists in the library
  (`zuno_db::prune::PruneRequest::restore_archive`, which sets `time_archived` back
  to `NULL`) and is covered by
  `crates/zuno-db/tests/prune.rs::prune_archive_is_reversible_without_deleting_session_data`.
- `--delete` removes rows from the tables below, sweeps orphaned parts, and
  switches artifact collection into delete mode. There is no undo. Restore from a
  backup or from the source of the data; nothing in this binary can bring it back.

Be aware of one asymmetry before you archive at scale: **the CLI and HTTP surfaces
can set the archive marker but cannot currently clear it.** Reversing an archive
today means calling `restore_archive` from Rust or clearing the column yourself.
The reversibility is real, but it is not yet a flag.

## Derived learning when deleting one session

Interactive session deletion makes the learning choice explicit:

- **keep learning** preserves derived Experience records and detaches their
  nullable session/message provenance when the transcript is removed;
- **clean learning** first creates pending-review reversals for applied Memory
  and Skill changes, rejects pending candidates that cite the evidence, then
  marks the Experience records `forgotten`.

Cleaning never silently removes an applied Memory entry or Skill. The TUI asks
for the choice. ACP `session/delete` requires
`cleanupDerivedExperiences: true|false`. Standalone
`zuno session delete <id>` requires exactly one of
`--keep-derived-experiences` or `--cleanup-derived-experiences`; cleanup with
derived rows must run through a live TUI or ACP learning profile so it can
prepare the review candidates.

`zuno session prune --delete` is the bulk retention path. It retains project
Experience, patterns, evaluations, Memory, and Skill candidates while deleting
session-owned feedback and learning jobs. This behavior is included in its
destructive confirmation and documented here rather than silently treating
project learning as transcript data.

## Preview first, always

With neither `--archive` nor `--delete`, the command is a preview and mutates
nothing —
`crates/zuno-db/tests/prune.rs::prune_default_preview_is_inert_across_every_real_table`
asserts inertness across every table, and
`prune_preview_counts_exactly_match_the_subsequent_transactional_delete` asserts
the preview's counts are the counts the delete then produces.

```sh
zuno session prune --older-than 90
zuno session prune --older-than 90 --format json
```

## Flags

| flag | effect |
|---|---|
| `--older-than DAYS` | required; the retention window |
| `--by updated\|created` | which timestamp the window applies to; default `updated` |
| `--project PATH\|ID` | scope to one project; default is the current project |
| `--all-projects` | every project; conflicts with `--project` |
| `--archive` | set the reversible archive marker; conflicts with `--delete` |
| `--delete` | irreversibly remove; conflicts with `--archive` |
| `--include-shared` | do not exclude shared sessions from selection |
| `--include-recent` | do not exclude recently active sessions |
| `--force` | proceed when a shared session's remote copy cannot be unshared |
| `--yes` | pre-confirm a delete; requires `--delete` |
| `--format table\|json` | output shape; default `table` |

## The confirmation gate

A delete never proceeds unasked. On a TTY you get a prompt. Without a TTY, and
without `--yes`, the command refuses:

```text
--delete requires --yes when stdin is not a TTY; nothing was changed
```

Answering anything but yes at the prompt is the same refusal:

```text
session deletion cancelled; nothing was changed
```

Both are asserted in `crates/zuno-cli/src/cmd/session_prune.rs`. Note that the
refusal happens **before** stdin is read at all, so a delete in a pipeline cannot
be confirmed by whatever bytes happen to arrive.

## Shared sessions

A shared session whose remote copy cannot be unshared is refused, not silently
deleted locally. `--force` proceeds and says so verbatim in the report's warnings:

```text
remote unshare failed for shared session <id>: <detail>; local rows were deleted because --force was supplied and the remote copy may survive
```

That is the honest statement: the local rows are gone and the remote copy may not
be.

## What a delete touches

Generated from `zuno_db::prune::DELETE_ORDER`. The order is pinned by
`crates/zuno-db/tests/prune.rs::prune_delete_order_and_true_related_table_count_are_pinned`,
because the order is what keeps foreign keys satisfied mid-transaction.

**18 tables**, in this order:

<!-- generated:BEGIN prune-tables -->
| order | table |
|---:|---|
| 1 | `session_note_operation` |
| 2 | `session_note` |
| 3 | `memory_reflection_job` |
| 4 | `memory_reflection_delivery` |
| 5 | `learning_job` |
| 6 | `message_feedback` |
| 7 | `agent_job` |
| 8 | `work_item` |
| 9 | `work_plan` |
| 10 | `session_context_epoch` |
| 11 | `session_input` |
| 12 | `session_message` |
| 13 | `part` |
| 14 | `message` |
| 15 | `session_share` |
| 16 | `session` |
| 17 | `event_sequence` |
| 18 | `event` |
<!-- generated:END prune-tables -->

Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs
```

After the table deletes, parts with no surviving session are swept, and artifact
collection runs in delete mode.

## Reading the artifact warning

A report may carry:

```text
`<database>` contains <n> sessions; artifact reclamation is skipped because shared snapshot stores cannot be attributed and may belong to another channel's database.
```

This is not a failure. It says the run could not prove a snapshot store belongs to
the sessions being pruned, so it left the bytes alone. The most common cause is
running a source build against a release install's data directory — the two select
different database files. See [migration.md](migration.md#the-channel-database).

An `n` of `0` here alongside a database you know has sessions is the signal that
you are looking at the wrong database, not that your sessions are gone.

## Over HTTP

```sh
curl 'localhost:PORT/api/session/prune?olderThan=90&by=updated'
```

`GET` is the preview and is inert. `POST` mutates and requires `apply: true`
explicitly:

```sh
curl -X POST localhost:PORT/api/session/prune \
  -H 'content-type: application/json' \
  -d '{"olderThan":90,"action":"archive","apply":true}'
```

Without it:

```text
session prune mutation requires `apply: true`; nothing was changed
```

The CLI and HTTP previews emit byte-identical JSON —
`crates/zuno-cli/src/cmd/session_prune.rs::session_prune_cli_and_http_preview_json_are_byte_identical`
— so an operator can build a policy against one and audit with the other.
