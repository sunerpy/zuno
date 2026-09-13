# Enterprise Skill evaluation

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

Passing evaluation does not apply or install a Skill. Enterprise application,
activation and private-evidence promotion remain separate implementation work;
no placeholder apply endpoint is registered.
