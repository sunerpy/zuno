# Memory and learning

Zuno keeps reusable information in three different forms. They have different
review and deletion rules; treating them as one store makes it easy to approve the
wrong thing.

| Kind | What it holds | Where it appears | Who can apply it |
| --- | --- | --- | --- |
| Resident Memory | A small global preference or project rule | Stable `memory.global` and `memory.project` prompt sections | A reviewed or policy-approved `MemoryCandidate` |
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

HTTP clients can read and update the policy through
`GET|PUT /api/session/{sessionID}/memory-policy`. Updates include
`expectedRevision`; a stale revision returns `409`. A client may request only
`enabled` or `disabled`. `excluded` is assigned by the host.

When `learning.post_turn.disable_on_external_context` is enabled, a completed turn
that consumed a successful Web or MCP result moves the session to `excluded`.
Zuno reads durable tool metadata for that decision, not text in the transcript.

## Propose and review resident Memory

The model-visible mutation tool is `memory_propose`. It accepts:

- `target`: `global` or `project`;
- `action`: `add`, `replace`, or `remove`;
- the proposed full entry in `content` for add/replace;
- a unique `old_text` locator for replace/remove;
- a durable reason and confidence value.

The tool creates an audited `MemoryCandidate`; it cannot write the resident file.
Validation rejects malformed or ambiguous operations, over-budget output,
prompt-injection patterns, known credential literals, unreadable files, and drift
from the version that was reviewed. Temporary failures, unresolved guesses,
secrets, and task narration are not suitable Memory.

The default promotion policy is `review`. Two optional policies change when a
valid proposal applies:

| `memory.promotion` | Behavior |
| --- | --- |
| `review` | Leave every proposal pending for a user |
| `high_confidence` | Apply proposals at or above `memory.auto_confidence`; retain the rest |
| `automatic` | Apply every proposal that passes the same validation and safety checks |

`memory.auto_confidence` defaults to `0.9`. Learning-generated proposals use a
narrower rule regardless of that setting: only project Memory at confidence
`>= 0.9` may apply automatically. Global and lower-confidence proposals stay
pending.

### Apply and undo recovery

Before changing a file, Zuno stores the exact before and after entry lists and
moves the candidate to `applying`. The resident file is replaced atomically, then
the candidate becomes `applied`. Undo follows the same pattern through `undoing`
and `undone`.

After a process loss, startup compares the current file with both snapshots:

- the expected after state proves apply completed;
- the expected before state proves it did not;
- any third state becomes `uncertain`.

No recovery branch mechanically repeats the file mutation. Inspect an `uncertain`
candidate and the resident file before deciding what to keep. External edits are
not overwritten.

## Record and retrieve Experience

Learning is enabled by default. `learning.enabled` is the ceiling;
`learning.use` and `learning.generate` can be controlled independently beneath it.
This lets a project retrieve old evidence without starting an extractor, or record
new evidence without placing it in foreground prompts.

A completed turn is eligible for automatic extraction when it contains at least
one tool call, artifact, recovered error, explicit correction, or feedback item.
The default scheduler waits for six idle hours, polls every 60 seconds, and claims
at most two jobs per wake. A job identity is
`(session_id, source_message_id, extractor_version)`, so retries and restarts do
not create a second batch.

`/reflect turn` selects the latest completed assistant turn. `/reflect session`
uses the durable session transcript. Manual reflection becomes due immediately
but still obeys the session generation policy and external-context rule.

The extractor:

- uses `learning.extractor_model` when configured, otherwise the active
  Provider's reachable `small_model`, then the session model;
- receives a redacted replay and a structured response schema;
- has no tools, network, filesystem authority, or foreground-session identity;
- persists its exact request and terminal outcome;
- attempts one durable job at most three times.

Settlement stores accepted Experience and evidence in one transaction. A bad item
with an unresolvable model-visible encoding is refused without discarding clean
siblings; the job result records each `refusedItems` entry.

Automatic retrieval is project-first. It defaults to five records within a
1,200-token rendered context budget. Every inserted item carries its durable id,
source, content, and digest, and the exact section is stored in the prompt receipt.
If the smallest match cannot fit, Zuno retrieves nothing and emits
`learning.retrieval_skipped` with the required and configured token counts.

Use the read-only `experience_search` tool for a deeper explicit search:

```text
experience_search(query: "sqlite migration preserved messages", limit: 10)
```

Search input is quoted and bounded before FTS5 evaluation, so punctuation and FTS
operators remain data rather than query grammar.

## Turn repeated evidence into a Skill

The background pattern miner groups promotable project Experience. By default it
requires at least three new records and three independent sessions before creating
an automatic project Skill candidate. Cross-project aggregation requires the same
promoted pattern in at least two projects. An explicit
`/learn promote <experience-id>` may create a one-evidence project pattern, but it
does not bypass review or evaluation.

A Skill candidate contains the complete proposed file, a unified diff, learned
rules, Experience ids, source identity and digest, target path, and operation.
Built-in or read-only Skills are never overwritten; Zuno proposes a distinctly
named project companion.

The safe path is deliberately split:

1. `/learn skill-review <candidate-id>` starts an immutable offline cassette suite.
2. Baseline and candidate use the same model, toolset digest, budgets, temperature,
   seed, and recorded tool responses.
3. The candidate passes only if cited failures are fixed, no protection case has a
   critical regression, and the weighted metric does not decrease.
4. Passing changes the candidate to `approved`; it does not write a file.
5. `/learn skill-apply <candidate-id>` performs a separate digest-checked apply.

Source drift makes a candidate `stale`. Apply and undo store before/after snapshots;
a restart classifies the observed filesystem state and never replays an uncertain
effect. Use `skill-undo` for an applied candidate and `skill-reject` for one you do
not want.

## Feedback, forgetting, and retention

Feedback targets a persisted assistant Message and uses an expected revision.
Revision `0` means no feedback may exist; later writes must match the current
revision. A stale write returns a conflict instead of replacing a newer opinion.

`/learn forget <experience-id>` marks evidence forgotten. Removing source evidence
does not silently remove applied Memory or Skills. Zuno creates reviewable inverse
or revocation candidates and keeps enough evidence for the audit trail.

Transcript retention removes session-owned feedback and pending learning jobs when
it deletes that transcript. Project Experience, Memory, patterns, evaluation
results, and Skill candidates survive unless the user explicitly chooses derived
learning cleanup. See [Session retention](/session-retention) before destructive
maintenance.

## Configuration entry point

The exact fields and defaults live in the
[configuration reference](/reference/configuration#resident-memory-and-user-learning).
A minimal override can separate use from generation:

```json
{
  "memory": {
    "promotion": "review"
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
first approved write and never at startup, so a fresh install has no `memory/`
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
