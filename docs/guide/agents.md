# Agents

An agent is a contract: a prompt, a model route, an exact tool surface, permission rules,
and a delegation boundary. Selecting an agent is how you choose both what work gets done
and what authority is available to do it.

The direction matters. An agent contract can only *narrow* authority. It cannot widen it,
which is what makes a read-only agent a guarantee rather than a default that
configuration can quietly reverse.

## The built-in roster

| Agent | Responsibility | Delegation |
| --- | --- | --- |
| `orchestrator` | Own the outcome, partition work, integrate results, verify completion | May delegate |
| `build` | Direct end-to-end implementation in one lane | No child tools |
| `plan` | Read-only research and implementation-ready planning | No child tools |
| `review` | Read-only high-assurance review: records checkable evidence and decides draft or ready | May delegate the review seats |
| `deep` | Establish evidence, test competing causes, fix the root when authorized, verify recovery | Bounded tasks within runtime authority |
| `fixer` | Focused local change and its regression scope | No recursive delegation |
| `general` | Bounded work with no narrower specialist | No recursive delegation |
| `explorer` | Read-only repository and call-chain research | No recursive delegation |
| `librarian` | Current external documentation and upstream research | No recursive delegation |
| `oracle` | Read-only architecture and root-cause review | No recursive delegation |
| `looker` | Visual artifact inspection | No recursive delegation |
| `compaction` | Preserve the task, constraints, evidence, and unfinished work in a context checkpoint | Hidden; no tools |
| `title` | Name the session from its subject in the user's language | Hidden; no tools |
| `summary` | Condense the session's outcome and outstanding user request | Hidden; no tools |
| `council-synth` | Synthesize supplied Council evidence while preserving attribution and dissent | Hidden; no tools |

All 15 roles remain native. `orchestrator` is the default; it and `deep` declare the
general `task` delegation tool. Actual delegation depends on the current tool surface,
parent authority, configured depth, and per-Agent restrictions. `build` keeps work
directly in one lane without child tools. `deep` has mode `all`, so it can be selected
as the session Agent or assigned a bounded objective by a delegating Agent.

The native `review_open` operation automatically seats the
`balanced-review` Council over `explorer`, `librarian` and `oracle`; `council_run` is
not exposed to the review model, so it cannot switch presets or bypass the review
binding. A read-only role cannot reach a writing child.
`review` cannot seat another `review`, update Plan/Todo state, or forge source and
receipt fields. Council seat output is parsed and imported by the runtime before
synthesis.

`deep` can read, create, update, and request input for the current durable Goal, so a
directly selected deep-work session can close the same evidence-gated objective it
implements. Goal ownership and delegation are separate capabilities.

## Verification and test-first changes

Working built-in Agents share one verification rubric for implementation, fixes,
testing, planning, and review:

1. For an authorized fix to a reproducible bug or a state-machine change, first add or
   extend a focused behavior test and run it against the old implementation.
2. Check that the red result demonstrates the intended behavioral failure. A build,
   dependency, permission, or environment error is not a reproduced regression.
3. Implement the change, rerun the same test to green, and run the relevant regression
   checks. Cover observable inputs, outputs, transitions, and applicable interruption,
   restart, or recovery paths.

Record exact commands, working directory, tested source and inputs, expected and
observed results, exit status, and authoritative test output or artifact/run receipts.
Separate completed checks from proposed, blocked, and unrun checks. If reproduction
is unavailable, explain why and report the strongest verification that was feasible.

Read-only roles gather existing reproduction steps, tests, and receipts without
editing files or running commands that write. They hand missing tests to an authorized
writer. Reviewers check the red/green evidence and name gaps; a plan specifies the
test and expected failure without claiming the test ran.

Documentation, trivial changes, and command/script deliverables use proportional
checks; not every command needs a new test. Source-string assertions alone do not
establish runtime behavior. Prompt-output contract tests verify the rendered prompt,
not whether a model follows it or whether the described runtime behavior works.

Keep serial CI waits on the critical path in the same foreground workflow, with one
polling owner. Background work is for independent parallel work or an explicit user
request. A polling timeout does not establish a remote failure; inspect the authoritative
run status before reporting its outcome.

This is built-in prompt guidance, with no new runtime gate, approval mechanism, or
authority. Explicit prompt overrides remain authoritative, and existing role and
delegation boundaries still apply. Hidden tool-free roles keep their output contracts.

The design reference is Codex `eaa8b6d917`: `codex-rs/models-manager/prompt.md`
("Validating your work"), `codex-rs/prompts/src/review_request.rs::REVIEW_PROMPT`
with `codex-rs/prompts/templates/review/rubric.md`, and
`codex-rs/core/tests/suite/prompt_caching.rs::prompt_tools_are_consistent_across_requests`.
Zuno adopts focused validation and checkable findings, adapting prompt composition
to one rubric shared by its working roles. The required old-implementation red/green
sequence for reproducible bugs and state-machine changes is a stricter, user-chosen
Zuno policy. It is not a claim that Codex enforces a runtime test-first gate.

## Deep work

The native `deep` definition requires the first-party `deepwork` and
`verification-planning` Skills at the start of each turn. Its method is:

1. Establish evidence and a reproducible baseline from the actual code, tests, logs,
   durable state, and authoritative sources.
2. Rank competing hypotheses and trace the causal chain through callers, state
   transitions, cleanup, and error paths.
3. Choose an experiment whose expected observations distinguish the plausible causes.
   Change one causal variable, inspect the result, and revise the hypotheses.
4. When authorized, fix the owning abstraction and update affected callers.
5. Verify the original failure, corrected behavior, and the relevant interruption,
   restart, or recovery path. Report checks actually run and remaining uncertainty.

An explanation or diagnosis request ends with the answer or demonstrated cause; selecting
a writing role does not authorize an unrequested fix. Authorized commands and scripts
are valid work, including when the deliverable is an operational action rather than a
source change. They remain subject to runtime permissions.

Deep may delegate bounded evidence or implementation tasks when specialization helps.
It retains causal reasoning, integration, and independent verification of child reports.
The runtime controls delegation limits; the prompt does not prescribe a fanout count.

## Choosing one

```sh
zuno run --agent plan "why does the retry budget start before the first attempt?"
zuno run --agent build "add pagination to the /users endpoint and run the tests"
zuno run --agent deep "the compaction boundary drops the tail on resume; find the root cause"
zuno tui --agent orchestrator
```

A practical rule:

| Situation | Agent |
| --- | --- |
| You want an answer or a plan, and no writes | `plan` |
| A plan or design that has to be judged ready to implement | `review` |
| A well-scoped change in one area | `build` |
| A local fix plus its regression scope | `fixer` |
| A hard cross-cutting problem | `deep` |
| Work that fans out across independent pieces | `orchestrator` |
| Read-only code archaeology | `explorer` |
| Current external documentation | `librarian` |

Selection resolves in order: an agent explicitly selected by the client, then top-level
`default_agent`, then built-in `orchestrator`.

## Tool fallback

Every Agent receives a `runtime.execution` fallback rule derived from its final
provider-visible tool snapshot. A rate-limited, unavailable, or transiently failing
tool is not repeated unchanged. When `tool_search` is visible, the Agent uses it to
discover another already-authorized connected tool; a connected `google_search` is one
possible alternative to `web_search`. Custom Agents receive the same rule when they
resolve the same tools.

When Shell is available, the Agent prefers `gh` for GitHub and `rg` for repository search
over raw `curl` or a hand-written directory walk.
It must verify availability and preserve the same source and evidence requirements.
Fallback never grants a tool, network path, filesystem capability, or permission the
Agent did not already have, and no Shell or connected-tool guidance is rendered when
that capability is absent.

## How a contract narrows authority

Four layers apply, and every one of them can only remove capability:

1. The parent Attempt's actual provider-visible tool schemas, for a delegated turn.
2. The target agent role and its extension-tool inheritance policy.
3. The agent's exact `tools` allowlist, when configured.
4. Effective user and agent permission rules.

An `allow` cannot restore a tool that was absent from the parent Attempt, and
`permission.mode: "allow_all"` suppresses prompts without widening this intersection.
Schema identity counts: a same-named tool with a different provider-visible schema is
outside the bound.

The sandbox follows the same one-way rule. A read-only agent receives `read-only`
confinement even when the invocation selected `workspace-write` or
`danger-full-access`:

```sh
# Require OS read-only confinement; refuse if unavailable.
zuno run --agent plan --sandbox-backend auto "audit the retry policy"
```

That guarantee is OS-enforced only where a confined backend runs. Under a trusted
`sandbox.backend: native` selection or the Windows/macOS platform-native default,
the same `read-only` request is recorded but not OS-enforced, and
"read-only" is then a role boundary made of the tool allowlist, the permission rules,
and the Shell risk gate rather than an OS boundary.

## Read-only is a role boundary, not just a sandbox mode

`explorer` is read-only by role, not merely by sandbox mode. Its default surface is
`read`, `glob`, `grep`, read-only `lsp`, `skill`, `report_write`, and `shell` with `bg` under the
read-only filesystem policy a read-only role always receives; edits, delegation, `job`,
and network research are denied. So `du`, `stat`, and `file` are available for evidence,
while workspace edits and Shell writes are refused below the prompt.

Read-only investigation can still produce report files. `report_write` is a separate
host-managed capability: it writes immutable artifacts under `.zuno/reports/` and
returns their exact paths and SHA-256 receipts. The parent can consume those files or,
with its own edit authority, copy them to a requested destination. The child's report
metadata carries the artifact receipts for both foreground and background tasks.
This does not open `.zuno` configuration, extensions, project sources, or Shell writes.

The parent Attempt must expose `report_write`, and a configured tool allowlist or
permission rule may still remove it. Without the capability, the child returns report
text for the parent to save. A child Shell error saying `Read-only file system` describes
that attempt's sandbox contract; it does not prove the host disk or parent is read-only.

Every role that may run a command may also inspect what it started. `bg` is granted
wherever `shell` is, including the read-only roles: a background execution is reachable
only through `bg`, and so is a result too large to return in the transcript.

Global `permission.mode: "allow_all"` skips ordinary confirmation but does not erase
those explicit denies. When external research or a change is needed, delegate to the role
that owns it — `librarian` for evidence outside this repository, `deep` or `general` for
an edit — or do the work in the parent session.

## Plan mode

`/plan` and `/start-plan` enter Plan collaboration mode idempotently, and the restriction is
enforced below the prompt by a deny-by-default capability overlay: repository inspection,
read-only LSP and search, external research, questions, Skills, background inspection,
typed Goal/Plan/Todo operations, and host-managed reports are allowed, while workspace file mutation, delegation, `job`,
and `execute` are denied. `shell` stays available under the read-only sandbox the role
receives, so a command can gather evidence but cannot change the tree.

Returning to Work mode requires a durable plan to exist, and the confirmation names its
title, revision, and completed-step count. The model can recommend starting work but
cannot select it for you. A confirmed selection is persisted as the session agent, so
`--continue`, `--session`, the `/session` picker, and ACP `session/load` all restore the
mode, together with the model and reasoning level the session last ran with.

When the built-in `plan` Agent completes its answer, the current durable Plan and Todos
are the handoff to Start Work. Their execution statuses remain intact and do not trigger
automatic execution-reconciliation turns or overwrite the completed planning answer.
An active Job still prevents handoff; the Plan Agent cannot use `job` in the built-in
profile.

## Inspecting what an agent actually resolves to

```sh
zuno agent list
zuno debug agent explorer
zuno debug permissions
```

`debug agent` reports the effective agent-filtered view, including metadata and
selected-body Skill budgets, rendered and omitted coverage, and a bounded preview.
`debug permissions` reports both the configured and the effective permission mode. Use
these rather than inferring the result from configuration, because global and project
definitions overlap.

`GET /api/agent` uses the same resolved catalog order, base prompts, and native role
permission overlays. Config and Markdown prompt overrides are preserved, including
overrides of hidden native roles; the explicit environment config layer still wins.
Permission rules are ordered as common defaults, native role policy, global configuration,
and resolved per-Agent configuration. The endpoint describes catalog policy before
runtime tool filtering and inherited attempt authority; it does not replace the effective
view shown by the debug commands.

## Custom agents

An agent is defined either in `zuno.json` under `agents.<name>` or as a Markdown file
with frontmatter under `.zuno/agent/`. The file is the definition: Zuno has no command
that writes one for you. Author `.zuno/agent/reviewer.md` yourself, then read the
resolved definition back with `zuno agent list`.

```markdown
---
description: Review diffs for regressions
mode: subagent
model: openai/gpt-5
---

Review the diff for regressions and report each one with its file and line.
```

A configured or extension agent whose mode is `subagent` or `all` can join the delegation
roster. A `primary`-only agent cannot be delegated to. The complete field list is in
[Custom agents](/config/custom-agents), and delegation mechanics are in
[Orchestration](/orchestration).
To package an Agent with Skills or tools, or to implement WASI/native behavior,
see [Developing agents and extensions](/guide/extension-development).

## See also

- [Custom agents](/config/custom-agents)
- [Tools](/guide/tools)
- [Permissions and sandboxing](/guide/permissions)
- [Orchestration](/orchestration)
- [Developing agents and extensions](/guide/extension-development)
