# Memory and learning

Zuno automatically extracts and maintains useful recall data. Ordinary Memory
does not require per-entry approval; executable Skill changes still do.

| Kind | What it holds | Where it appears | Who can apply it |
| --- | --- | --- | --- |
| Resident Memory | A small global preference or project rule | Versioned global/project context on model requests | Automatic by default, with an auditable `MemoryCandidate` |
| Experience | Evidence from one outcome, correction, failure, or verified procedure | Retrieved `learning.experiences` prompt section and `/learn` | The learning service writes evidence; it does not edit Memory directly |
| Skill candidate | A proposed reusable method with complete `SKILL.md`, diff, and evidence | `/learn` review state | A user, after review and a passing offline evaluation |

Unresolved issues may be stored as Experience, but they cannot become Memory,
pattern evidence, or Skill evaluation evidence.

## Check the current state

The TUI exposes four native commands:

| Command | Use it for |
| --- | --- |
| `/memory` | Inspect resident entries and approve, edit, reject, remove, or undo Memory candidates |
| `/memories` | Change whether the current session uses Memory and whether it may generate learning |
| `/learn` | Inspect Experience, feedback, patterns, evaluation runs, and Skill candidates |
| `/reflect [turn\|session]` | Run the no-tools extractor now instead of waiting for background eligibility |

Run `/learn help` for the complete action list:

```text
/learn
/learn status
/learn list [offset]
/learn get <experience-id>
/learn reprocess <assistant-message-id>
/learn repair-history [--dry-run]
/learn inspect-memory|import-memory <global|project>
/learn remember <stable fact, preference, or project rule>
/learn issue <unresolved issue>
/learn solved <experience-id> <resolution>
/learn forget <experience-id>
/learn promote <experience-id>
/learn feedback <assistant-message-id> positive|negative <expected-revision> [note]
/learn pattern-promote|pattern-reject <pattern-id>
/learn skill-review|skill-apply|skill-reject|skill-undo <candidate-id>
```

The HTTP projection is `GET /api/session/{sessionID}/learning`. ACP publishes the
same state in `_meta.zuno.learning`. These are views over durable stores; disabling
generation does not make prior records unreadable.

## Session policy

Global configuration sets the maximum capability. Each session freezes a
revisioned policy when it is materialized:

| Field | Meaning |
| --- | --- |
| `use_memories` | Include resident Memory and automatic Experience retrieval in later prompts |
| `generation=enabled` | Allow explicit and automatic learning work |
| `generation=disabled` | Stop new extraction while retaining existing Memory and Experience |
| `generation=excluded` | Fail-closed state for an ineligible session; the session cannot enable generation again |

`/memories` updates that policy for the current session. Turning use off changes
future prompt assembly; it does not delete files, Experience, candidates, or audit
records. Existing sessions do not inherit a later configuration-default change.

A new child copies the latest durable parent policy in the same transaction that
creates its session and job. A parent at revision 1 or later is valid; fallback values
are a separate, unversioned input used only for legacy parents with no policy row.
The child starts its own revision 1, preserves disabled/excluded choices, and is not
rewritten when the parent later changes. No database rebuild or revision reset is needed.

HTTP clients can read and update the policy through
`GET|PUT /api/session/{sessionID}/memory-policy`. Updates include
`expectedRevision`; a stale revision returns `409`. A client may request only
`enabled` or `disabled`. `excluded` is assigned by the host.

When `learning.post_turn.disable_on_external_context` is enabled, a completed turn
that consumed a successful Web or MCP result moves the session to `excluded`.
Zuno reads durable tool metadata for that decision, not text in the transcript.

## Automatic resident Memory

The old tool id `memory_propose` is replaced by `memory_update`. Existing permission
rules, tool switches or Agent allowlists using the old id fail configuration
validation with a rename instruction; an old deny or disable choice is never
silently dropped.

The model-visible mutation tool is `memory_update`. It accepts:

- `target`: `global` or `project`;
- `action`: `add`, `replace`, or `remove`;
- the proposed full entry in `content` for add/replace;
- a unique `old_text` locator for replace/remove;
- `expected_revision` from `memory_read` or the current prompt; without it,
  replace/remove must copy the exact full existing entry;
- a durable reason and confidence value.

The tool commits an audited `MemoryCandidate` through the managed memory service,
not through arbitrary file writes. `memory_read` returns current global/project
entries and revisions, with optional `target`, `query`, and `limit` filters.
It also reports how many unsupported derived entries were withheld.
Validation rejects malformed or ambiguous operations, over-budget output,
prompt-injection patterns, known credential literals, unreadable files, and drift
from the version that was reviewed. Temporary failures, unresolved guesses,
secrets, and task narration are not suitable Memory.

The default promotion policy is `automatic`. An explicit existing `review` or
`high_confidence` setting remains effective:

| `memory.promotion` | Behavior |
| --- | --- |
| `review` | Leave every proposal pending for a user |
| `high_confidence` | Apply proposals at or above `memory.auto_confidence`; retain the rest |
| `automatic` | Apply every proposal that passes the same validation and safety checks |

`memory.auto_confidence` defaults to `0.9` and is used only by `high_confidence`.
Foreground and background changes obey the same promotion choice. Global
automatic memory needs explicit user evidence and is reserved for cross-project
preferences; repository knowledge stays project-scoped.

Memory updates are a narrow native data capability. They do not trigger generic
strict side-effect approval, but an explicit tool `deny`/`ask` and the session
generation policy still apply. Memory never grants Shell, filesystem, MCP or
Skill authority. `build`, `deep`, `general`, `fixer` and the orchestrator can use
`memory_update`; read-only roles receive recall tools without the update grant.

Background learning has two stages: extraction records source-linked Experience
and raw memory hints; a separate no-tools maintenance job deduplicates, corrects
and consolidates them with current memory and explicit user changes. It consumes
at most 64 recent validated experiences within the configured input budget and
commits at most 32 changes atomically. A successful no-op advances its durable
watermark, so unchanged input is not sent to the model on every poll.
One semantic repair is allowed for an invalid plan. Stale revisions, lost leases
or source-policy changes cannot commit a stale plan.
Each memory job is claimed only by its bound canonical memory path; switching
worktrees leaves other paths' queued jobs and attempt budgets intact.

### Apply and undo recovery

Managed memory files and their immediate directories must be regular paths,
not symbolic links or Windows junctions. This is checked before importing or
reading files and again before publishing a projection. Keep actual memory in
the managed location; use ordinary approved file tools for other files.

Resident entries now have an authoritative SQLite revision. Applying a candidate
commits its exact before/after snapshots, the new entries, revision history, and
the `applied` state in one transaction. Undo advances the revision and records
`undone` in the same way. Two writers cannot both replace the same revision.

The global and project Markdown files are readable projections. Existing files
are adopted once; later file edits do not silently replace accepted Memory.
Projection failure leaves the committed entries available. Startup can restore a
missing projection from its recorded revision, while a file with different
external contents is preserved and reported. Cooperating file writers use a
shared operating-system lock around comparison and atomic replacement.

The host refreshes accepted Memory at the next provider-request or compaction
safe point, including between model steps of one foreground turn. A change made
by background maintenance or another session can therefore enter the next request
without starting a new foreground turn. The current `/memories` use policy still
applies. Requests already sent and their persisted prompt receipts retain the
exact revision and content they used.

Older releases may have left candidates in `applying` or `undoing`. Startup
continues to reconcile these historical states from their exact snapshots:

- the expected after state proves apply completed;
- the expected before state proves it did not;
- any third state becomes `uncertain`.

These historical uncertain mutations are never mechanically replayed. Inspect
an `uncertain` candidate and the preserved file before deciding what to retain.
Projection repair does not repeat the logical Memory operation or advance its
revision. Adding an existing entry also preserves the document revision.

Use `/learn inspect-memory global|project` to compare the accepted version with
file entries and inspect a projection error. After reviewing an external edit,
`/learn import-memory global|project` explicitly accepts it as a new version.
An unresolved pre-upgrade write is withheld from prompts until inspected and
imported; neither side is silently chosen or deleted.

## Record and retrieve Experience

Learning is enabled by default. `learning.enabled` is the ceiling;
`learning.use` and `learning.generate` can be controlled independently beneath it.
This lets a project retrieve old evidence without starting an extractor, or record
new evidence without placing it in foreground prompts.

A successfully completed assistant turn is eligible for automatic extraction when
it contains at least one tool call, artifact, recovered error, explicit correction,
or feedback item. Unfinished, failed, or output-truncated turns and turns with
pending tools are not closed learning sources.

`learning.post_turn.idle_delay_ms` defaults to `0`. The completed turn is captured
as a bounded source snapshot and its extraction job becomes due immediately.
A later live turn does not block work on that already closed snapshot or add new
messages to its input. The worker rechecks the source at admission, claim, heartbeat,
and settlement. A queued source that changed or became unavailable is skipped
before spending a new attempt; losing source or lease authority during execution
prevents a stale result from committing. An explicit positive idle delay retains
the idle-session requirement.

The worker polls every 60 seconds to recover missed work and claims at most two
jobs per wake. A job identity is `(session_id, source_message_id, extractor_version)`,
so retries and restarts do not create a second batch. New closed-turn jobs take
priority over legacy backlog. A process-owned supervisor keeps project learning
alive when an ACP session releases its foreground host, without retaining the
entire foreground runtime. A new process checks up to 64 recent completed turns
from the last seven days for missed admission. Pending work is resumed from SQLite;
a stopped process does not run background work.

`/reflect turn` selects the latest completed assistant turn. `/reflect session`
selects bounded sources across the durable session. Manual reflection becomes due immediately
but still obeys the session generation policy and external-context rule. Manual reflection also keys the exact source-input digest: new feedback or a wider
session selection is a new job, while an identical input remains idempotent.
Admitting a new explicit input revokes older queued/running extraction leases for
the same source message. A retry cannot reuse an older recorded experience to
automatically approve changed or unverified Memory.

The extractor:

- uses `learning.extractor_model` when configured, otherwise the active
  Provider's reachable `small_model`, then the session model;
- receives bounded, redacted source records with one canonical citation ID per
  record and authoritative verification markers; raw storage IDs and digests
  remain in the host's durable source manifest;
- caps input, output and total request time; a malformed JSON answer gets at most
  one repair within the same deadline;
- has no tools, network, filesystem authority, or foreground-session identity;
- persists its exact request and terminal outcome;
- attempts one durable job at most three times.

### Model settings and diagnostics

Learning uses the selected model's resolved request parameters and headers,
including reasoning controls and compatible endpoint-specific options.
It does not impose a temperature. Explicit sampling settings are retained only
when the selected model supports them; unsupported sampling parameters are omitted.
Headers reach the provider without their values being copied into learning receipts.

`learning.execution.max_output_tokens` defaults to `4096`. The effective output
limit is the smallest applicable limit from learning configuration, the model's
declared output capacity, and an explicitly smaller model request limit.
Increasing one limit does not bypass the others. The serialized request is bounded
by `max_input_bytes` (default `131072`), and model work has a total `timeout_ms`
(default `120000`); a JSON repair shares the original deadline.

A provider-level `maxTokens: 0` means no additional configured limit; it does not
disable the learning output ceiling. Native requests lower that ceiling to the
selected API's output-limit field. If the endpoint rejects bounded output for
the selected model, learning reports that failure without silently removing the
ceiling or switching models.

`learning.execution.structured_output` defaults to `false`: the output schema is
included in the prompt and the answer is decoded locally. Enable native structured
output only for a selected endpoint that supports its Chat, Responses, or Messages
JSON-schema format. A provider rejecting an option is a request failure, not proof
that the completed source turn was lost.

The durable request/outcome events carry a request ID, selected provider/model,
effective parameters, prompt digest, and terminal result. Provider failures retain
a bounded, redacted reason and, when supplied upstream, HTTP status, error code,
and provider request ID. `/learn` shows job state, attempt count, next deadline, and
recent errors. Check the job result as well as the provider outcome: a completed
provider response can still fail JSON or evidence validation.

### Inspect and repair learning history

Historical repair works on the current project's legacy extraction jobs.
It preserves old requests, errors, and audit history; it does not rewrite an old
HTTP 400 message into a newly inferred cause.

| Action | Effect |
| --- | --- |
| `/learn status` | Read queue state, recent errors, retrieval selection, current extractor version, and generation availability/policy |
| `/learn repair-history --dry-run` | Preview at most 32 legacy jobs without changing evidence, queues, files, or attempt counts |
| `/learn repair-history` | Revalidate eligible old evidence and queue a versioned extraction where new model work is needed |
| `/learn reprocess <assistant-message-id>` | Select one completed assistant message from this project and admit or reuse its current-version extraction job |

Start with the dry run. Its report distinguishes `would_revalidate`, `would_queue`,
`existing`, `excluded`, and `unavailable` items, with `wouldQueue` and `hasMore`
summary fields. Dry-run counts describe proposed work. Apply with
`/learn repair-history` after inspecting that report.
If `hasMore` is true, a later invocation examines another bounded batch.

Exact old citations can be revalidated against their source addresses, excerpts,
and recorded digests without a paid extraction. Missing or changed evidence stays
unverified. A source digest mismatch is not repaired merely because the old quote
still appears somewhere in the edited text. The service queues fresh extraction
only when the source remains available and eligible.

`reprocess` takes an assistant-message ID, not an Experience ID. An existing
current-version job is returned unchanged, including a completed or failed job;
repeating the command does not reset its attempt counter or repeat a completed
extraction. An already running extraction for that source is reused as well.
A new extractor version can admit one upgrade job while preserving
the old failure. Each new durable job still has the three-attempt ceiling.
Running or uncertain legacy jobs are not rewritten by repair.

These actions retain project ownership, session generation, external-context,
and explicit-forget restrictions. Forgotten sources cannot be re-admitted or
reverified. `status` remains readable when generation or model construction is
unavailable. A repair result saying `queued` means background admission, not
completed learning or accepted Memory: the command does not invoke a foreground
model, insert a foreground input, or resume a Goal. A subsequently claimed
background extraction may make a provider request.

### Evidence validation

Settlement stores accepted Experience and evidence in one transaction. A bad item
with an unresolvable model-visible encoding is refused without discarding clean
siblings; the job result records each `refusedItems` entry. Citations must match a
supplied source address and an exact excerpt, and the source is rechecked before
storage. High confidence alone never authorizes automatic Memory. Maintenance
revalidates source bytes and requires authoritative successful tool evidence or
verified user corrections/preferences, including explicit user records.
Unsupported citations remain unverified observations. Memory-tool results are
not recycled as independent evidence.
Each claimed attempt has a unique lease token and a heartbeat; lost authority or
session exclusion prevents automatic Memory commits.

Automatic retrieval is project-first. It defaults to five records within a
1,200-token rendered context budget. Every inserted item carries its durable id,
source, content, and digest, and the exact section is stored in the prompt receipt.
If the smallest match cannot fit, Zuno retrieves nothing and emits
`learning.retrieval_skipped` with the required and configured token counts.

Use the read-only `experience_search` tool for a deeper explicit search:

```text
experience_search(query: "sqlite migration preserved messages", limit: 10, match: "any")
```

Search input is quoted and bounded before FTS5 evaluation, so punctuation and FTS
operators remain data rather than query grammar. Natural queries recall any meaningful
term and rerank candidates; `match: "all"` requires every term. Chinese/Japanese/Korean
text uses a trigram index with a bounded fallback for short terms. Migrations build
the indexes, writes maintain them through triggers, and searches remain read-only.
Results include source citations and verification state.

`/learn` shows queue counts, pending deadlines, recent errors and the latest recall
selection or skip reason. `/learn list [offset]` pages through Experience (100 per
page). Selecting context for a foreground turn records its ids and use counts;
ordinary searches do not change those counts. `/learn get <experience-id>` shows
its citations, validation state and usage details.

## Turn repeated evidence into a Skill

The background pattern miner semantically groups verified, promotable project
Experience, including different wording of the same rule. By default it
requires at least three new records and three independent sessions before creating
an automatic project Skill candidate. Cross-project aggregation requires the same
rule supported by promoted patterns in at least two projects. Model-proposed groups
must cite known evidence ids; repeated unchanged evidence preserves an already
promoted pattern and its revision. An explicit
`/learn promote <experience-id>` may create a one-evidence project pattern, but it
does not bypass review or evaluation.

A Skill candidate contains the complete proposed file, a unified diff, learned
rules, Experience ids, source identity and digest, target path, and operation.
Built-in or read-only Skills are never overwritten; Zuno proposes a distinctly
named project companion.

The safe path is deliberately split:

1. `/learn skill-review <candidate-id>` explicitly starts an immutable offline cassette suite.
2. Baseline and candidate each execute a bounded model attempt. Tools can return
   only exact matching recorded calls; unknown calls never reach the filesystem or network.
   The grader sees the actual answer and call trace. Expected answers are withheld
   from the task attempt. Both variants use the same model and execution budgets.
3. The candidate passes only if cited failures are fixed, no protection case has a
   critical regression, and the weighted metric does not decrease.
4. Passing changes the candidate to `approved`; it does not write a file.
5. `/learn skill-apply <candidate-id>` performs a separate digest-checked apply.

Opening a session only binds the evaluator; it does not start Skill evaluation.
No additional model setting is required: review uses the same resolved learning
model described above. An explicit cross-provider model is reported unavailable,
not silently substituted. Skill review produces extra model requests. This review
mechanism uses recorded tool I/O; model generation still calls the configured
provider API.

Each evaluation has a total deadline and an ownership token. Another session's
startup cannot cancel a live evaluation; cancellation or expiry preserves a
terminal diagnostic, and an old attempt cannot settle a newer review.

Source drift makes a candidate `stale`. Apply and undo store before/after snapshots;
cooperating writers and the reconciler share an OS path lock. A restart classifies the observed filesystem state and never replays an uncertain
effect. Use `skill-undo` for an applied candidate and `skill-reject` for one you do
not want.

## Feedback, forgetting, and retention

Feedback targets a persisted assistant Message and uses an expected revision.
Revision `0` means no feedback may exist; later writes must match the current
revision. A stale write returns a conflict instead of replacing a newer opinion.

`/learn forget <experience-id>` marks evidence forgotten and atomically retracts
resident entries that lose all their sources. It does not restore the old side
of a corrected entry. Other independent support, imported notes and direct user
reaffirmations are preserved. Explicit forget/undo decisions cannot be
automatically resurrected from unchanged evidence.

Source edits/deletion also invalidate derived recall immediately, even before
maintenance cleans up its projection. Disabling future generation does not
invalidate existing memory. Skill revocations remain separately reviewed because
they change executable methods.

Transcript retention removes session-owned feedback and pending learning jobs when
it deletes that transcript. Project Experience, Memory, patterns, evaluation
results, and Skill candidates retain their audit rows unless the user explicitly
chooses derived learning cleanup; automatic Memory whose original sources are
gone is withheld from recall. See [Session retention](/session-retention) before destructive
maintenance.

## Configuration entry point

The exact fields and defaults live in the
[configuration reference](/reference/configuration#resident-memory-and-user-learning).
A minimal override can separate use from generation:

```json
{
  "memory": {
    "promotion": "automatic"
  },
  "learning": {
    "use": true,
    "generate": false
  }
}
```

The retired `memory.reflection` and `memory.nudge_interval` keys are rejected.
Post-turn extraction belongs to `learning`.

## Where resident Memory lives

| Scope | File | Default cap |
| --- | --- | --- |
| Global agent notes | `$CONFIG/memory/MEMORY.md` | 2200 characters |
| Project rules | `<worktree>/.zuno/RULES.md` | 3000 characters |

`zuno debug paths` prints both resolved paths and marks a store `(absent)` when it does
not exist yet. Absent is the ordinary state, not a fault: a store is created by its
first accepted write and never at startup, so a fresh install has no `memory/`
directory and an empty scope adds no bytes to the prompt.

Both caps are configurable, as `memory.global_char_limit` and
`memory.project_char_limit`, and a refused write names the key that would raise the one
it crossed. They are counted in Unicode scalar values rather than in bytes or tokens —
bytes would charge the same rule three times as much for being written in Chinese, and
a token cap would move under content already on disk whenever the model changed.

The global cap is the smaller of the two deliberately: those notes ride in *every*
session's prompt, including sessions in repositories that have nothing to do with them,
while project rules are only ever loaded by the one repository that pays for them.

## See also

- [Goals, plans and todos](/guide/durable-state)
- [Tools](/guide/tools)
- [Sessions and turns](/guide/sessions)
- [Resident Memory design](/design/memory-learning)
- [User learning flywheel design](/design/user-learning-flywheel)
