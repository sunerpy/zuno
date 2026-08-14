# Research: deepseek-ai/deepseek-harness

- **Repo**: https://github.com/deepseek-ai/deepseek-harness
- **Commit pinned for all permalinks**: `47f943859bef60e4160492346772ded9b24f765a`
- **Language**: TypeScript (monorepo, 51 packages, ~85 MB, 7404 files)
- **Licence**: MIT
- **Created**: 2026-08-13 · **Last push**: 2026-08-13 · **Stars**: 57,187 (at time of research, 2026-08-14)
- **Status**: self-declared *developer preview*, "THERE WILL BE COMPATIBILITY-BREAKING CHANGES"
- **Homepage**: https://deepseek.com/harness

## CONCLUSION

**A production agent runtime, not an eval harness. No paper exists.** Written in TypeScript, not
Python — so the policy/mechanism split still applies, but for a different reason: the *ideas* are
almost all dependency-free state machines, and the *substrate* (Cordis DI, Node worker threads, a JS
realm for Code Mode) is what does not port.

**On the question that matters most, it is weaker than Zuno's target design**: `dsh` does not
machine-check completion. `complete` is model self-report, gated by *who may claim it* rather than
*whether it is true*. It spends its considerable rigour on authority, budgets, and fail-closed
error paths instead.

**But it contains one machine-checked gate worth more than the rest of the repo combined**: the
Ralph loop's structural report contract, which deterministically refuses a `complete` claim carrying
no evidence or carrying leftover next-steps. That is a completion gate that needs no test runner and
no acceptance criteria, so it applies on day one and to every task.

**It corroborates the persisted-goal convergence as a fifth independent implementation** — and adds
three refinements none of the other four had: a CAS revision, `blocked` as a first-class terminal
peer with a machine-routable code, and an `armed`/`disarmed` split that keeps autonomous authority
out of persistence so a restart never silently resumes a spending loop.

**It also confirms a refusal**: its plan mode is prompt text with zero enforcement, and its docs say
so outright. A fifth strong team shipping the weaker version is confirmation, not a reason to revisit.

Top three adoptions: **(1)** the structural completion contract [pure], **(2)** monotonic tool
guards where deny is absorbing and "allow" is not representable in the type [pure], **(3)** the
`armed`/`disarmed` activation split with fail-closed disarm and flush-before-continue [pure].

---
## 1. What the harness actually is

**It is a production agent runtime, not an evaluation harness.** The name misleads: "harness" here
means *the thing that wraps a model to make it act*, in the same sense as "agent harness". It is
not a SWE-bench-style benchmarking rig and not an RL training environment.

Evidence:

- `README.md:5` — "DeepSeek Harness (`dsh`) is an open-source agent harness developed by DeepSeek AI.
  It uses an architecture where **everything is a plugin**".
- It ships a **Web UI and a CLI** as the user-facing products: `apps/web`, `apps/cli`; the documented
  entry point is `npx @deepseek-ai/dsh web` serving `http://127.0.0.1:3080` (`README.md:21-27`).
- `BENCHMARK.md` is 231 bytes and contains **no benchmark at all** — it just says "Follow *Get started
  with the Python SDK* … and run the `jsonrpc-agent` minimal variant. Use separate workspaces and
  session IDs for independent benchmark tasks." Benchmarking is an afterthought delegated to the user.
- The package list is a coding-agent feature list, not an eval feature list: `packages/goal`,
  `packages/plan`, `packages/todo`, `packages/subagent`, `packages/compaction`, `packages/lsp`,
  `packages/mcp`, `packages/acp`, `packages/sandbox`, `packages/shell`, `packages/skill`,
  `packages/guard`, `packages/spill`, `packages/workflow`, `packages/schedule`.

### Weight to give its design choices

| Dimension | Value |
|---|---|
| Language | TypeScript (**not** Python — the expected-dependency assumption in the brief is wrong) |
| Size | 51 workspace packages, ~85 MB, 7404 tracked files |
| Licence | MIT (permissive; ideas and even code are legally reusable) |
| Age | Created **2026-08-13**, researched 2026-08-14 — *one day old* |
| Traction | 57,187 stars in ~24h |
| Maturity | Self-declared **developer preview**, "THERE WILL BE COMPATIBILITY-BREAKING CHANGES" |
| Artifact type | Maintained software, corporate-backed, not a research artifact |

The code quality is unusually high for a first public drop: every source file carries a module
doc comment, there is a per-package `invariant.ts` convention, `knip.json` (17 KB) for dead-code
detection, `.oxlintrc.json` (11 KB), `lefthook.yml` pre-commit hooks, and seven separate vitest
configs (unit, e2e, snapshot, web, web-perf, web-stress). This is not a demo. **But it is one day
old**: nothing here has survived contact with long-term maintenance, and the compatibility warning
is explicit. Treat its designs as *well-reasoned proposals by a strong team*, not as
battle-tested conclusions.

Relationship to this project: `dsh` is a **direct architectural sibling** of Zuno's plugin design.
Both are "everything is a plugin" agent runtimes with a governed tool registry and hook system.
That makes it the highest-transferability reference examined so far — and also means much of it is
work Zuno has already done.

---

## 2. The verification and completion model — **THE HEADLINE FINDING**

**`dsh` does NOT machine-check task completion. Completion is model self-report, gated by
*authority* rather than by *evidence*.** This is a principled counterexample to the prior
"machine-checked completion gates" conclusion, and it is worth understanding precisely, because
what `dsh` builds instead is genuinely clever and partly worth stealing.

### 2.1 What `complete` actually does

`packages/goal/tool-goal/src/index.ts:298-305` — the terminal transition:

```ts
const goal = args.action === 'complete'
  ? ctx.goals.complete(execution.agent, ref)
  : ctx.goals.block(execution.agent, ref, {
    code: 'model-reported',
    message: args.blocked_reason as string,
  })
```

No test run. No acceptance-criteria evaluation. No judge model. The *only* checks on `complete` are:

1. **Compare-and-set on the goal revision** (`tool-goal/src/index.ts:145-154`): the model must pass
   the exact `goal_id` and `revision` it read from `get_goal`. A stale revision is rejected with
   `GOAL_TOOL_INVALID_UPDATE`. This defeats a model completing a goal it has not re-read.
2. **Authority** (`tool-goal/src/authority.ts:101-108`): `complete` requires either a direct human
   turn on a root agent, or *the exact currently-admitted goal round*.
3. **Argument hygiene**: `blocked_reason` is rejected on `complete`, required on `blocked`.

"Verify the work" is delegated entirely to **prompt text**
(`packages/goal/goal-round-driver/src/prompt.ts:18-23`):

> "Make concrete progress and verify the result. Before claiming completion, gather evidence that
> the whole objective is achieved, read the current goal, and mark it complete."

And `tool-goal/src/index.ts:118` in the system-prompt section: *"Mark complete only when the
objective is actually achieved."* That is exactly the class of instruction this project's fourteen
review waves concluded is worthless on its own.

**Verdict: on the single question you care most about, `dsh` is weaker than Zuno's target design.
It has no completion gate. Do not adopt its completion model.**

### 2.2 What it builds instead, which IS worth stealing: the authority model

The interesting inversion is that `dsh` spends its rigour on *who may make a claim* rather than
*whether the claim is true*. Three tiers, all machine-enforced at execution time:

`packages/goal/tool-goal/src/authority.ts:19-22`:

```ts
/** Hard authority granted to one state-changing call. */
export type GoalToolAuthority =
  | { readonly kind: 'direct-human' }
  | { readonly kind: 'goal-round'; readonly goal: GoalView }
```

- `create_goal`, `edit`, `pause`, `resume` → **`requireDirectHuman`** only
  (`authority.ts:90-93`). A model may *never* create, re-scope, or re-arm its own goal autonomously.
  `hasDirectHumanInput` (`authority.ts:70-74`) requires a `user/message` whose
  `source.kind === 'user'` inside the current *root-agent* turn — and the doc comment names the
  attack it defends: *"An omitted `Agent.followup()` / `steer()` source resolves to `user`, so
  non-human producers must supply their own source rather than inheriting this authority."*
  Subagents are structurally excluded (`authority.ts:71`: `ctx.agents.roots().includes(...)`).
- `complete`, `blocked` → direct-human **or** the exact matching goal round (`authority.ts:101-108`).
- Every goal tool additionally requires the live calling agent to be *the current initiator* of its
  own driver (`authority.ts:55-61`), rejecting `GOAL_TOOL_DRIVER_REQUIRED` otherwise.

This is the same philosophy as Zuno's "modes as permission rulesets a model cannot argue with",
applied to goal lifecycle. **Strong corroboration of an already-decided principle**, extended to a
place Zuno may not have covered: *the model must not be able to widen or re-arm its own objective.*

### 2.3 The one machine-checked gate: a give-up brake

`dsh` has exactly one deterministic gate on a terminal transition, and it guards `blocked`, not
`complete` (`packages/goal/tool-goal/src/index.ts:290-297`):

```ts
if (args.action === 'blocked' && authority.kind === 'goal-round'
  && authority.goal.roundsStarted < resolved.blockedAfterConsecutiveRounds) {
  throw new HarnessError(
    `blocked requires at least ${resolved.blockedAfterConsecutiveRounds} consecutive goal rounds; `
    + `current round is ${authority.goal.roundsStarted}`,
    'GOAL_TOOL_BLOCK_THRESHOLD',
  )
}
```

Default `blockedAfterConsecutiveRounds = 3` (`tool-goal/src/index.ts:33-35`). The model *cannot*
declare itself blocked before round 3, full stop — the tool call throws.

This is the **mirror image of a livelock breaker**: a livelock breaker stops a model spinning
forever; this stops a model bailing out early. Note it is only enforced under `goal-round`
authority — a human can mark blocked at any time. The prompt policy accompanying it is precise
about what does *not* count (`tool-goal/src/index.ts:120-121`): *"difficulty, uncertainty, or
useful remaining work is not blocked."*

**This is a genuinely novel idea and cheap to adopt.** Zuno has (planned) a livelock breaker for
the spin case; `dsh` shows the premature-surrender case is a distinct failure mode needing its own
deterministic floor.

### 2.4 Failure handling: fail-closed disarm

Where `dsh` *is* rigorous is what happens when anything goes wrong. `GoalActivation` is the key
invention (`packages/goal/goal/src/types.ts:70-71`):

```ts
/** Whether this live process may automatically continue an active goal. */
export type GoalActivation = 'armed' | 'disarmed'
```

**`activation` is process-local and never persisted** (`types.ts:81-82`, `types.ts:85-92`). The
durable phase says the goal is `active`; a separate in-memory bit says whether *this* process may
autonomously continue it. Consequences, all in `goal-round-driver/src/index.ts`:

- On plugin load, **every pre-existing agent is disarmed** (`:416-421`): *"Loading a lifecycle
  driver over existing agents never inherits hidden automatic authority from an earlier producer
  instance."* A restart, resume, or fork therefore never silently resumes an autonomous loop; a
  human must call `update_goal action=resume`, which requires direct-human authority.
- Any driver exception → `disarm` (`:220-223`, `:226-230`, `:236-240`).
- `agent/error` → `disarm` (`:246-249`).
- A turn ending with `reason.kind === 'max-tokens'` → `disarm` (`:317-321`). Context exhaustion
  stops autonomy rather than burning the remaining budget.
- **A failed durability checkpoint → `disarm`** (`:144-152`): the driver calls
  `ctx.sessions.flush(agent.session)` *before* reserving the next round, and if the flush fails it
  refuses to continue. Autonomous rounds can never run ahead of persisted state.
- A competing human prompt arriving mid-flight → the reserved round is dropped and the goal is
  **`pause`d**, not blocked (`:259-277`).

And the reservation fence is checked **twice**, before and after the hook chain, because awaiting
`next()` may have invalidated it (`:349-414`, predicate at `:333-347`):

```ts
/** Fail closed unless the queued prompt still owns the exact live revision. */
function validReservation(...): boolean {
  return ctx.fiber.state === FiberState.ACTIVE
    && !state.stopping && attempt !== undefined && attempt.phase === 'claimed'
    && !attempt.stale && sameQueued(content, source, attempt)
    && goal !== undefined && goal.id === source.goalId && goal.revision === source.revision
    && goal.phase === 'active' && goal.activation === 'armed'
    && source.round === goal.roundsStarted + 1
}
```

**Dependency verdict for §2: [pure] policy throughout.** Authority tiers, CAS revisions, the
armed/disarmed split, disarm-on-error, flush-before-continue, and the blocked-round floor are all
plain state machines over data Zuno already persists. Nothing here needs a crate.

---

## 3. Task specification and decomposition

### 3.1 The goal record — a fifth independent data point, and the most sophisticated one

`packages/goal/goal/src/types.ts:58-68`:

```ts
/** Full durable state written by every non-clear goal mutation. */
export interface GoalSnapshot extends GoalRef {
  /** Human-requested completion objective. */
  readonly objective: string
  /** Durable lifecycle phase. */
  readonly phase: GoalPhase
  /** Present exactly while `phase` is `blocked`. */
  readonly blockedReason?: GoalBlockReason
  /** Total admitted goal-round cap. */
  readonly maxGoalRounds: number
}
```

with `GoalPhase = 'active' | 'paused' | 'blocked' | 'complete'` (`:44-48`), and

```ts
/** Compare-and-set identity for one exact goal revision. */
export interface GoalRef {
  readonly id: GoalId
  /** Positive revision; every durable mutation increments it. */
  readonly revision: number
}
```

**This corroborates the prior finding at full strength and adds three things the other four
implementations did not have:**

1. **A CAS revision on the goal itself.** Every mutation increments it; every model-side mutation
   must echo the exact revision it read. This makes "the model completes a goal whose scope has
   since changed" structurally impossible.
2. **`blocked` as a first-class fifth-state peer of complete**, with a machine-routable
   `GoalBlockReason { code, message }` (`:50-56`). Observed codes in the driver:
   `round-limit`, `queue-failed`, `prompt-rejected`, and from the model path `model-reported`.
   A stable kebab-case code means the *host* can route on why a goal stopped without parsing prose.
3. **The durable/process-local split** (`activation`, §2.4) — persisted intent separated from
   in-process authority to act on it.

It is event-sourced as a session projection with a last-wins whole-value fold
(`packages/goal/goal/src/types.ts:102-111`):

> "Whole-value rule: every goal change carries the complete post-change state, so the fold is
> last-wins."

`GoalView` (`:73-83`) adds derived counters — `roundsStarted`, `createdAt`, `updatedAt`,
`activation` — on top of the snapshot.

**Corroboration verdict: the persisted user-owned goal with status and budget is now confirmed by
FIVE independent teams.** `maxGoalRounds` is exactly the "budget" field. Keep it ranked first.

### 3.2 What is NOT there: no goal template

**There is no objective/constraints/acceptance-criteria/allowed-tools template.** `objective` is a
single free-text `string` (`types.ts:61`). There is no acceptance-criteria field anywhere in the
goal domain, which is the direct cause of the missing completion gate in §2 — there is nothing to
check against. `dsh` is therefore **evidence against** a rich goal template being necessary for a
working goal loop, and **evidence for** the claim that without acceptance criteria you cannot have
a completion gate. Both directions are useful.

Constraints and allowed-tools live elsewhere entirely, in the permission/guard layer — the same
separation Zuno already made.

---

## 4. The paper — **there isn't one**

**There is no paper for `deepseek-ai/deepseek-harness`.** Verified three ways:

1. `grep -rn "arxiv"` across every `.md` in the checkout returns **nothing**.
2. The only paper the README references is for the *underlying framework*, not the agent design
   (`README.md:7`): "powered by [Cordis](https://github.com/cordiverse/cordis), whose design is
   described in *A Programming Paradigm for Spatiotemporal Composability*". That paper is about a
   plugin/DI system's composability model — not about agent loops, verification, or task
   specification.
3. Web search surfaces no DeepSeek-authored harness paper. The repo went public 2026-08-13
   alongside V4-Pro; coverage is press, not research (e.g. Sina Finance 2026-08-13:
   "DeepSeek 正式开源 Harness").

### Name-collision warning

Searching "deepseek-harness paper" returns **three unrelated projects** that are *not* DeepSeek's,
and one genuinely relevant third-party paper. Do not conflate them:

| Thing | What it actually is | Relevance |
|---|---|---|
| `deepseek-ai/deepseek-harness` | **This repo.** TS agent runtime. No paper. | The subject |
| `HenryZ838978/deepseek-harness` | Third-party Python lib auditing DeepSeek's *wire protocol* (16 quirks, 12 probes). Has a "technical report". Also ships a CLI called `dsh`. | None — it is an API-compatibility shim |
| `zjukop/deepseek-harness` | 0-star single-author Python repo, 20 minutes of commits, README claims to beat Claude/Codex | None |
| arXiv 2604.25850, "Agentic Harness Engineering" (2026-04-28) | Independent academic work on auto-evolving coding-agent harnesses | **Genuinely useful; see §4.2** |

### 4.1 Paper-versus-code check: not applicable, but the *docs* oversell in one place

With no paper there is no claim to falsify. The `docs/` tree (60+ files, bilingual) is unusually
honest — it even ships `docs/postmortem/`. One gap worth naming: press coverage and the docs
describe "plan, goal, todo and background task mechanisms" for long tasks, which invites the reading
that the harness *ensures* long tasks complete correctly. It does not. As §2 establishes, the
completion decision is the model's, and the only thing the harness guarantees is *who* is allowed to
make it and *how many rounds* it gets. That is a real limitation, not a documented one.

### 4.2 The paper you actually want is somebody else's

arXiv **2604.25850**, *Agentic Harness Engineering: Observability-Driven Automatic Evolution of
Coding-Agent Harnesses* (Lin, Liu, Pan, et al., 2026-04-28, preprint, 1 citation) is far more
on-target for this project's question than anything DeepSeek published, and it contains one result
that should directly influence Zuno's sequencing:

> "Ablations further localize the gain to **tools, middleware, and long-term memory rather than the
> system prompt**." … "the system prompt alone **regresses**, indicating that factual harness
> structure transfers across tasks and models whereas **prose-level strategy does not**."

Measured, on Terminal-Bench 2, pass@1 69.7% → 77.0% over ten iterations, transferring to
SWE-bench-verified and three other model families (+5.1 to +10.1 pp).

**This is empirical support for a decision this project already made**: encoding policy as
mechanism (permission rulesets, typed registries, deterministic gates) beats encoding it as prompt
text. It is also a direct rebuke of `dsh`'s choice to put "verify before claiming complete" in
prose. Also relevant: their self-modification constraints — the evolving agent may not touch "the
runs directory, tracer, verifier, and LLM configuration", explicitly "to block the shortcuts an
unconstrained self-modifier would take, such as **disabling the verifier**". Same instinct as
Zuno's compatibility oracle.

I did not verify this paper's code; it is cited here as corroborating evidence for an existing
decision, not as a new adoption candidate.

---

## 5. The agent loop and its bounds

### 5.1 Two distinct loops, deliberately separated

`dsh` ships **two** iteration mechanisms and its own docs tell the model which to use
(`packages/workflow/tool-ralph/src/index.ts:184-186`): *"Ordinary long-running same-session work
belongs to goal tools."*

| | Goal loop | Ralph loop |
|---|---|---|
| Package | `packages/goal/*` | `packages/workflow/tool-ralph` |
| Context | **Same session**, accumulating | **Fresh child per round**, zero inheritance |
| Memory across rounds | Full session history | Only a bounded structured report + the workspace |
| Round cap | `maxGoalRounds` (config default) | `maxRounds` default **256**, hard deployment ceiling |
| Terminal states | `complete` \| `blocked` (+ paused) | `complete` \| `blocked` \| `budget-limited` \| `round-failed` |
| Completion check | model self-report, authority-gated | **structural gate on the report — see §5.3** |
| Who may start it | direct human only | model may call, but only "when the direct human explicitly asks for Ralph" |

### 5.2 Concrete numbers and their rationale

- `maxGoalRounds` — no hard default in the type; resolved from service config
  (`packages/goal/goal/src/types.ts:26-30`). Exceeding it does **not** silently stop: it transitions
  the goal to `blocked` with code `round-limit` (`goal-round-driver/src/index.ts:166-172`).
- `blockedAfterConsecutiveRounds` = **3** (`tool-goal/src/index.ts:34`). Minimum rounds before the
  model may self-declare blocked.
- Ralph `maxRounds` = **256**, `maxHandoffChars` = **16384**, `maxResultChars` = **16384**
  (`tool-ralph/src/index.ts:36-40`).
- `repeat-tool-reminder` thresholds = **[3, 5, 8]**, `argumentsPreviewChars` = **500**
  (`packages/guard/repeat-tool-reminder/src/index.ts:46-50`).
- Tool timeouts are **per-tool declared**, not global (`packages/guard/timeout-policy/src/index.ts:57`).

Note the **ceiling pattern**: the model may pass `maxRounds` but cannot exceed the deployment's
configured ceiling (`tool-ralph/src/index.ts:208-218`):

```ts
function resolveMaxRounds(requested: number | undefined, ceiling: number): number {
  const value = requested ?? ceiling
  ...
  if (value > ceiling) {
    throw new TypeError(`Ralph maxRounds ${value} exceeds the deployment ceiling ${ceiling}`)
  }
  return value
}
```

Model-supplied budgets are clamped by operator config, and violating the clamp *throws* rather than
silently truncating. Same philosophy as Zuno's permission rulesets.

### 5.3 **The one real completion gate in the repo: Ralph's structural report contract**

This is the most adoptable thing in `deepseek-harness`. Ralph's per-round report is a typed envelope
(`tool-ralph/src/index.ts:51-57`):

```ts
interface RalphRoundReport {
  readonly status: 'continue' | 'complete' | 'blocked'
  readonly summary: string
  readonly evidence: string[]
  readonly nextSteps: string[]
  readonly blocker: string
}
```

and the status field is **cross-validated against the other fields, deterministically**
(`tool-ralph/src/index.ts:125-143`, and again at the host boundary `:262-274`):

```
case 'continue':
  if (report.nextSteps.length === 0 || report.blocker !== '')
    throw new Error('a continuing Ralph report needs nextSteps and an empty blocker')
case 'complete':
  if (report.evidence.length === 0 || report.nextSteps.length !== 0 || report.blocker !== '')
    throw new Error('a complete Ralph report needs evidence, no nextSteps, and an empty blocker')
case 'blocked':
  if (!normalizedText(report.blocker))
    throw new Error('a blocked Ralph report needs a concrete blocker')
```

**A model cannot claim `complete` without naming at least one piece of evidence, and cannot claim
`complete` while still listing remaining work.** It is not test execution — nothing checks that the
evidence strings are *true* — but it deterministically kills the two most common false-completion
shapes: the bare "Done!" and the self-contradicting "Complete — remaining: fix the failing test."
Cheap, model-independent, unarguable.

Three more enforcement details worth copying:

1. **Validated twice, on both sides of the boundary.** Once in the sandboxed script, then again by
   the host in `readReport` (`:262-274`), which additionally requires the key set to match *exactly*:
   `Object.keys(value).sort().join(',') !== 'blocker,evidence,nextSteps,status,summary'`. Comment:
   *"Defensively decode the fixed script's report across a provider boundary."*
2. **Bounded handoff, hard-failed.** `maxHandoffChars` 16384; an oversized report throws
   (`:144-148`, `:272-275`). Only the report crosses rounds — never the child's transcript.
3. **Structural enforcement of context freshness** (`tool-ralph/src/index.ts:220-232`):

```ts
function requireFreshProvider(ctx: Context, name: string): SubagentProvider {
  ...
  if (!provider.capabilities.outputSchema)
    throw new Error(`... does not support structured output`)
  if (provider.inheritsParentContext)
    throw new Error(`... inherits parent context; Ralph requires a fresh provider`)
```

The loop *refuses to run* on a subagent provider that would leak parent context. Misconfiguration is
a load-time error, not a silent behaviour change.

4. **The orchestration script is deployment-owned, not model-authored** (`:86-89`):

> "Fixed, deployment-owned orchestration. The model supplies data only; it cannot alter the loop,
> provider route, schema, or handoff validation."

### 5.4 Corroboration of the codex `wait`-returns-no-content lesson

Ralph is an independent rediscovery of the same principle, arrived at from the other direction: the
parent never sees a child transcript at all. Only `≤16 KB` of validated structured JSON crosses the
boundary, and *the workspace itself* is the shared long-term memory
(`tool-ralph/src/index.ts:159`): *"The shared workspace and its current working tree are the
long-term memory and source of truth. … Treat the previous report only as a bounded handoff; confirm
it against the workspace."*

**Corroborates the prior finding, and strengthens it**: don't just make `wait` return no content —
cap the handoff in bytes and *require it to be re-verified against ground truth* rather than trusted.

### 5.5 Retry policy

**There is no retry loop in the guard layer.** `timeout-policy` produces a structured
`TOOL_TIMEOUT` error and stops; the doc comment explicitly leaves retry to a hypothetical other
plugin (`packages/guard/timeout-policy/src/index.ts:33-37`): *"`error.code` is the same
`TOOL_TIMEOUT` this plugin owns, so a retry/sandbox plugin (and replay) can route on it."* So `dsh`
offers **no answer on retry wall-time**; the 180-second bound already shipped here has no
counterpart to compare against.

What it *does* offer on timeouts is a subtle correctness idea worth noting — the deadline is
**scoped by code** so a nested outer timeout is not misattributed
(`timeout-policy/src/index.ts:18-25, 61-79`):

```ts
using d = deadline(exec.signal, timeoutMs, TOOL_TIMEOUT)
const upstream = exec.signal
exec.signal = d.signal
try {
  const result = await next()
  if (timeoutOf(d.signal, TOOL_TIMEOUT) !== undefined) return toolTimeoutResult(timeoutMs)
  return result
} finally {
  exec.signal = upstream
}
```

Three things here: (a) it **never races or abandons the tool promise** — it awaits `next()` and only
substitutes the result once the tool has reached quiescence; (b) scoping by code keeps *"a nested
outer deadline (another `tools/execute` wrapper's timer that fired first) from being misread as this
plugin's own timeout — it reads as an ordinary upstream cancel"*; (c) the caller's signal is
restored in `finally` so post-execute listeners never observe the timeout signal.

### 5.6 The livelock breaker: `repeat-tool-reminder`

`packages/guard/repeat-tool-reminder/src/index.ts` is a complete, well-reasoned livelock breaker.
Design decisions, each with a stated rationale:

- **Identity key is the canonicalized argument set**, deep-key-sorted before stringify
  (`:89-105`), so `{a:1,b:2}` and `{b:2,a:1}` are the same call.
- **Escalating thresholds `[3, 5, 8]`** with a gentle first message then a detailed one naming the
  tool, run length, and arguments (`:63-79`). Validated fail-loud at load: empty list, non-integer,
  `< 2`, or duplicates all throw (`:128-141`).
- **Advisory, never vetoing** (`:209-224`): it counts, delegates so a later listener can still
  block, then folds its reminder onto whatever decision came back. Comment: *"Observe-and-enrich,
  never veto."*
- **Counts denied calls too**, deliberately (`:184-188`): *"denied calls also flow through this
  waterfall … a model hammering a denied call is exactly the loop worth breaking."*
- **Resets on human interjection** (`:226-232`): *"A user interjection changes the context;
  repetition across it is not a loop."*
- **Untracked tools are transparent** — they neither count nor reset (`:33-34`, `:175-179`). Subtle
  and right: a `read` interleaved between two identical `bash` calls should not mask the loop.
- **The reminder itself is bounded** (`:35-42`): `argumentsPreviewChars` = 500, because *"Large
  payloads (a `write` body, a long command) would otherwise ride into the next request unbounded —
  precisely in a loop scenario"*. Detection always uses the full string; only the model-visible text
  is capped.

**Corroborates the livelock-breaker finding (third independent implementation) and supplies a
better design than a plain counter.** [pure] — a `HashMap<AgentId, (String, u32)>`, canonical JSON
via `serde_json` with sorted keys, and a hook on post-tool-execute.

---

## 6. Tool / environment interface

### 6.1 The pipeline

`docs/tool-execution-pipeline.md` (machine-generated from source) documents a five-stage pipeline
per tool call:

```
tools/pre-execute waterfall   (hooks, permission, sandbox → allow | deny | ask)
  → ctx.approval one-shot prompt (absent or unanswerable: DENY)
  → registered monotonic guards (deny or abstain; identity protected)
  → tools/execute waterfall     (around-dispatch: timeout, retry, metrics)
      → tool execute() body
      → fs/write-intent | fs/edit-intent  (read-before-edit gate, tool-fs only)
  → tools/post-execute waterfall (accept | block | replace | add context)
  → registry outer normalization (throws become isError)
  → ToolDefinition.finalizeContent (content-only invariant)
  → tools/result                 (frozen authoritative outcome, observe-only)
```

Largely equivalent to Zuno's governed registry + hooks. **Two details are better than boilerplate:**

**(a) Monotonic guards — deny is absorbing.** `packages/core/tools/README.md:25, 51`:

> `ToolGuard` — `(execution) => string | undefined`; … "Register a monotonic synchronous execution
> guard after `tools/pre-execute`: returning a reason denies the call, while `undefined` leaves it
> unchanged. … **Later waterfall listeners cannot turn a guard denial back into permission.**"

and `packages/extensions/tool-cordis/src/api-catalog.ts:1912`: *"Any matching guard may deny by
returning a reason, while **no guard can force-allow a call another guard denied**."*

The guard's return type makes widening *unrepresentable*: there is no `Allow` variant. Combined with
"identity protected" (a guard cannot rewrite which tool/args are being run), this means no plugin —
including a hostile or buggy one — can escalate another's denial. **This is the single best idea in
the tool layer** and it is a type-level property, not a runtime check.

**(b) Fail-closed approval.** `docs/tool-execution-pipeline.md:20` —
`ctx.approval` one-shot prompt, "**absent or unanswerable: deny**"; `:44` routes
`rejected, cancelled, unavailable` all to `denied`. A headless or disconnected UI cannot
accidentally auto-approve.

### 6.2 Sandboxing — and one genuinely novel idea

`docs/subsystems/sandbox.md`. Three modes, filesystem-effects only:

```ts
type SandboxMode = 'read-only' | 'workspace-write' | 'danger-full-access'
```

Backends: Linux `bwrap`/Landlock, macOS Seatbelt, Windows ACL restricted-token. Policy is resolved
**per call**, carrying `mode`, `workspaceRoot`, and an optional `sessionId`, and the workspace root
is *canonicalized with filesystem semantics before lexical normalization* "so a cwd containing
`symlink/..` identifies the directory where a spawned process actually runs" — a real symlink-escape
defence.

**Novel: enforcement completeness is a reported fact, not an assumption:**

```ts
/**
 * Enforcement completeness for this host. `partial` means an active backend or
 * older kernel ABI cannot govern every promised file effect; callers requiring
 * an absolute boundary must not treat it as `full`.
 */
type SandboxEnforcement = 'full' | 'partial'
```

Current `partial` cases are named: older Landlock ABIs, and the Windows ACL runner's
Everyone/hard-link boundaries. A caller that needs an absolute boundary **must reject or surface the
distinction** rather than silently trusting a mode it asked for and did not fully get.

This is the same instinct as Zuno's compatibility oracle refusing a mismatched binary: *report the
limits of your own guarantee instead of pretending.* Applied to sandboxing, it is an idea Zuno
should have — a sandbox that cannot fully enforce `read-only` on an old kernel should say so, and a
caller in a strict mode should refuse to proceed.

**Environments are NOT reset between attempts.** Both loops treat the live workspace as persistent
shared state and authoritative memory (`tool-ralph/src/index.ts:159`). There is no per-attempt
snapshot/restore anywhere — further confirming this is a production runtime, not an eval harness.

### 6.3 Code Mode

The registry's `mode` config selects native function calling, "Code Mode", or both
(`packages/core/tools/README.md:5`). Code Mode routes a reserved `run_code` transport whose
serialized sub-calls **still go through the full pipeline**, carrying the parent token, logging
`tool/code-dispatch`, and returning denials as binding rejections. Correct design — code execution
does not become a permission bypass. Not adoptable as mechanism ([no] — needs a JS realm), but the
*policy* is: if Zuno ever adds a scripting transport, sub-calls must re-enter the governed registry
rather than bypass it.

### 6.4 Spill: oversized tool output

`docs/subsystems/spill.md` — a `tools/post-execute` policy persists oversized tool text to a
session-scoped file and hands the model **an opaque locator plus retrieval guidance and an exact
byte count**, instead of the text. Not novel (Claude Code and others do this), but the seam split is
clean and the "locator, not path" discipline is right (`suggestedName` "is a hint, never a path",
sanitized to one safe path segment). [pure] if Zuno wants it.

---

## 7. Novel versus boilerplate, measured against what this project already has

### Boilerplate — Zuno already has equal or better

| `dsh` feature | Zuno equivalent | Verdict |
|---|---|---|
| Everything-is-a-plugin architecture | 4 plugin tiers (out-of-process JSON-RPC, WASM, JS, native Rust) | Zuno's is broader |
| `tools/*` waterfall hooks | 21 wired plugin hooks | Comparable |
| Governed tool registry, permission/sandbox at pre-execute | governed registry + permission rulesets | Comparable |
| Session log as replayable event source | SQLite session store | Comparable |
| MCP / ACP / LSP integration | present | Comparable |
| `packages/todo` — `TodoItem { content, status }` (`packages/core/session/src/types.ts:189-194`) | — | **Weaker than expected**: no id, no ordering, no evidence, no dependency. Nothing to adopt |
| `packages/plan/plan-mode` | — | **Actively worse — see below** |
| Spill / compaction | — | Standard; mild interest |

### `plan-mode` is the refused design, and its failure is instructive

`packages/plan/plan-mode/src/index.ts:1-7`:

> "Plan mode is logged per-agent collaboration state: while active, **a deployment-owned guidance
> section is included in each model request**, and `exit_plan_mode` presents the completed plan for
> user review… **Sandbox mode and approval policy enforce restrictions independently and do not read
> or write plan state.**"

**`dsh`'s plan mode is prompt text with no enforcement.** Nothing stops a model in plan mode from
writing files; the sandbox is configured separately and does not know plan mode exists. This is
exactly the design this project already refused ("collaboration modes as prompt text — this project
encodes modes as permission rulesets a model cannot argue with"). **A fifth strong team building the
weaker version is confirmation of the existing decision, not a reason to revisit it.**

One non-obvious insight buried in that same file worth keeping (`:17-19`):

> "The exit tool remains registered while plan mode is inactive, so entering or leaving plan mode
> changes only the prompt section, **not the request tool catalog**."

Rationale is KV-prefix-cache stability: mutating the tool catalog mid-session invalidates the
provider's prefix cache and costs real money on long sessions. **If Zuno's mode switching adds or
removes tools from the request catalog, it is paying a cache-invalidation tax.** The fix is to keep
the catalog stable and deny at execution time. Zuno's ruleset approach is already compatible with
this — worth confirming the catalog does not change shape when a mode changes.

### Genuinely novel

1. **Ralph's structural report contract** — status cross-validated against `evidence` / `nextSteps` /
   `blocker`, validated on both sides of the boundary (§5.3). Have not seen this elsewhere.
2. **Monotonic guards where deny is absorbing and unwidenable by type** (§6.1a).
3. **`SandboxEnforcement: 'full' | 'partial'`** — the sandbox reports the limits of its own
   guarantee (§6.2).
4. **`GoalActivation` armed/disarmed: durable intent separated from process-local authority to act
   on it**, with fail-closed disarm on every error path and disarm-on-load so restart never inherits
   autonomy (§2.4).
5. **The block-threshold floor** — a machine-enforced minimum number of rounds before a model may
   declare itself blocked (§2.3). The premature-surrender failure mode, treated as first-class.
6. **CAS revision on the goal**, so a model cannot complete a goal it has not just re-read (§3.1).
7. **Code-scoped deadlines** so a nested outer timeout is not misattributed to the inner one (§5.5).
8. **Bounded, hard-failed cross-round handoff** (16 KB) where the parent sees no child transcript,
   and the child is instructed to re-verify the handoff against the workspace (§5.4).

---

## 8. Ranked adoption candidates

Ordered by (value to Zuno) × (cheapness), excluding everything already decided.

### 1. Structural completion contract on any terminal claim — **[pure]**

Require a terminal status to be *structurally consistent with its own fields*: `complete` demands
≥1 evidence item and zero remaining steps; `continue` demands ≥1 next step and empty blocker;
`blocked` demands a non-empty concrete blocker. Reject the tool call otherwise.

- **Improves on**: the planned machine-checked gate. This is strictly *cheaper* and *composable with
  it* — it needs no test runner, no acceptance criteria, and no project-specific config, so it works
  on the first day and on tasks that have no runnable test. It kills bare "Done!" and
  self-contradicting completions deterministically.
- **Touches**: the goal/completion tool schema + its validator; a shared `TerminalReport` type;
  duplicate validation at any subagent boundary.
- **Note**: this is *not* a substitute for running tests. Adopt both: structure gate first (always
  applicable), evidence-execution gate second (applicable when a verify command exists).

### 2. Monotonic tool guards — deny absorbing, unwidenable by type — **[pure]**

A guard signature whose only outputs are *deny-with-reason* or *abstain*, evaluated after the
extensible hook waterfall, with call identity frozen. No later hook, plugin, or tier can convert a
denial into permission.

- **Improves on**: 21 hooks + permission rulesets. If any Zuno hook can currently *return allow* and
  thereby override an earlier deny, a third-party plugin (WASM/JS/JSON-RPC tiers are third-party
  code) can escalate its own permissions. Making the type unable to express "allow" removes the
  class of bug rather than auditing for it.
- **Touches**: the tool registry's guard/hook decision enum; hook dispatch ordering; a review of the
  21 hooks for any that can widen.
- **Highest security value on this list**, given Zuno's four plugin tiers.

### 3. `armed | disarmed` activation split on the goal loop — **[pure]**

Persist the goal's phase; keep "may this process autonomously continue it" in memory only. Disarm
on: any driver error, agent error, a turn ending on max-tokens, a failed durability flush, and
**process/plugin load over pre-existing sessions**. Re-arming requires a direct human action.

- **Improves on**: the already-ranked-first persisted goal. This is the missing safety half. Without
  it, a crash-restart or a session fork silently resumes an autonomous budget-spending loop — the
  worst possible failure mode for a single self-contained binary users run locally.
- **Touches**: goal state (add a non-persisted field), the continuation driver's error paths, session
  resume/fork, and the resume command's authority check.
- Pair with **flush-before-continue**: never reserve round *N+1* until round *N* is durably
  persisted, and treat a failed flush as disarm.

### 4. Authority tiers on goal mutation: the model may not widen its own objective — **[pure]**

`create`, `edit` (objective or budget), `pause`, `resume` require a *direct human message in the
current root-agent turn*. `complete` / `blocked` additionally accept the exact current
autonomous round. Subagents structurally excluded. Reject with a stable code.

- **Improves on**: permission rulesets, by extending them to goal *lifecycle*. A model that can edit
  `maxGoalRounds` has no budget; a model that can re-arm itself has no off switch.
- **Touches**: goal tool execute paths; needs a trustworthy `MessageSource` discriminant — note the
  attack `dsh` calls out: a default-`user` source lets a non-human producer inherit human authority
  (`authority.ts:69-73`). Zuno must ensure synthetic/plugin-injected messages carry a distinct source.
- Cheap and independently valuable even before the full goal loop lands.

### 5. Escalating repeat-call reminder as livelock breaker — **[pure]**

Thresholds `[3, 5, 8]` on consecutive identical calls keyed by *canonicalized* arguments; gentle
then detailed message; advisory (never vetoes); counts denied calls; resets on human interjection;
untracked tools transparent; the reminder's quoted arguments capped (500 chars) so the reminder
cannot itself balloon in the loop it is breaking.

- **Improves on**: the planned livelock breaker, by supplying a designed-out version — the argument
  canonicalization, the deny-counting, and the reminder cap are each a bug you would otherwise ship.
- **Touches**: a post-tool-execute hook; `HashMap<AgentId, (key, count)>`; canonical JSON.
- **Crate**: none — `serde_json` with `BTreeMap` gives deterministic key order.

### 6. Block-threshold floor: a minimum round count before self-declared `blocked` — **[pure]**

The model may not report `blocked` before round N (default 3) under autonomous authority; the tool
call throws. Humans exempt. Accompany with an explicit non-definition: difficulty, uncertainty, and
remaining work are not blockers.

- **Improves on**: nothing currently planned — this is a *distinct* failure mode from livelock.
  Roughly ten lines.
- **Touches**: the completion/blocked tool validator.

### 7. `SandboxEnforcement: full | partial` — honest guarantees — **[crate]**

Have the sandbox layer report whether it can fully enforce the mode requested on this host, and let
a strict caller refuse rather than silently accept partial confinement.

- **Improves on**: Zuno's compatibility oracle, by applying the same honesty to the security
  boundary.
- **Touches**: the sandbox/exec seam; a per-backend capability probe.
- **Crate**: the *reporting type* is [pure]; actual enforcement needs platform backends —
  `landlock` (pure-Rust Linux LSM bindings) is the plausible choice, with macOS Seatbelt via
  `sandbox_init` argv and Windows restricted tokens via `windows-sys`. Given
  `unsafe_code = "forbid"`, prefer spawning `bwrap`/`sandbox-exec` as argv wrappers over FFI, and
  report `partial` when the tool is absent — which is precisely what this idea is for.

### 8. Code-scoped deadlines — **[pure]**

Tag each timeout with the identity of the layer that armed it, so a layer only substitutes a
`TIMEOUT` result when *its own* deadline fired; an outer layer's expiry reads as an ordinary
upstream cancel. Never race or abandon the operation — await it to quiescence, then replace the
result. Restore the caller's cancellation token afterwards.

- **Improves on**: the 180-second retry bound, once timeouts nest (per-tool, per-retry, per-turn).
  Without scoping, nested deadlines are misattributed and error messages lie about which budget was
  exceeded.
- **Touches**: the timeout/cancellation plumbing; a `CancellationToken` carrying a reason tag.
- `tokio_util::sync::CancellationToken` supports this pattern; Zuno already has tokio.

### 9. Bounded, hard-failed subtask handoff with re-verification instruction — **[pure]**

Cap the child→parent handoff in bytes (16 KB), *error* rather than truncate on overflow, never pass
a child transcript, and instruct the consumer to confirm the handoff against ground truth (the
workspace) rather than trust it.

- **Improves on**: the already-decided typed subtask envelope and codex's `wait`-returns-no-content
  lesson — adds the byte cap, the hard failure, and the distrust instruction.
- **Touches**: the subtask envelope type + its validator.

### Not worth adopting

- **`dsh`'s completion model** (model self-report, no evidence check). Weaker than Zuno's target.
  Take the *authority* half, reject the *self-report* half.
- **`plan-mode`** — prompt-text collaboration mode with no enforcement. Already refused; `dsh`
  confirms the refusal.
- **`TodoItem { content, status }`** — too thin to be worth porting.
- **Code Mode** [no] — needs a JS realm. Keep the policy (sub-calls re-enter the governed registry).
- **The Cordis framework itself** [no] — a TS DI/plugin runtime; Zuno's 4-tier plugin system already
  covers the ground.
- **Workflow worker-threads** [no] — Node `worker_threads` executing model-adjacent scripts.
- **The Python SDK** — a client for a Node server; irrelevant to a single-binary Rust CLI.


---

## Appendix: corroboration / contradiction ledger

| Prior finding | `dsh` verdict | Detail |
|---|---|---|
| Persisted user-owned goal with status and budget | **CORROBORATES — 5th independent implementation** | `GoalSnapshot { objective, phase, blockedReason?, maxGoalRounds }` + `GoalRef { id, revision }`, `packages/goal/goal/src/types.ts:19-68`. Adds CAS revision, `blocked` as first-class terminal state with routable code, and the armed/disarmed split |
| Machine-checked completion gates | **PARTIALLY CONTRADICTS, and supplies a better cheap gate** | No evidence check on `complete` (`tool-goal/src/index.ts:298-305`) — completion is authority-gated self-report. BUT Ralph's report contract *is* a deterministic structural gate (`tool-ralph/src/index.ts:125-143`): `complete` requires ≥1 evidence and zero nextSteps |
| Livelock breaker | **CORROBORATES — 3rd implementation, best design seen** | `repeat-tool-reminder`, thresholds `[3,5,8]`, canonicalized-arg keying, advisory, counts denials, resets on human input, bounded reminder text |
| Typed subtask envelopes | **CORROBORATES** | `RalphRoundReport { status, summary, evidence[], nextSteps[], blocker }`, schema-enforced, cross-validated, double-validated across the provider boundary |
| `wait` must return no content | **CORROBORATES, from the other direction** | Parent never sees the child transcript at all; only ≤16 KB of validated JSON crosses, and the consumer is told to re-verify it against the workspace |
| Collaboration modes as prompt text — REFUSED | **CONFIRMS THE REFUSAL** | `plan-mode` is prompt text only; `packages/plan/plan-mode/src/index.ts:5-7` states sandbox and approval policy "do not read or write plan state" |
| Python/Node runtime as tool substrate — REFUSED | **No new evidence** | `dsh` *is* Node; Code Mode and workflow worker-threads confirm that path needs a JS realm. Nothing here changes the refusal |
| Vector-DB memory / `sqlite-vec` — REFUSED | **No new evidence** | `dsh` ships no vector memory at all in this drop; the workspace filesystem is the long-term memory. Mildly supports the refusal |

### New idea with no prior counterpart

- **Block-threshold floor** — a machine-enforced minimum round count before a model may declare
  itself blocked (`tool-goal/src/index.ts:290-297`, default 3). Premature surrender treated as a
  first-class failure mode distinct from livelock.
- **Monotonic guards** — deny is absorbing and "allow" is not representable
  (`packages/core/tools/README.md:25,51`).
- **`SandboxEnforcement: full | partial`** — the sandbox reports the limits of its own guarantee
  (`docs/subsystems/sandbox.md`).
- **KV-prefix-cache stability as a constraint on mode switching** — keep the request tool catalog
  shape stable across mode changes; deny at execution time instead
  (`packages/plan/plan-mode/src/index.ts:17-19`).

## Method notes

- Cloned at `47f94385` (`--depth 1`); all line numbers are from that commit. Permalink form:
  `https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/<path>#L<n>`
- Read source and type declarations, not only READMEs: `packages/goal/**` (all 2560 lines),
  `packages/guard/**`, `packages/workflow/tool-ralph/src/index.ts`, `packages/plan/plan-mode`,
  `packages/todo`, `packages/core/session/src/types.ts`, `packages/core/tools/README.md`, and the
  generated `docs/tool-execution-pipeline.md` and `docs/subsystems/sandbox.md`.
- Paper search: repo-wide `grep -rn arxiv` over all Markdown (zero hits) plus web search. Confirmed
  no DeepSeek-authored paper for this repo; three same-named unrelated third-party projects exist and
  are disambiguated in §4.
- **Not examined** (time-bounded): `packages/compaction`, `packages/context`, `packages/schedule`,
  `packages/jobs`, `packages/skill`, `packages/identity`, `packages/credentials`, `packages/lsp`,
  `packages/e2b`, `packages/session-query`, `apps/web`. `compaction` and `context` are the most
  likely to hold further transferable policy and are the recommended follow-up if more is wanted.
