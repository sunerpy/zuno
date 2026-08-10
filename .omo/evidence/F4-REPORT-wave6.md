# F4 Scope Fidelity Review — Final Verification Wave 6

## Verdict: REJECT

HEAD `b753fb950e5376f6f93e51024e3539900bc544ce` (`b753fb9`) contains faithful, in-scope remediation for the substantive defects assigned to todos 138–142. The subject-freshness fix protects correctness without imposing a per-probe build, the real JavaScript provider path now reaches the production `models` command, the assistant turn-part mismatch is honestly classified as an outstanding named gap, and the diagnostics divergence has a two-sided executable witness. The four explicitly requested product properties remain backed by source-level or executable guards. All 17 allow-listed divergences describe measured behavior and have defensible reasons.

F4 nevertheless cannot approve the final artifact. Two current contract statements are false: the public divergence page says there are thirteen divergences while the executable source of truth contains seventeen, and success criterion 6 still requires the nonexistent upstream command `models --format json` after todo 139 correctly established that the real surface is plain `models`. The implementation ledger also contains 145 checked rows but only 142 unique todo identifiers; that distinction must be reported rather than calling them 145 uniquely numbered implementation tasks.

## Blocking findings

### 1. The public divergence introduction still says thirteen while the authoritative count is seventeen

`docs/divergences.toml` contains 17 entries and `oc_testkit::divergence::DECLARED_COUNT` is 17. The generated detail block in `docs/divergences.md` contains those entries. However, the hand-written introduction still says “Thirteen deliberate differences,” “four of the thirteen,” and “The thirteenth” (`docs/divergences.md:3-14`). This is a current user-facing contradiction on a page that calls itself the single declaration point.

The docs test proves that generated entries and their reasons match the TOML source, but it does not derive or check the hand-written total. Consequently, the suite can remain green while the headline count is false. This violates success criterion 17's artifact-honesty requirement even though none of the 17 individual declarations is fabricated.

**Required to satisfy:** update the current introduction and historical explanation so they accurately distinguish the 13-entry historical state from the current 17-entry total, and add a docs guard that derives or validates the current public count against `DECLARED_COUNT`.

### 2. Success criterion 6 still promises an upstream command that does not exist

Todo 139 measured upstream 1.18.15's actual interface and explicitly records that `models --help` offers `--verbose`, `--refresh`, and `--pure`, but no `--format json`. Its acceptance criterion therefore correctly requires the real plain `models` output. The implementation and `plugin_models` integration test now prove the important semantic path: JavaScript plugin `ProviderHook.models` results are merged through `Catalog::replace_provider_models`, and the real providers become visible through the production CLI without hard-coding `kiro-auth`.

The authoritative success criterion was not corrected with that finding. It still says both auth plugins' providers must appear in `models --format json` (`.omo/plans/opencode-rust.md:1365`). A final contract cannot require a literal upstream parity command that upstream rejects, particularly when the completed todo already names and tests the correct command.

**Required to satisfy:** amend criterion 6 to require provider visibility through plain `models`, preserving the production-path and real-plugin assertions from todo 139. This is a contract correction, not a request for additional product scope.

## Mandatory five judgments

### 1. Is todo 138's Cargo-based subject freshness fix faithful correctness work, or an out-of-scope performance regression?

**Faithful correctness work.** The prior harness could run a stale `target/debug/opencode-rust` and report a mutation green even though the mutated source was absent from the tested binary. `Subject::discover_or_build` now invokes the package-specific Cargo build and caches the resulting path once per test process, while an explicit `OC_TESTKIT_SUBJECT` remains a visible caller-owned override. That closes a false-green verification seam. The measured incremental cost is small and is not paid for every probe, so the implementation does not trade correctness for an unusable suite.

### 2. Is todo 139's real-plugin-to-`models` path faithful remediation, or unrelated provider expansion?

**Faithful remediation.** Criterion 6 already promised that the user's two real auth plugins contribute usable providers. Before todo 139, loading a plugin and invoking hooks did not prove its models reached the user-facing catalog. The new path consumes `ProviderHook.models`, replaces that provider's catalog models, and exercises the production command. It does not hard-code Kiro models, add an unrelated hosted provider, or broaden the public contract. The remaining `opencode` hosted-provider difference is separately identified rather than conflated with the Kiro defect.

### 3. Is todo 140 correct to classify `assistant-turn-step-parts` as a named gap rather than divergence 18?

**Yes.** Upstream unconditionally persists `step-start` and `step-finish` around an assistant turn. This port models and can serialize those parts, and `StreamProjector` can produce the shape, but the production `run_turn` to `checkpoint_assistant` path is not wired to persist them. That is incomplete implementation, not an intentional behavioral choice with a reason. Publishing `assistant-turn-step-parts` in the generated known-gaps section, with a real-turn witness that would fail if behavior and classification diverge, is the honest treatment. `DECLARED_COUNT` correctly remains 17.

### 4. Are all 17 declared divergences real, reasoned behavioral choices rather than omissions relabeled to make parity green?

**Yes.** Each declaration maps to an observed or source-backed behavioral difference and states why this port intentionally retains it. The set covers session sorting, attributable tool-output names, directory creation timing, compatibility identity, execute schema, the two C8 endpoint additions, provider-family refusal, memory-off behavior, literal subpath filtering, `CONTEXT.md` exclusion, malformed-auth refusal, formatter rollback, non-pure plugin-generated trees, plain CLI presentation, causal diagnostics, session-list output shape, and absolute non-VCS plan globs. The latest diagnostics entry now has direct two-sided witnesses for all three declared surfaces rather than merely checking that both binaries fail. The count/prose contradiction in blocker 1 is a documentation-integrity failure, not evidence that an entry is invented.

### 5. Does the completed ledger honestly establish 145 uniquely numbered implementation tasks?

**No.** There are 145 checked implementation-entry rows and no unchecked implementation row, but only 142 unique identifiers. Numbers 124, 125, and 129 each occur twice; identifiers 1 through 142 otherwise have no gaps. It is accurate to say “145 checked entries” or “142 unique todo numbers,” but not “145 uniquely numbered tasks.” The duplicate historical numbering does not itself create missing implementation work, yet final reporting must preserve the distinction.

## Four explicitly requested implementation properties

All four remain satisfied on the reviewed HEAD:

1. **No first-party `unsafe`: satisfied.** Workspace policy forbids unsafe code and the release-surface scanner covers the closed first-party crate roster.
2. **Rust plugin without JavaScript: satisfied.** The Rust example registers its tool and hooks and passes `ConformanceSuite` without using the JavaScript host.
3. **Slim agent design: satisfied.** Built-in agents retain negative delegation boundaries, temperature, deny-by-default permissions, and output envelopes; a shipping-source guard rejects model-id literals in `oc-agent`, while session inheritance and explicit overrides remain available.
4. **Goal behavior: satisfied.** Tests preserve objective and counters through two consecutive compactions, enforce exactly-once guarded idle continuation, keep status system-owned, and allow objective edits while rejecting status edits through the Markdown projection.

## Counts and scope assessment

- **Implementation ledger:** 145 checked rows, 142 unique todo identifiers, duplicate identifiers 124/125/129, and no unchecked implementation row.
- **API contract:** 58 upstream operations, with 48 locally backed and exactly ten frozen named gaps. `known_gaps()` now derives and publishes the current accounting rather than the stale 14/44 wording from wave 5.
- **Divergences:** 17 TOML entries and 17 generated details; the executable count is consistent, while the hand-written public introduction is not.
- **Assistant turn parts:** one named compatibility gap, not an eighteenth divergence.
- **Scope growth:** todos 138–142 repair verification fidelity, promised plugin behavior, compatibility disclosure, fail-closed/event-order coverage, and divergence liveness. No unrelated product surface was found.

## Non-blocking observations and disclosed limits

- The approximately 100-minute memory gate and two-hour soak were not rerun, as instructed.
- Windows process containment remains implemented but not executed on this Linux host, as the narrowed criterion permits when disclosed.
- F3 did not produce a current successful final report in the reviewed sequence. This independently prevents project-wide criterion 18 from being claimed, although it is not a reason to alter the F4 scope findings above.
- The duplicate todo numbering should be cleaned up or explicitly preserved as historical numbering before presenting a single task total to users.

## Verification basis

This review inspected the authoritative plan, todos 138–142 evidence, current source/test guards, the 17-entry divergence registry, generated compatibility artifacts, and the four requested implementation properties. The short verification set passed for subject freshness, plugin-backed `models`, the turn-part gap witness, universal CLI parity, docs generation, the first-party unsafe scanner, Rust plugin conformance, goal behavior, the compatibility suite, and the workspace build. The prohibited long memory and soak gates were not run. No source, test, documentation, plan, commit, branch, or remote state was changed; this report is the only intended worktree modification.

F4 VERDICT: REJECT
