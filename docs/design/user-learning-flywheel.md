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

## Component boundaries

The implementation is split between two native crates and typed database stores:

| owner | responsibility |
| --- | --- |
| `FeedbackService` | revisioned feedback for one persisted assistant message |
| `ExperienceService` | extraction settlement, manual records, Memory proposals, and evidence cleanup |
| `LearningExtractor` | no-tools structured extraction through a dedicated model |
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

`learning.enabled` is the master switch. Under it, `learning.use` and
`learning.generate` resolve independently:

- use without generate retrieves existing Experience without starting an
  extractor provider;
- generate without use records new evidence without injecting it into foreground
  prompts;
- `post_turn.enabled` controls only automatic extraction;
- a session's durable `/memories` policy can narrow both use and generation
  without changing global configuration.

Projection and review remain readable when generation is disabled. Cleanup,
rejection, undo, and forgetting never require an extractor model.

## Fast path: record after a useful task

A completed turn is eligible when it includes at least one of:

- a tool call;
- a produced artifact;
- recovery from an error;
- an explicit user correction;
- explicit positive or negative feedback.

The runtime serializes the replayed durable turn, redacts the active credential
and sensitive process-environment values, and admits an extraction job before
invoking the extractor. The original user Message remains unchanged. The unique identity is
`(session_id, source_message_id, extractor_version)`. Retrying admission, losing a
worker lease, or restarting the process therefore returns to the same job and
cannot create a second experience batch.

Automatic jobs do not run in the foreground completion path. By default they
become due after six idle hours; a host-owned worker polls every 60 seconds and
claims at most two jobs per wake. Claiming and the idle decision share one SQLite
write transaction. A newer session activity timestamp, queued/steering/promoted
input, a process-local live-turn lease, or a disabled/excluded session policy
prevents the claim without spending an attempt. Manual `/reflect` is made
immediately due, but still respects the session generation policy.

When `post_turn.disable_on_external_context` is true, Web and MCP tools mark
their successful results with a durable typed metadata bit. A completed turn
that consumed that context moves the session to `generation=excluded` and skips
its queued automatic extraction. The classifier reads stored tool metadata, not
tool narration or a mutable registry guess.

The extractor has no tools, network capability, filesystem capability, or
foreground-session identity. Its request and terminal outcome are durable
`learning.extraction.request` and `learning.extraction.outcome` events. The request
records the exact prompt, digest, model, structured response contract, and an
empty tool list.

Extraction settlement inserts all `experience_record` and
`experience_evidence` rows and completes the leased job in one transaction.
`unresolved_issue` is durable evidence, but SQL and service validation prevent it
from becoming Memory, pattern evidence, or Skill evaluation evidence.

The extractor may propose resident Memory. Zuno first creates an ordinary
reviewable `MemoryCandidate`. Only a project-scoped proposal with confidence at
or above `0.9` may be applied automatically by this learning path. Global
proposals and lower-confidence project proposals remain pending even when the
general Memory promotion policy is more permissive.

`/reflect turn` and `/reflect session` use the same durable admission and
idempotency path. They do not bypass extraction provenance or promotion policy.

## Slow path: consolidate repeated evidence

A host-owned periodic task checks the project interval while the profile is
mounted. The same check runs after successful extraction and on restart recovery.
Jobs use interval buckets plus durable evidence identities, so several hosts may
check concurrently without duplicating work.

Project aggregation:

- defaults to one 24-hour bucket;
- skips when fewer than three new project experiences exist in the window;
- groups only promotable experiences;
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
- requires the same pattern in at least two independent projects;
- includes a digest of the promoted project evidence in the job identity, so new
  evidence can be checked without replaying an unchanged proposal;
- produces a global pattern, not a writable global Skill.

Explicit promotion of an eligible global pattern creates a project companion
Skill for the current project. Its evaluation evidence is the flattened set of
the cited project experiences.

A rejected pattern stores the evidence version and digest. The same evidence is
suppressed on later runs; additional evidence reopens the pattern for review.

## Retrieval and prompt receipts

Automatic retrieval is project-first and defaults to five records within a
1,200-token context budget. The SQLite provider uses FTS5 over title, summary,
and resolution. The `experience_search` tool exposes explicit deeper search
without changing the default prompt budget.

Retrieved experience enters the prompt as the stable
`learning.experiences` section. Each item carries its durable Experience id,
source identity, content, and SHA-256 digest. Prompt assembly persists the exact
post-hook section in the normal prompt receipt before the provider request.
Replaying the receipt therefore reconstructs every model-visible learned item
without consulting the current FTS index.

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

Resident Memory keeps its own fence. A candidate extracted from an experience is
proposed through the normal review path, so the injection and exfiltration
pattern scan still runs on the exact text that would be written to the resident
file; a hit rejects that one candidate, is reported as `memories.proposal`, and
the experience it came from is still stored.

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

Human review starts an immutable offline suite. The baseline and candidate use
the same model, toolset digest, output budget, step budget, temperature, seed,
and `AttemptSnapshot`. Tool responses come only from recorded cassettes; the
evaluation runner exposes no real side-effecting tool.

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

No stale source is overwritten. After process loss, reconciliation reads the
authoritative filesystem:

- exact `after` means the apply completed;
- exact `before` means it did not;
- any third state is `uncertain`.

Reconciliation classifies the observed state and never mechanically replays the
filesystem effect.

Applied Skills and Memory are not silently removed when evidence disappears.
Deleting a source session or using `/learn forget` first creates pending-review
revocation candidates and rejects pending candidates that cite the removed
evidence. Experience rows become `forgotten` but remain durable for the review
record.

## Client contract

`LearningStateProjection` contains current feedback, experiences, patterns, and
Skill candidates. It is loaded from durable stores even when extraction is
disabled or the extractor model cannot start.

- TUI exposes `/learn`, `/reflect`, the learning sidebar summary, and an explicit
  keep-versus-clean choice during session deletion.
- Server exposes `GET /api/session/{sessionID}/learning` and publishes
  `learning.state.changed`.
- ACP includes the same projection in `_meta.zuno.learning` for replay and live
  updates. `session/delete` requires an explicit
  `cleanupDerivedExperiences` boolean.

`/memory` remains the resident Memory review surface. `/memories` controls use
and generation for the current session. `/learn` owns evidence,
patterns, feedback, Skill review, evaluation, apply, and undo.

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

Schema format 9 adds `session_memory_policy`. The format-8 to format-9 migration
creates an empty sidecar table without rewriting sessions or learning records.
New sessions freeze their configuration default when materialized; child
sessions inherit their parent's effective policy in the same transaction that
admits the child job.

Destructive transcript retention explicitly removes session-owned feedback and
learning jobs. Experience, Memory, patterns, evaluations, and Skill candidates
are project learning and survive transcript retention; nullable session/message
provenance is detached by foreign-key policy.

## Configuration migration

Learning is off by default and requires an explicit extractor model:

```json
{
  "learning": {
    "enabled": true,
    "use": true,
    "generate": true,
    "extractor_model": "provider/model",
    "post_turn": {
      "enabled": true,
      "idle_delay_ms": 21600000,
      "poll_interval_ms": 60000,
      "max_jobs_per_wake": 2,
      "disable_on_external_context": false
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

`use` and `generate` both default to true only after the `enabled` master switch
is true. The retired `memory.reflection` and `memory.nudge_interval` fields have no
compatibility aliases and are rejected as unknown keys. `small_model` remains an
independent internal-model route; learning uses the explicit
`learning.extractor_model`.

## Non-goals

The learning subsystem does not:

- edit Zuno source or configuration;
- build, deploy, push, merge, or create a pull request;
- grant the extractor tools or ambient authority;
- promote unresolved issues;
- apply a Skill without review and a passing evaluation;
- replay an uncertain side effect;
- silently replace a built-in, read-only, or drifted Skill.
