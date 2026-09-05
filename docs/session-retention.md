# Session retention

Retention is a Zuno-owned capability exposed by the two `/api/session/prune`
operations and the matching CLI command.

## The one thing to get right

`--archive` is **reversible**. `--delete` is **irreversible**.

- `--archive` writes a single column: `session.time_archived`. No session,
  message, part, or artifact row is removed. The reverse operation exists in
  the library
  (`zuno_db::prune::PruneRequest::restore_archive`, which sets `time_archived` back
  to `NULL`) and is covered by
  `crates/zuno-db/tests/prune.rs::prune_archive_is_reversible_without_deleting_session_data`.
- `--delete` removes rows from the tables below, sweeps orphaned parts, and
  switches artifact collection into delete mode. There is no undo. Restore from a
  backup or from the source of the data; nothing in this binary can bring it back.

Be aware of one asymmetry before you archive at scale: **the CLI and HTTP surfaces
can set the archive marker but cannot currently clear it.** Reversing an archive
today means calling `restore_archive` from Rust or clearing the column yourself.
The reversibility is real, but it is not yet a flag. Archiving is also not
side-effect-free on a running server — see
[Archiving ends a session's standing HTTP authorizations](#archiving-ends-a-sessions-standing-http-authorizations).

## Archiving ends a session's standing HTTP authorizations

Reversible in the database is not the same as without effect on a running
server. `POST /api/session/prune` with `action: "archive"`, like
`action: "delete"`, withdraws every standing `always` authorization that the
selected sessions granted in that `zuno serve` process, and only those: an
`always` reply is one session's decision, so it does not outlive the session
that gave it.

No durable state is lost, because those authorizations were never durable. They
live in the serving process, and a `zuno serve` restart drops them too. What
changes is that a restored session asks again — `restore_archive` clears
`time_archived`, it does not reinstate an authorization.

Two things follow for an unattended HTTP client that leans on saved `always`
replies:

- Over HTTP the liveness exclusion is the serving process's own set of sessions
  with a turn in flight, and `includeRecent` never widens it. An idle but
  resumable session older than `olderThan` is therefore eligible, and after the
  archive its next permission ask parks for a human that the automation may not
  have. Keep such sessions outside the window, or archive them only once the
  client is finished with them.
- `zuno session prune --archive` runs in its own process and holds no request
  broker, so the CLI withdraws nothing. Archiving from the CLI while a
  `zuno serve` is up leaves that server's authorizations installed until an HTTP
  prune selects the same sessions or the process exits.

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

**21 tables**, in this order:

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
| 10 | `work_plan_archive` |
| 11 | `session_context_epoch` |
| 12 | `session_input` |
| 13 | `session_message` |
| 14 | `part` |
| 15 | `message` |
| 16 | `session_share` |
| 17 | `session_memory_policy` |
| 18 | `session` |
| 19 | `event_sequence` |
| 20 | `event` |
| 21 | `verification_receipt` |
<!-- generated:END prune-tables -->

That table is the ordered part of the delete. After it, the same transaction sweeps every
session-keyed table the live schema declares with no foreign key on that key — today
`human_request` and `provider_retry_backoff` — derived from the schema at run time and
shared with `zuno session delete`, so a table added later cannot be reached by only one of
the two paths. Preview counts those rows too, which is why `database.tables` in the JSON
report can list more tables than the block above.

Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno --test docs
```

After the table deletes, parts with no surviving session are swept, and artifact
collection runs in delete mode.

### Which tool-output roots are swept

Persisted tool output lives in two places, and a delete covers both:

- `$DATA/tool-output`, shared by every session;
- `<worktree>/.zuno/tool-output` for every checkout the pruned database records in
  `project.worktree` — the store a session writes next to the code it is changing.

The rules are identical in both. A file whose name carries a deleted session is
reclaimed. A file that belongs to a surviving session is kept, because that path is how
the model reads back output the size limits withheld. A file whose name carries no
session at all is reclaimed only once it is older than seven days. A checkout that no
`project` row names is not scanned.

A root that cannot be read — a network mount that is offline, a checkout on a volume that
is gone, a directory whose permissions changed, a path where a checkout used to be — is
skipped, not fatal. Its files are left exactly as they are, every other root is still
swept, and the report carries one warning per skipped root:

```text
tool output under /mnt/nas/repo/.zuno/tool-output was not swept: could not inspect /mnt/nas/repo/.zuno/tool-output (No such device)
```

That warning is the only record those files will get. A delete has already removed the
session rows by the time the sweep runs, so a later prune cannot attribute a file named
after a session that no longer exists, and the seven-day rule applies only to files that
carry no session at all. If you need the space, remove that path yourself once it is
reachable again.

## Reading the artifact warning

A report may carry:

```text
`<database>` retains <n> sessions after this operation; snapshot store reclamation is skipped because a shared artifact cannot be attributed to a surviving session and may belong to another channel's database.
```

This is not a failure, and it is not a work item. It says the run could not prove a
snapshot store belongs to the sessions being pruned, so it left those bytes alone; the
same pass still reclaims tool output and attachment objects and reports their bytes.
Preview and `--delete` report this identically, so the projection you approve is the
operation you get. Nothing has to be done by hand: the next prune that runs against this
database while at least one session survives evaluates the snapshot class and reclaims
the store. Do not delete anything under `$ZUNO_DATA/snapshot` yourself — that directory
is shared, a store there can belong to another channel's database, and choosing which is
exactly the judgement the run declined to make. The most common cause is running a
source build against a release install's data directory — the two select different
database files. See [migration.md](migration.md#the-channel-database).

A report may also carry:

```text
`<database>` kept 12 attachment objects whose 12 digests surviving rows name only as free text; model- or tool-authored content can produce that spelling, so those bytes are not reclaimable while such a row survives.
```

An attachment object is kept whenever any surviving row still names its digest, including
a digest that appears only as text inside a message or a tool result. That direction is
deliberate — deleting the only copy of an object a queued prompt still names is
unrecoverable — but it means model- or tool-authored content can hold attachment bytes on
disk. This warning is how you see it: it appears only when bytes were actually held back
that way, or when a payload row was stored as neither text nor a blob and could not be
scanned at all. Deleting the session that holds the row releases those objects on the
next pass.

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

A successful `archive` or `delete` also withdraws the standing HTTP
authorizations of every session it selected —
[Archiving ends a session's standing HTTP authorizations](#archiving-ends-a-sessions-standing-http-authorizations).

The CLI and HTTP previews emit byte-identical JSON —
`crates/zuno-cli/src/cmd/session_prune.rs::session_prune_cli_and_http_preview_json_are_byte_identical`
— so an operator can build a policy against one and audit with the other.
