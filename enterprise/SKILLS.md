# Enterprise Skill evaluation and activation

Skill evaluation is explicitly reviewed work on the existing learning Job,
Worker lease and model-accounting framework. It does not start simply because a
session ends and does not borrow private Memory automation consent.

## Model configuration

An immutable Agent definition may set `skillEvaluation` to a `ConfigurationRef`
of an installed completion-only definition in the same workspace. That profile
selects the actual model, credentials and budgets. Its Agent step limit must be
1–8; credentials stay on the Worker. Removing the reference makes Skill routes
unavailable for that definition. No default model is silently selected.

The evaluator runs the baseline and candidate with the same model/attempt
settings and frozen cases. Tools use only recorded responses; no environment,
filesystem, MCP or command executor is supplied. Each attempt produces an answer
and trace, which a separate model request grades against the expected behavior.
Native and distributed evaluation share failure/protection comparison policy.

## Candidate and review API

| Route | Behavior |
| --- | --- |
| `POST /api/v1/jobs/{sourceJob}/skills` | Propose a private candidate bound to an owned completed Job and its installed configuration |
| `GET /api/v1/skills/{candidate}` | Read the exact baseline, proposal, cases, model reference, state and report |
| `POST /api/v1/skills/{candidate}/evaluate` | Approve the exact candidate digest and atomically enqueue evaluation |

The request includes a name, baseline/proposed content and 1–8 fixed cases.
Each case has a stable ID, prompt, expected behavior, recorded tool calls, kind
(`failure`, `protection`, `general`) and weight. These are explicitly supplied
evaluation scenarios, not automatically promoted proof of real-world success.
Each case permits at most 16 recorded calls; candidate JSON is limited to 128 KiB.

Review requires an active human principal using an approved review application.
A proposal alone schedules no model work. Review, Job admission, request receipt
and audit commit atomically. Repeating a request ID reuses its original Job.
Failed/cancelled candidates may receive a new explicit review, which creates a
new Job and retains the earlier run and accounting.

SDK methods are `proposeSkill`, `skillCandidate` and `evaluateSkill`. Existing
learning Job detail and cancellation APIs also handle `skill_evaluation`.
Public activity identifies `SkillEvaluation`; no Worker credentials or private
lease tokens are returned. This change delivers no App/UI.

## Runtime and evidence

Learning protocol 2 adds the Skill input/result variant. PostgreSQL preview
format 26 records private candidates, reviews, audits and requests, while reusing
learning execution leases, deadlines, budgets and model journals. The exact
format-25 migration retains sessions, messages, private/shared Memory and MCP
records.

Every model request is bound to the approved case, baseline/candidate role,
installed model and recorded tool schemas. Request counts, serialized input,
output limits and total accounting are bounded. Worker replacement cannot reset
a Job's charged/reserved tokens. Unknown outcomes are charged conservatively;
truthful delayed model usage may reconcile an existing reservation.

Before admitting a grade request, the data owner reconstructs the attempt trace
from durable model outcomes and the frozen cassette. Its answer, trace and
unmatched-call count must match the grade input. Final scores must match recorded
grade outputs; aggregate pass/fail and metrics are recomputed. A Worker cannot
submit a standalone `passed: true` result without those records.

Step-budget exhaustion produces a failed observation only when the durable
attempts support it. Cancellation, policy revocation and stale leases block new
model requests. Evaluation currently restarts a case sequence after a lost
Worker rather than migrating an in-flight provider stream; the same persisted
Job budget still applies.

## Versioned installation

Passing evaluation never installs or activates a Skill automatically. The owner
must use an approved review application for each installation, activation or
rollback. These operations recheck current organization authorization and use
request IDs plus expected revisions. Each installation retains its exact
candidate digest, body and completed evaluation proof.

| Route | Behavior |
| --- | --- |
| `POST /api/v1/skills/{candidate}/install` | Store evaluated content; new installations use expected revision `0` |
| `GET /api/v1/workspaces/{workspace}/skills` | Page owned installation metadata |
| `GET /api/v1/installed-skills/{skill}` | Read owned content and its current revision |
| `POST /api/v1/installed-skills/{skill}/activation` | Explicitly activate or deactivate the current revision |
| `POST /api/v1/installed-skills/{skill}/rollback` | Restore a prior body and proof as a new, inactive revision |

Installation is keyed by owner, workspace and validated name. Replacing content
requires the evaluated baseline to match the installed body and its current
revision. Every installation or rollback is inactive until separately activated.
Revision history, request receipts and content commit atomically. Up to 64 Skills
may be installed per workspace; each body is limited to 32 KiB. Descriptions are
bounded to 1,200 UTF-8 bytes.

This provider implements native **embedded Skills**. It stores bodies in
PostgreSQL and supplies the existing `Skills` catalog and `skill` tool. Workers
receive a bounded metadata index before each model request; `list`, `search` and
`load` use native identity and pagination rules. Loading checks current ownership,
lease and activation before and after materialization. A source locator includes
the installation revision, so replaced or deactivated sources cannot be loaded.
Previously logged content remains part of durable history; deactivation changes
future discovery and reads.

The internal state service does not scan control-plane host directories.
Embedded Skills have no resource root: `read_resource` reports that limitation,
and package resources/workspace installation and evidence-based proposal
automation remain separate work. Completion profiles expose no Skill tool.

PostgreSQL preview format 27 adds installation, revision and request tables with
forced owner RLS. Its exact format-26 fixture preserves evaluated candidates,
sessions, messages and Memory, including rollback on injected migration failure.
SDK methods are `installSkill`, `installedSkills`, `installedSkill`,
`activateSkill` and `rollbackSkill`. No App/UI is included.
