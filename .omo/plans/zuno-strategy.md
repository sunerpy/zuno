# Zuno — strategy: from port to product

> **Zuno — Zero code. Any task.**

Created 2026-08-13, at the user's direction:

> *"做到现在的话就没必要完全照搬了，我们应该取其精华去其糟粕，并参考热门的比如 claw-code
> opencode 等等，取其先进的设计理念和设计架构，因为本项目最后会重新更名为 Zuno — Zero code.
> Any task."*

## What this changes, stated plainly

This is **not a rename**. It is a change of contract, and the contract is what the last 183 plan
items were built to enforce.

The project reached 183/183 with four-reviewer approval, 3479 tests, and zero first-party `unsafe`.
**Every piece of that verification apparatus measures one thing: agreement with a pinned upstream.**
Concretely:

- `crates/oc-testkit/src/oracle.rs:81` pins `PINNED_RELEASE = "1.18.18"` and **refuses any binary
  that reports otherwise**.
- Nine compat test files in `crates/oc-testkit/tests/` exist to measure that agreement.
- `docs/divergences.toml` holds **17 entries** whose entire justification is *"we deliberately
  differ here, and nowhere else"* — plus a rule at `:11-14` that an unimplemented surface is a
  **gap**, never a divergence.
- **F4's whole role is scope fidelity** — "is this what was asked for" — judged against a frozen
  plan whose goal was parity.

So the risk is specific and it is not "we lose tests". It is this: **if parity stops being the goal
but the machinery keeps running, we get an apparatus that measures something nobody cares about
while reporting green.** That is exactly the "prose nothing derives from" defect class this project
fixed seven separate times — a stale claim that no longer tracks reality — except now at
architecture scale instead of one `&'static str`.

The pivot is right. It has to be made deliberately rather than by drift.

## Recommendation: split the surface, do not abandon parity wholesale

Parity is an **asset on one surface and a cage on the other**. Treat them differently.

### Keep parity: the HTTP API and the plugin ABI

Real third parties depend on this, today, on this machine. `opencode.json` loads
`opencode-antigravity-auth@1.6.0`, `@sunerpy/opencode-kiro-auth@0.20.6`, and
`@sunerpy/oh-my-openagent@4.21.0`. Those plugins call the pre-`/api` v1 routes — that is why todos
165/169/175 existed at all, and why F1 rejected criterion 4 until the measured routes actually
served.

**On this surface the oracle and compat suite stay exactly as they are.** Breaking it does not
"differentiate Zuno"; it breaks working installations for no gain. The 3479 tests keep their meaning
here.

### Diverge freely: the agent orchestration layer

Goal loop, task dispatch, subtask messaging, memory, skills. Here upstream becomes **one reference
among several**, and the contract changes from *"matches 1.18.18"* to *"has declared behavior, and
tests that pin it"*.

This is where `docs/divergences.toml` needs a decision I cannot make alone: its 17 entries currently
mean "deviation from upstream". Under Zuno, deviation in the agent layer is **the point**, not an
exception requiring justification. Either that registry is scoped to the compatibility surface only,
or it needs a second category. Left ambiguous, every future agent-layer change will look like a
divergence that needs declaring, and the rule will be quietly ignored — which is worse than either
choice.

## The strongest evidence in the whole research corpus, found while writing this

`.omo/refs/claw-code/.omx/ultragoal/` contains a **file-based goal state machine**:

```
get-goal-G010-session-hygiene.active-20260515T020857Z.json
get-goal-G010-session-hygiene.active.json
get-goal-G010-session-hygiene.complete.json
g010-final-quality-gate.log
g010-leader-verify.log
```

Active/complete state transitions per goal, with **per-goal quality-gate logs** and a separate
leader-verify step. That is a third independent implementation of the same shape, and it changes the
count:

| Design | codex (Rust) | ZCode | claw-code | prime-agent |
|---|---|---|---|---|
| Persisted goal with status | ✅ `ThreadGoal` | ✅ `/goal` | ✅ `ultragoal` | ✅ single goal + budget |
| Machine-checked completion gates | — | — | ✅ gate logs | ✅ autonomous gates |

**Four independent teams shipped goal-with-status; two of them added machine-checked gates.** That is
the strongest convergence signal available, and it is exactly E-1 and E-4 in
`.omo/plans/opencode-rust-enhancements.md`, which were already ranked first and fourth on other
grounds.

**Conclusion: E-1 (goal loop) and E-4 (autonomous quality gates) are the two items to build first,
and they are now corroborated four ways.** claw-code's own state-file layout is worth studying as a
concrete schema — it is on disk, 415 files, and has not been mined.

## What "取其精华去其糟粕" means operationally

The research already produced 167 KB of source-grounded findings with dependency verdicts
(`.omo/research/`). Its value is as much in the refusals as the recommendations. Carried forward:

**Essence worth taking** — all `[pure]`, no new runtime:
- Persisted user-owned goal with status and budget (E-1) — the only design that changes control flow;
  a hook cannot make the loop run again.
- Machine-checked completion gates plus a livelock breaker (E-4) — this project spent fourteen review
  waves proving by hand that "the model says done" is worthless.
- Typed subtask envelopes with `author`/`recipient` (E-2), and codex's v1→v2 lesson: split
  `send`/`wake`, make `wait` return **no content** (E-3) so a coordinator does not accumulate every
  child's output.
- Skill catalog with a hard description cap and path-glob gating (E-5) — the hard part of skills is
  keeping unused ones out of context.

**Dross to refuse, with reasons already established:**
- A Python or Node runtime as the tool substrate — prime-agent's central abstraction, and flatly
  incompatible with a single self-contained binary.
- prime-agent's memory store: one JSON file with a 20-entry/120-char dump. `oc-memory`'s
  SQLite + FTS5 + CJK trigram already beats it.
- Vector-database-dependent memory retrieval; `sqlite-vec` too, since it is a C extension needing
  compilation.
- Collaboration modes as prompt text — this project already encodes modes as permission rulesets
  (`plan` denies `edit`), which a model cannot talk its way out of. Prompt-text modes are strictly
  weaker.
- A structured acceptance-criteria schema inside the goal object — three teams could have; none did.
- `encrypted_content` on inter-agent messages, team create/delete tools, a general blackboard: each
  either solves a hosted-product problem or was deleted by the team that shipped it.

## The rename is real work, and one part of it is a user-facing decision

Measured, not estimated: **30 source files under `crates/*/src/` contain `opencode-rust`**, plus
`user_agent()` at `crates/oc-cli/src/version.rs:44`.

Mechanical: crate names, binary name, user agent, long display version, docs.

**Not mechanical — needs your decision:**

1. **Config and data directories.** If Zuno stops reading `~/.config/opencode/` and
   `~/.local/share/opencode/`, **every existing installation breaks**, including the three working
   plugins and the `auth.json` this machine's `google` OAuth credential lives in. Options: keep the
   old paths, read old and write new, or migrate on first run with a notice. This is a
   compatibility promise, not a detail.
2. **`COMPATIBILITY_VERSION`.** It is `1.18.13` (`version.rs:11`) and deliberately distinct from the
   oracle pin — it is what npm plugins see when checking `engines.opencode`. Under Zuno, what does a
   plugin's `engines.opencode` range even mean? Note a fact that bears on this: **antigravity@1.6.0's
   `engines` contains only `node`, no `opencode`** — so todo 174's version gate does not currently
   fire on the real installed plugins at all. Worth knowing before deciding.
3. **Whether the `opencode` command name is still accepted** as an alias.

## What I am not doing yet, and why

I am **not** rewriting the plan into a Zuno roadmap in this pass. Two reasons:

- claw-code is 415 local files, largely unmined, and its `ultragoal` schema is directly relevant to
  the two highest-value items. A roadmap written before reading it would be guesswork dressed as
  strategy.
- The parity-surface split above is a decision with real consequences for the divergence registry and
  for F4's role. I have given my recommendation; it should be confirmed before work is sequenced
  around it.

**Immediate next step**: a focused study of claw-code's goal/gate architecture, since it is on disk,
free to read, and corroborates the two items already ranked first.

## Open questions for the user

1. **The surface split** — parity kept on HTTP API + plugin ABI, free divergence in the agent layer.
   Agreed?
2. **Config directory compatibility** on rename — keep, dual-read, or migrate?
3. **Sequencing** — build E-1/E-4 (goal loop + gates) first under the current name and rename later,
   or rename first? Renaming first touches 30 files and invalidates nothing; building first delivers
   value sooner. I lean to **build first, rename at a natural boundary**, because the rename is
   mechanical and the goal loop is not.
