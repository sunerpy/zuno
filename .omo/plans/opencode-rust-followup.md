# opencode-rust — Follow-up backlog

Created 2026-08-13, after the main plan reached **183/183 with four-reviewer approval** at
product tree `crates=1b37eafb1e14` (HEAD `d2eb383f`).

## What this file is, and is not

Every item here was **explicitly ruled non-blocking** by the reviewer who raised it, under
the convergence protocol's Blocker admission threshold (`.omo/plans/opencode-rust.md`,
section `## Final Verification Wave — convergence protocol`). None of them blocked
acceptance, and closing them is **not** a precondition for anything already delivered.

They are recorded here because a non-blocking finding that lives only in a review report
is a finding that gets lost — which is exactly what happened in wave 8, when three of F2's
non-blocking observations went unactioned until F2 escalated them to Blockers in wave 9.
The orchestrator called that a scheduling failure of its own. This file is the fix.

**Correction to the record**: the orchestrator twice told the user this backlog held
"four items". It holds **five** from the backlog paragraph plus **one** F4 ruled Follow-up
in Round 2 — **six**. The undercount was an orchestrator error, not a reviewer omission.

## Status

None started. No item has an owner. Nothing here is in progress.

---

## FU-1. `ProviderRetryPolicy` has no production caller

**Status: CLOSED (2026-08-13)** — `run_turn` now performs bounded typed provider retries with rollback, terminal-error propagation, and permanent/idle-partial guards.

**Source**: F2, non-blocking observation F2-O1, raised in Round 1 (wave 13) and confirmed
still true in Round 2.

**Verified location**: `crates/oc-engine/src/retry.rs:140` (`ProviderRetryPolicy`), `:187`
(`retry_provider`). Callers exist only in tests and testkit.

**Why it matters.** This is the **zero-non-test-caller shape** this project has now found
five times — seam #17 (`HookInvocation::Auth`/`Tool`), `PluginKind::Tui` (todo 160),
`discover_plugins` (todo 163), `MODEL_ENDPOINTS_OPTION` (todo 162), and now this. In each
prior case a reviewer required it to be either wired or declared. A public retry policy
that production never invokes is a capability the tests prove and users cannot get.

F4's Round-2 note is relevant: todo 166 established that `run_turn` reads
`provider.stream(...)` and returns the first `ProviderError` **without** calling
`retry_provider`, so there is no retry amplification of the idle-timeout path. That is why
it is not a defect today — but it also means a transient provider 503 ends the turn.

**Two honest options.**
1. **Wire it**: a retryable transient failure emits `RetryRollback`, clears attempt-local
   accumulation, replays within a finite budget, and surfaces the final failure. F4's
   Round-2 report sketched exactly this in its required-closure list.
2. **Remove it and declare the narrowing**, as todo 160 did for `PluginKind::Tui`.

**Do NOT** leave it as a public API only tests can reach. That is the state five reviewers
have rejected.

**Acceptance if wired**: a production-path test drives a transient 503 through a real turn
and asserts bounded replay then a surfaced failure; removing the retry call fails it by
name; a permanent failure is not retried.

---

## FU-2. Malformed plugin-model diagnostic is unguarded

**Source**: F2, non-blocking observation F2-O2, Round 1 and Round 2.

**What exists**: todo 167 correctly isolates a malformed model sibling — one bad model does
not discard the whole provider — and emits a debug event naming plugin, model id, and decode
error.

**The gap**: deleting that event leaves **both** production JavaScript boundary tests green.
So the diagnostic is behavior nothing pins.

**Why it matters.** The silent-drop branch is deliberate and correct (discarding a whole
provider over one bad model is worse). But that makes the diagnostic the *only* way a plugin
author learns their model was rejected. An unguarded diagnostic is one refactor from
silence, and this project's worst defects have all been silent.

**Acceptance**: a test asserts the diagnostic names plugin, model id, and decode reason;
deleting the emission fails it by name.

---

## FU-3. Antigravity recovery's `tool_use_id` is not covered end to end

**Status: CLOSED (2026-08-13)** — The v1 adapter now resolves only the pending calls named by submitted ids and rejects unknown ids without writes.

**Source**: F2, non-blocking observation F2-O3, Round 2.

**Verified location**: `crates/oc-server/src/compat_v1.rs:1354-1357` — the route reads and
validates `tool_use_id`, requiring a string.

**The gap**: the route **discards** it after validation. Changing the fixture to a wrong id
leaves the route regression green. A separate engine test proves session-wide unfinished-call
behavior, but nothing connects the submitted id to that behavior.

**Why it matters.** Todo 169 fixed the case where an adapter *discarded the body* — F2 found
that by noticing a body extractor could be removed with the test still passing. This is the
same shape one field down: validated, then dropped.

**Acceptance**: a route-level test submits a `tool_result` whose `tool_use_id` names a
specific pending call and asserts *that* call is the one resolved; a wrong id produces an
observable difference.

---

## FU-4. The published OpenAPI binds no request or response bodies

**Source**: F3, finding F3-W13-03, Round 1.

**What F3 measured**: the document served at `/doc` binds **0 of 60** operations to any
request or response body.

**Why it matters.** Todo 171 fixed a case where the port served a response its own published
schema rejected, and did it by *deriving* the required-key set from `/doc` and the oracle
rather than hard-coding it. That technique only works where `/doc` actually describes bodies.
A schema that describes no bodies cannot catch the next projection defect — and projection
defects are what F4 spent eight layers on.

The oracle's own capture has **188 operations** for comparison
(`.omo/fixtures/oracle-openapi-1.18.12.json`).

**Scope caution.** This is potentially large. Bind bodies where the types already exist and
the binding is mechanical; do **not** invent schemas to raise the count. If most operations
cannot be bound without new type work, say so and record the remainder as a frozen gap with
its reason — the registry rule (`docs/divergences.toml:11-14`) forbids laundering an
unimplemented surface into a divergence.

**Acceptance**: every operation whose Rust types already describe its body binds that body in
`/doc`; a test asserts a bound operation's response validates against its own published
schema; unbound operations are enumerated in the frozen gap inventory with reasons.

---

## FU-5. `/agent` schema drift is unverifiable against the available capture

**Source**: F3, lower-severity observation, Round 1; recorded by todo 171 as the frozen gap
`v1-agent-projection-drift` (`crates/oc-testkit/src/compat_report.rs:491`) with a witness
test.

**Why it is honestly blocked, not neglected.** Todo 171 established three facts: all four
oracle-required keys (`name`, `mode`, `permission`, `options`) are returned; this build
publishes **no** `Agent` schema at `/doc`, so there is no self-contradiction to detect; and
the tree holds only a **1.18.12** capture while the oracle now pins **1.18.18**. Optional-key
drift (extra `builtIn`/`maxSteps`/`tools`, missing `hidden`/`native`/`steps`/`temperature`/
`topP`/`variant`) therefore **cannot be characterised** without fabricating a capture.

**What would unblock it**: a `/doc` capture from the currently pinned release, taken the way
`compat_suite.rs` takes its others — refetched from the running binary so provenance is an
executable assertion rather than a claim. Then the drift becomes measurable and is either
matched or declared.

**Do NOT** hand-author a capture to make the comparison possible. A capture no test
re-derives is a claim, not a fixture — the rule `no_pinned_oracle_paths.rs:20-26` states.

**Acceptance**: a provenance-asserted capture from the pinned release exists; the `/agent`
comparison either passes or its residual difference is declared with a reason; the witness
test for the gap is updated or retired accordingly.

---

## FU-6. Legacy reverse-projection `release_date` restoration is unguarded

**Status: CLOSED (2026-08-13)** — Pinned, and the gap was four fields wide, not one: `release_date`, `variants`, `capabilities.interleaved` and `limit.input` were all unobservable because `kiro_model()` held their defaults; two new tests drive the real JS auth loader over a non-default canonical model so all five restorations fail by name.

**Source**: the orchestrator found it while mutation-verifying todo 176; **F4 independently
ruled it Follow-up** in Round 2 after `git blame` showed it predates todo 176
(commit `35cda9514`), so it is not a regression that todo introduced.

**Verified location**: `crates/oc-plugin/src/js/projection.rs:267` —
`model.release_date.clone_from(&original.release_date);` in the legacy reverse-projection
branch.

**The gap**: commenting that line out and running the **entire** workspace suite produces
**zero** failures. The orchestrator confirmed this directly.

**Worth recording precisely because of how it was found.** The orchestrator's first mutation
of the epoch fix targeted this line, saw nothing fail, and briefly mistook it for the real
guard's absence — the actual guard is at `crates/oc-server/src/api/provider.rs:812`, pinned by
`compat_v1_provider_projection_preserves_catalog_model_semantics`. Two separate paths handle
`release_date`; only one is guarded.

**Acceptance**: a test pins the legacy reverse-projection's `release_date` restoration such
that removing it fails by name; or, if the line is genuinely redundant now that the forward
projection is typed, remove it and prove the removal changes nothing observable.

---

## Working rules for whoever picks these up

Carried from the main plan, because each was learned the hard way:

- **Mutate to verify, and mutate the right thing.** A registry-entry deletion and a
  behavioral break catch different things (wave 59). Beware equivalent mutants — the
  orchestrator was fooled four times, twice in this session alone.
- **Back up with `&&`, never `;`.** A truncating write once ran after `cp` had already
  reported `No space left on device` and destroyed an uncommitted file. Check `df -h /`
  first; stop above 90%.
- **Never `git add -A`, never combine `-A` with `-f`.** 48,148 build-product files were
  once committed that way.
- **Prose that nothing derives from goes stale.** Seven instances were fixed in the main
  plan, one of them in the orchestrator's own notepad. If you write a number, make
  something derive or check it.
- **Test inputs hide defects as effectively as weak assertions.** Todo 162 survived four
  provider waves because its tests used only heuristic-friendly model ids.
