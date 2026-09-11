# User learning flywheel

Zuno turns durable work evidence into three deliberately different products:

- an `ExperienceRecord` preserves one concrete outcome, problem, correction,
  feedback item, or verified procedure;
- a `MemoryCandidate` proposes a stable fact, preference, or project rule for
  resident prompt memory;
- a `SkillCandidate` proposes a reusable working method and always requires
  human review, offline evaluation, and a separate apply action.

This subsystem learns from how a user works with Zuno. It does not modify Zuno's
source, build or deploy the harness, create pull requests, or silently rewrite a
Skill.

For the operator workflow, use [Memory and learning](/guide/memory-learning).
This design record owns extraction, retrieval, evaluation, and storage details.

## Component boundaries

The implementation is split between two native crates and typed database stores:

| owner | responsibility |
| --- | --- |
| `FeedbackService` | revisioned feedback for one persisted assistant message |
| `ExperienceService` | extraction settlement, raw memory hints, manual records, and evidence cleanup |
| `LearningIngestion` | bounded closed-turn source snapshots, manual selection, history repair and startup catch-up |
| `LearningModelClient` | provider requests, deadlines, output schemas and request receipts |
| `LearningExtractor` / `PatternConsolidator` | isolated extraction and semantic grouping |
| `MemoryConsolidator` / `MemoryMaintainer` | independent automatic memory consolidation and source invalidation |
| `LearningSupervisor` / `ProjectLearningService` | process ownership and project queue execution |
| `ExperienceRetriever` | project-first SQLite FTS retrieval and prompt budgeting |
| `PatternMiner` | project and cross-project evidence grouping |
| `SkillCandidateService` | candidate rendering, review, evaluation, CAS apply, undo, and revocation |
| `LearningScheduler` | durable idempotent jobs, interval buckets, leases, and restart recovery |
| `EvaluationService` | immutable cassette suites and paired baseline/candidate scoring |
| `LearningProjectionService` | one durable projection for TUI, Server, ACP, and future clients |

`zuno-learning` owns the flywheel policy. `zuno-eval` owns the provider-neutral
evaluation contract. SQLite stores durable records; no client receives a private
learning loop.

## Enablement and session ownership

Learning is enabled by default. `learning.enabled` remains the master switch;
under it, `learning.use` and `learning.generate` resolve independently:

- use without generate retrieves existing Experience without starting an
  extractor provider;
- generate without use records new evidence without injecting it into foreground
  prompts;
- `post_turn.enabled` controls only automatic extraction;
- a session's durable `/memories` policy can narrow both use and generation
  without changing global configuration.

Projection and review remain readable when generation is disabled. Cleanup,
rejection, undo, and forgetting never require an extractor model.

The extractor model is resolved without opening another provider. An explicit
`learning.extractor_model` is authoritative; otherwise the active provider's
reachable `small_model` is preferred, then the active session model.

## Fast path: record after a useful task

A completed turn is eligible when it includes at least one of:

- a tool call;
- a produced artifact;
- recovery from an error;
- an explicit user correction;
- explicit positive or negative feedback.

The runtime collects bounded durable source records, redacts credentials before
clipping, and admits an extraction job before invoking the extractor. Every source
has a part/feedback address, a raw-source digest and a redacted-content digest.
The model-facing projection exposes only one citation `source_id` per record,
equal to the manifest's canonical reference. Raw storage IDs and digests remain
host-owned; the model is not asked to choose between two differently named IDs.
Unknown references still produce unverified observations, never automatic
promotion or an inferred alias.
The extractor's exact admitted subset is persisted on the claimed job before the
provider request; full legacy transcript blobs are never sent alongside it. The original user Message remains unchanged. The unique identity is
`(session_id, source_message_id, extractor_version)`. Retrying admission, losing a
worker lease, or restarting the process therefore returns to the same job and
cannot create a second experience batch.

Automatic model work does not run in the foreground completion path. The default
idle delay is zero: admission captures the completed turn and makes its durable
job immediately due. A process-owned project worker polls every 60 seconds and
claims at most two jobs per wake; explicit wake notifications coalesce on the
existing project worker and create no foreground session input.

`LearningSourceSnapshot` binds the project/session, start and end message IDs,
successful completion timestamp, terminal message digest, and exact bounded
source-manifest digest. A source must end in a normally completed assistant message
with no error or pending tool. A caller-provided marker is not authority:
the database revalidates the closure and every admitted source against stored rows.
The final redacted/clipped manifest is persisted before the provider request.

With the default zero delay, a valid closed-turn snapshot can be claimed and
heartbeated while a subsequent turn in the same session is live. Its source window
does not expand to include the newer messages. Explicit positive delays retain
idle-session checks, including newer activity and pending input. A live turn never
grants an exception to a legacy job that lacks a verified snapshot. Current
closed-turn work is selected before legacy backlog.

Claiming, source validation, session policy, and attempt admission share the
database boundary. At claim, a changed source becomes `skipped` without consuming
a new attempt; disabled/excluded generation and forgotten sources cannot gain a
lease. Manual `/reflect` is immediately due but still respects the source and policy
boundaries.

There is no quota-percentage, daily-token, or currency budget for automatic
learning. Zuno's API-key providers do not expose one common remaining-quota
snapshot. Actual provider rate limits retain their typed `Retry-After`; the idle
delay, eligibility transaction, two-job wake cap, idempotency, and three-attempt
ceiling bound the work, together with the `learning.execution` input/output/step
and total-time limits. A startup pass admits up to 64 missed completed turns from
the last seven days. Legacy queued inputs receive freshly captured source manifests.
Closing an ACP foreground session does not destroy the project worker.

When `post_turn.disable_on_external_context` is true, Web and MCP tools mark
their successful results with a durable typed metadata bit. A completed turn
that consumed that context moves the session to `generation=excluded` and skips
its queued automatic extraction. The classifier reads stored tool metadata, not
tool narration or a mutable registry guess.

The extractor has no tools, network capability, filesystem capability, or
foreground-session identity. Its request and terminal outcome are durable
`learning.extraction.request` and `learning.extraction.outcome` events. The request
records the exact prompt, digest, model, effective request parameters, structured
response contract, and an empty tool list.

`LearningModel` carries the selected model's resolved parameters, request headers,
API surface, and sampling capability. `LearningModelClient` retains reasoning and
supported endpoint options, removes unsupported sampling controls, and never
introduces a fixed temperature. The host clamps the learning output budget to the
model's declared capacity; the client further intersects any smaller explicit
model request limit. Multiple token-limit aliases cannot enlarge that bound.
Request headers are forwarded without persisting their values in request receipts.

Native structured output is opt-in through `learning.execution.structured_output`;
otherwise the same schema is supplied in the prompt and decoded locally.
JSON repair shares the request deadline. Provider errors retain their typed
recovery behavior and bounded, redacted diagnostics: HTTP status, provider error
code, request ID, and reason when available. Job settlement reports validation
failures separately from a provider successfully returning text.

Extraction first atomically records accepted experiences and verified evidence.
It settles the job with source-linked raw Memory hints, without mutating resident
Memory. `(job, ordinal)` makes a resumed attempt idempotent. Only completed
extractions enter the separate memory maintainer.
`unresolved_issue` is durable evidence, but SQL and service validation prevent it
from becoming Memory, pattern evidence, or Skill evaluation evidence.

The no-tools memory maintainer combines current resident entries, verified sources
and user correction signals into one bounded plan. It obeys `memory.promotion`
(`automatic` by default), revalidates source content and commits the plan, candidate
journal, provenance and completion watermark atomically. Successful tool evidence
or verified user corrections/preferences can support project memory; global
preferences require explicit user evidence. Model confidence is not an execution
receipt. It cannot rewrite user-owned entries, resurrect explicitly forgotten text,
or apply executable Skills. See [automatic resident memory](memory-learning.md).

`/reflect turn` and `/reflect session` use the same durable admission and
idempotency path. They do not bypass extraction provenance or promotion policy.

## Historical repair and reprocessing

`LearningIngestion::repair_history` selects at most 32 legacy extraction jobs from
the current project. The batch includes queued, completed, skipped, and failed
records; running and uncertain jobs remain outside repair. A dry run performs the
same source checks without changing evidence flags, job payloads, queue state,
files, or attempt counters.

For a completed old job, exact evidence can be revalidated without another model
request. Source addresses, excerpts, and any recorded source digests must agree.
An edited source cannot regain verification merely because its old excerpt still
appears in the new text. Unavailable and unverifiable evidence stays unverified.
If usable closed source remains but fresh extraction is needed, repair admits a
versioned `LegacyReprocess` job.

`LearningIngestion::reprocess` selects one assistant-message ID from the same
project. It reuses an existing current-version extraction, even when that job has
already completed or exhausted its attempts. A running source extraction is also
reused rather than overlapped. A new extractor version permits one upgrade job;
the old input and failure remain available. Reprocess admission retains the
configured scheduling delay and the source session's generation/privacy policy.

Repair bookkeeping uses a compare-and-set timestamp and records the extractor
version and outcome without replacing the original request or failure. Terminal
repair outcomes prevent the same missing/excluded source or paid failure from
being processed on every scan. Forgotten sources and experiences cannot be
re-admitted or reverified. Independent support and explicit user-owned Memory
remain governed by the existing provenance/retraction rules.

The native command contract is:

| Command | Service boundary |
| --- | --- |
| `/learn status` | Durable projection plus use/generation availability, extractor version, session generation policy, and history batch limit |
| `/learn repair-history --dry-run` | Read-only bounded repair preview |
| `/learn repair-history` | Apply eligible revalidation and admit required background extraction |
| `/learn reprocess <assistant-message-id>` | Admit or return the existing extraction for that exact project-owned source |

The repair report includes `dryRun`, `examined`, `revalidatedExperiences`, `queued`,
`wouldQueue`, `unavailable`, `excluded`, `hasMore`, and per-job actions.
An admission response identifies `queued` or `existing`, job/source/version,
attempt, deadline, and any bounded diagnostic. Commands return native results;
they do not invoke the foreground model, create a foreground input, or resume a
Goal. An admitted background job may subsequently call the provider.

## Slow path: consolidate repeated evidence

A host-owned periodic task checks the project interval while the profile is
mounted. The same check runs after successful extraction and on restart recovery.
Jobs use interval buckets plus durable evidence identities, so several hosts may
check concurrently without duplicating work.

Project aggregation:

- defaults to one 24-hour bucket;
- skips when fewer than three new project experiences exist in the window;
- semantically groups only verified, promotable experiences and validates cited ids;
- records independent supporting sessions;
- creates an automatic Skill candidate only with at least three independent
  sessions;
- limits each pattern and Skill candidate to 15 learned rules.

An explicit `/learn promote <experience-id>` may create a one-evidence project
pattern and companion candidate. It bypasses the automatic evidence-count gate,
not review or evaluation.

Global aggregation:

- defaults to a seven-day bucket;
- mines only promoted project patterns;
- semantically matches rules supported by promoted patterns in at least two independent projects;
- includes a digest of the promoted project evidence in the job identity, so new
  evidence can be checked without replaying an unchanged proposal;
- produces a global pattern, not a writable global Skill.

Explicit promotion of an eligible global pattern creates a project companion
Skill for the current project. Its evaluation evidence is the flattened set of
the cited project experiences.

A rejected pattern stores the evidence version and digest. The same evidence is
suppressed on later runs; additional evidence reopens the pattern for review.
Unchanged rules and evidence preserve promoted status and version. The durable
input identity is separate from the semantic concept chosen by the model.

## Retrieval and prompt receipts

Automatic retrieval is project-first and defaults to five records within a
1,200-token context budget. The SQLite provider uses FTS5 over title, summary,
and resolution. The `experience_search` tool exposes explicit deeper search
without changing the default prompt budget.

Foreground prompt text and explicit search text are converted to bounded quoted
terms before `MATCH`. FTS5 operators, unmatched quotes, column selectors, and
punctuated identifiers therefore remain input data instead of becoming SQL
query grammar or failing the turn. Default recall uses OR terms and reranking;
explicit `match: "all"` uses all terms. Unicode and CJK trigram FTS indexes are
created once by migration and maintained by insert/delete/update triggers. Reads
never rebuild them. A foreground selection records a retrieval snapshot and usage;
ordinary search is pure. Unverified observations are labeled in retrieved context.

Retrieved experience enters the prompt as the stable
`learning.experiences` section. Each item carries its durable Experience id,
source identity, content, and SHA-256 digest. Prompt assembly persists the exact
post-hook section in the normal prompt receipt before the provider request.
Replaying the receipt therefore reconstructs every model-visible learned item
without consulting the current FTS index.

Resident Memory is refreshed at provider-request and compaction safe points, not
only when a foreground turn starts. The host reads the accepted global/project
revisions and the current session use policy, builds bounded dynamic Memory
context, and removes earlier static Memory sections so an old and a new version
cannot appear together. Changes committed by maintenance or another session can
reach the next model request without opening another foreground turn.
Already-sent requests and durable prompt receipts remain unchanged.

The host integration must preserve the complete service chain:

- Completion admits the closed snapshot and wakes the existing project worker.
  Disabled generation or unavailable model construction suspends an older binding.
- The actual learning-model constructor passes the selected model's resolved
  native reasoning/request settings, headers, sampling capability, and output cap.
- The extraction writer calls `extraction_sources_current_on` inside its SQLite
  transaction; a separate earlier preflight cannot fence a concurrent source edit
  or forget operation.
- The request refresher attaches current Memory at each safe point. Worker success
  alone is not proof that the next foreground request used the new revision.
- `/learn` dispatches the native status/repair/reprocess adapters through the
  existing command-result path.

## Untrusted learned text

Everything a reflection writes is untrusted model output, and the boundary has
two halves that are deliberately not the same set.

At write time an extracted experience or Memory field is refused only when its
*encoding* cannot be resolved to what a model will read: the Unicode Tags block
(`U+E0000..=U+E007F`), the Variation Selectors Supplement
(`U+E0100..=U+E01EF`), and the C0/C1 controls other than tab, newline, and
carriage return. A payload re-spelled in the Tags block contains no ASCII `<`,
so no text scan can see it. Nothing else is refused: variation selectors, soft
hyphens, directional marks, and prose that merely mentions `~/.ssh/config`,
`AGENTS.md`, or a quoted injection attempt are all stored, because a record of
an attack is exactly what this subsystem exists to keep.

A refusal is per item, never per batch. The offending entry is skipped, its
clean siblings in the same extraction are stored under their original extractor
ordinals, the job settles `completed`, and the reason is durable in the job's
`result` JSON as `refusedItems`: one object per discard, with the experience
ordinal, the responsible field (`experiences.summary`, `memories.content`,
`memories.old_text`, `memories.reason`, `memories.experience_ordinal`, or
`memories.proposal`), and the detail. Only a failure that makes the whole job
unusable (no durable project, session, or source message, a `memories[]` entry
pointing outside the extractor's own list, or a confidence that is not a
probability) settles the job `failed`. A learning job is attempted at most
three times. The `/reflect` result carries the same `refusedItems`, and the
post-turn extraction worker surfaces each refused entry to the client as a
`warning: learning extraction refused experience …` status line.

Resident Memory keeps its own fence. The separate maintenance plan passes
through the same write validation, so the injection and exfiltration
pattern scan still runs on the exact text that would be written to the resident
file. An invalid plan gets one bounded repair and never partially commits;
the source Experience remains stored independently.

At read time retrieval carries the rest of the boundary. The
`learning.experiences` section escapes `&`, `<`, `>`, and `"`, announces itself
as data rather than instruction, and replaces every invisible or reordering
codepoint with a visible `[U+XXXX]` marker. A marker is evidence only if a
record cannot forge one, so a literal `[` that begins `[U+` in stored text is
emitted as `&#91;`: every `[U+` in a rendered section was inserted by the
renderer.

The reported token cost is measured on the rendered section, after escaping and
marker expansion, so it is never lower than what the prompt actually spends. If
`retrieval_max_context_tokens` is too small to hold the framed section plus its
cheapest matching record, retrieval returns nothing and says so, naming the
configured budget and the token figure that smallest record needs, so a budget
below the floor is a visible diagnostic rather than a project that appears to
have learned nothing. The turn reports that condition once per session as a
warning notice with code `learning.retrieval_skipped`.

## Feedback

Feedback targets only an already persisted assistant message. A write supplies
the expected revision:

- revision `0` requires that no feedback exists;
- a positive revision must match the current row;
- every material change increments the revision;
- a stale revision returns a conflict instead of overwriting the newer value.

The current value lives in `message_feedback`. The same transaction appends a
`learning.feedback.changed` session event, so each change remains auditable even
though clients normally consume the compact current projection.

## Skill candidates and evaluation

A Skill candidate contains:

- the complete proposed `SKILL.md`;
- a unified diff;
- learned rules and durable Experience evidence ids;
- an exact source identity and original source digest;
- a target path and whether that source is writable;
- the candidate operation (`apply` or `revoke`);
- review, evaluation, effect, and reconciliation status.

Built-in and read-only Skills are never overwritten. Zuno proposes a distinctly
named project companion under `.agents/skills/<name>/SKILL.md`.

Only explicit `/learn skill-review` starts an immutable versioned offline suite;
opening a session merely binds the evaluator. Baseline and candidate each run a
bounded multi-step attempt with the same model and budgets. `CassetteDispatcher`
requires exact tool names/arguments and never falls back to real execution. The
grader receives the resulting answer and tool trace; expected answers are hidden
from the task attempt. Observation-only historical suites expose recorded evidence
through a read-only lookup tool instead of fabricating shell results.

The selected learning model is reused; a separate model setting is optional, and
explicit cross-provider configurations are reported unavailable. Model requests
still use the configured provider API. A whole suite has an absolute deadline;
Drop guards persist cancellation, candidate ownership tokens fence late results,
and another session's startup only reconciles expired evaluations.

A candidate passes only when:

- every cited failure case is fixed;
- no protection case has a critical regression;
- the weighted overall metric does not decrease.

Passing evaluation changes the candidate to `approved`; it does not write a
file. Apply is a separate explicit action.

## At-most-once file effects

Before apply or undo, Zuno persists an operation id and exact before/after file
snapshots. Apply then checks the source digest and destination state:

- source drift marks the candidate `stale`;
- an existing read-only companion destination marks the candidate `stale`;
- a writable target must still match the recorded source digest.

Cooperating apply/undo writers and the reconciler share an OS path lock. A busy
writer is skipped by reconciliation. No stale source is overwritten. After process loss, reconciliation reads the
authoritative filesystem:

- exact `after` means the apply completed;
- exact `before` means it did not;
- any third state is `uncertain`.

Reconciliation classifies the observed state and never mechanically replays the
filesystem effect.

Applied Skills still require reviewed revocation when evidence disappears.
Memory is recall data: invalid sources immediately suppress unsupported entries;
explicit `/learn forget` atomically forgets its evidence and retracts unsupported
managed entries, rejecting pending derived candidates. Independent support and
user-owned entries survive. No inverse replacement restores old corrected text.
Experience and mutation rows remain durable for audit.

## Client contract

`LearningStateProjection` contains current feedback, experiences, patterns, and
Skill candidates, queue counts/deadlines/errors, the last retrieval selection,
and an Experience pagination cursor. It is loaded from durable stores even when extraction is
disabled or the extractor model cannot start.

- TUI exposes `/learn`, `/reflect`, the learning sidebar summary, and an explicit
  keep-versus-clean choice during session deletion.
- Server exposes `GET /api/session/{sessionID}/learning` and publishes
  `learning.state.changed`.
- ACP includes the same projection in `_meta.zuno.learning` for replay and live
  updates. `session/delete` requires an explicit
  `cleanupDerivedExperiences` boolean.

`/memory` remains the resident Memory review surface. `/memories` controls use
and generation for the current session. `/learn` owns evidence, bounded history
repair/reprocessing, status, patterns, feedback, Skill review, evaluation, apply,
and undo. Reading status is independent of model construction and generation
availability.

## Storage and migration

Schema format 6 adds:

- `message_feedback`;
- `learning_job`;
- `experience_record` and `experience_evidence`;
- `learning_pattern`;
- `evaluation_suite`, `evaluation_case`, `evaluation_run`, and
  `evaluation_result`;
- `skill_candidate`.

The format-5 to format-6 migration creates the complete schema before advancing
the format marker. Existing project, session, message, and Memory rows are not
rebuilt or copied. Historical `memory_reflection_delivery` and
`memory_reflection_job` rows remain readable as legacy history, but the runtime
does not admit new work through that retired reflection pipeline.

Schema format 11 adds resident Memory documents/revisions, retrieval snapshots,
source-verification and usage columns, extraction/evaluation ownership tokens,
incremental Unicode/trigram FTS and query indexes. Supported formats 5 through 10
upgrade atomically with the marker last. Exact released fixtures preserve rows;
current markers with altered tables, triggers or indexes fail closed.

Format 12 adds candidate base revisions and evidence references, resident-memory
provenance and completed-maintenance watermarks. Formats 5–11 advance atomically;
user-authored entries and older revision history are preserved.

Schema format 9 adds `session_memory_policy`. The format-8 to format-9 migration
creates an empty sidecar table without rewriting sessions or learning records.
New sessions freeze their configuration default when materialized; child
sessions inherit their parent's effective policy in the same transaction that
admits the child job.

Destructive transcript retention explicitly removes session-owned feedback and
learning jobs. Experience, Memory, patterns, evaluations, and Skill candidates
are project learning and survive transcript retention; nullable session/message
provenance is detached by foreign-key policy.

## Configuration

Learning is on by default. The following object shows the tunable defaults; it
does not need to be present to enable learning:

```json
{
  "learning": {
    "post_turn": {
      "enabled": true,
      "idle_delay_ms": 0,
      "poll_interval_ms": 60000,
      "max_jobs_per_wake": 2,
      "disable_on_external_context": false
    },
    "execution": {
      "timeout_ms": 120000,
      "max_input_bytes": 131072,
      "max_output_tokens": 4096,
      "max_steps": 8,
      "structured_output": false
    },
    "aggregation": {
      "interval_ms": 86400000,
      "min_new_records": 3
    },
    "global_promotion": {
      "interval_ms": 604800000,
      "min_projects": 2
    },
    "retrieval": {
      "max_items": 5,
      "max_context_tokens": 1200
    },
    "skill": {
      "min_independent_sessions": 3,
      "max_learned_rules": 15,
      "require_review": true
    }
  }
}
```

`enabled`, `use`, and `generate` default to true. Explicitly disabling the master
switch caps both subordinate capabilities. The retired `memory.reflection` and
`memory.nudge_interval` fields have no compatibility aliases and are rejected as
unknown keys. An explicit `learning.extractor_model` wins; otherwise learning
uses a reachable same-provider `small_model`, then the active session model.
Existing durable session policies are not rewritten when this default changes.

## Source-informed design choices

The Memory implementation was compared with Codex commit
`eaa8b6d91701d6cabe464141facc677e5915fbfc`. The references below name the actual
source paths and callers at that pin; they do not use older README descriptions
as evidence of current scheduling.

| Concern | Pinned Codex source | Zuno decision |
| --- | --- | --- |
| Separate extraction and consolidation | `codex-rs/memories/write/src/start.rs::start_memories_startup_task`; `codex-rs/memories/write/src/phase1.rs::job::run`; `codex-rs/memories/write/src/phase2.rs::run` | Adopt phase separation. Extraction persists Experience/raw hints; independent Memory maintenance commits a bounded managed-data plan. |
| Closed, bounded input and exact consumption | `codex-rs/memories/write/src/phase1.rs::job::sample`; `codex-rs/state/src/runtime/memories.rs::mark_global_phase2_job_succeeded` | Adapt immutable input/consumption principles to closed-turn SQLite snapshots, content digests, leases, and transactional source verification. |
| When work becomes eligible | `codex-rs/app-server/src/request_processors/turn_processor.rs::TurnRequestProcessor::turn_start_inner`; `codex-rs/state/src/runtime/memories.rs::claim_stage1_jobs_for_startup` | Codex kicks an idle sweep of other threads at turn start. Zuno deliberately selects each eligible completed turn, defaults to zero delay, and permits a verified old snapshot while the next turn is busy. |
| Worker authority and model binding | `codex-rs/memories/write/src/runtime.rs::MemoryStartupContext::stage_one_request_context` and `stream_stage_one_prompt`; `codex-rs/memories/write/src/phase2.rs::agent::get_config` | Adopt explicit model settings and detached work identity. Zuno keeps both stages tool-free; it does not copy Codex's default ephemeral phase-two agent with Memory-root write capability. |
| User edits and authoritative storage | `codex-rs/memories/write/src/workspace.rs::memory_workspace_diff`; `codex-rs/memories/write/templates/memories/consolidation.md` | Preserve user authority using Zuno's SQLite revision/CAS journal. External file edits are preserved and explicitly imported, rather than automatically ingested through a Memory Git diff. |
| Forgetting and surviving support | `codex-rs/state/src/runtime/memories.rs::delete_thread_memory` and `mark_thread_memory_mode_polluted`; `codex-rs/memories/write/templates/memories/consolidation.md` | Adopt selective removal of unsupported material and non-resurrection. Zuno retains its durable audit/revision history and does not claim equivalent filesystem erasure. |
| Bounded retries and unchanged inputs | `codex-rs/state/src/runtime/memories.rs::try_claim_stage1_job`; `codex-rs/memories/write/src/phase2.rs::run` | Adapt ownership/backoff and no-op principles to source/extractor-version identities, three storage-enforced attempts, and consumed-input watermarks. |

These are design references, not configuration, storage, protocol, scheduling,
or filesystem compatibility claims. The operator commands and limits above are
Zuno contracts.

## Non-goals

The learning subsystem does not:

- edit Zuno source or configuration;
- build, deploy, push, merge, or create a pull request;
- grant the extractor tools or ambient authority;
- promote unresolved issues;
- apply a Skill without review and a passing evaluation;
- replay an uncertain side effect;
- silently replace a built-in, read-only, or drifted Skill.
