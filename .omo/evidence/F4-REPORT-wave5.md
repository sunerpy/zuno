# F4 Scope Fidelity Review — Final Verification Wave 5

## Verdict: REJECT

HEAD `56c229c` is materially closer to the stated scope than wave 4. The two wave-4 contract defects are repaired in the authoritative success criteria: criterion 4 now says 48 backed operations and ten named gaps, and criterion 6 explicitly excludes Kiro `effort` rather than promising evidence the allowed test boundary cannot produce. Todos 134–137 are relevant remediation, not unrelated product growth. The four newly declared CLI divergences are also real measured differences rather than labels invented to make a comparison green.

The artifact still cannot receive F4 approval. Current user-facing and generated compatibility statements contradict the executable contract, one production transcript difference discovered by todo 136 is absent from both the divergence list and the known-gap report, and criterion 6 still retains a provider/model-catalog outcome that no current integration test proves. These are honesty or promised-proof failures, not demands to implement all ten deliberately retained API gaps.

## Blocking findings

### 1. The current compatibility report still publishes 14/44 after criterion 4 and the live gate moved to 10/48

The plan is now correct: success criterion 4 states **ten** frozen `503 backend_unavailable` operations and **48** operations with local backends (`.omo/plans/opencode-rust.md:1323`). The executable gate agrees: `FROZEN_API_GAPS` has ten method/path members, the live subject run requires ten unavailable and 48 backed operations, and the matrix freezes 93 compared / 81 exempt dimensions (`crates/oc-testkit/tests/compat_suite.rs:1842-1849,1893-1905,1908-1932`). `README.md:95` also says 48/10.

However, the same compatibility target's current `known_gaps()` still emits `"14 of the 58"`, `"Forty-four operations have local backends"`, and `"remaining 14"` (`crates/oc-testkit/tests/compat_suite.rs:2841-2847`). A green executable therefore produces a report that contradicts the contract it just enforced. This is the unresolved report half of the wave-4 blocker.

**Required to satisfy:** change the current report data to the source-derived ten/48 inventory and add a gate that derives or checks those report figures against `FROZEN_API_GAPS` and the matrix, so another backend closure cannot leave stale prose green.

### 2. The public divergence count says thirteen while the executable allow-list contains seventeen

`docs/divergences.toml` contains 17 entries and `oc_testkit::divergence::DECLARED_COUNT` is 17 (`crates/oc-testkit/src/divergence.rs:57-65`). The generated detail block in `docs/divergences.md` also contains all 17 entries. Its current introduction nevertheless says “Thirteen deliberate differences”, says four of the thirteen came from todo 119, and calls the todo-133 entry “the thirteenth” (`docs/divergences.md:3-14`). Its closing prose still describes “six records” (`docs/divergences.md:142-149`). `README.md:27` likewise links to “the thirteen deliberate differences.”

This violates the artifact-honesty purpose of criterion 17: a reader is told two different current totals by files that claim to be the single divergence source. The generated-entry test did not protect the surrounding prose.

**Required to satisfy:** update every current public count and historical explanation to 17 without rewriting preserved history, and make the docs gate check the current prose count against `DECLARED_COUNT`.

### 3. Todo 136 found a production transcript-shape difference that is reported only inside task evidence

The bidirectional lifecycle tests correctly prove that both binaries can list, continue, replay, and export one shared session. Their evidence also measures a distinct production difference: for one assistant turn release 1.18.15 persists `step-start`, `text`, and `step-finish` assistant parts, while this port persists only `text` (`.omo/evidence/task-136-opencode-rust.txt:191-215`). The evidence explicitly calls it a production-behavior difference and says it was deliberately not added to `docs/divergences.toml`.

That restraint was appropriate during a parallel test-only todo, but the final artifact cannot leave the difference only in a task log. It is neither one of the 17 deliberate divergences nor one of the three entries returned by current `known_gaps()`. `docs/divergences.md:3-6,135-140` promises that an unimplemented surface is recorded in `known_gaps`, not hidden on the divergence page. The current report breaks that promise.

**Required to satisfy:** either implement the release's turn-part lifecycle, deliberately accept and declare the difference with a reason and executable witness (raising the divergence count), or retain it as a named compatibility gap. Whichever classification is chosen must appear in the current compatibility artifact, not only historical evidence.

### 4. Criterion 6's retained model-catalog outcome is still not proved through the production CLI path

The real-plugin tests prove useful but narrower facts. `js_real_supported_plugins_load_with_their_own_sdk_clients` loads both installed packages and observes auth providers `google` and `kiro-auth` plus their own SDK clients (`crates/oc-plugin/tests/js.rs:319-365`). Todo 137 proves those real auth hooks coexist with Rust and WASM tiers and that Kiro's header hook remains live. Neither test feeds the real plugin results into the production model catalog and then invokes the CLI.

Success criterion 6 still says those plugins' providers appear in `models --format json` so the user's primary models work (`.omo/plans/opencode-rust.md:1327`). The literal command is itself wrong: the catalog source documents that upstream has no `--format json` flag and that `opencode models` is the actual list surface (`crates/oc-llm/src/catalog.rs:40-54`). The universal CLI parity test's `models` rows use a pinned catalog fixture, not the two real plugins (`crates/oc-cli/tests/cli_parity.rs:230-235`). Thus plugin loading is proved, generic model-list parity is proved, but the promised integration between them is not.

**Required to satisfy:** correct the impossible command spelling in the criterion and add a deterministic production-path integration proving the retained semantic outcome: with the real supported plugin configuration loaded, the intended Kiro and antigravity-backed models/providers are selectable and visible through the actual `models` CLI surface. If plugin auth registration is not what populates that catalog, amend the criterion to the real day-one behavior rather than asserting a nonexistent connection.

## Mandatory five judgments

### 1. Are todo 135's four new divergences real, or explanations invented to excuse CLI mismatch?

**They are real differences with defensible reasons; none is merely an unimplemented command relabeled to make the suite green.**

- **`plain-cli-presentation`: real and appropriately normalized.** Dual-process probes measure upstream SGR output despite `NO_COLOR`, its `Error: ` prefix, `@clack/prompts` gutter, and JSON insertion-order/integral-number rendering. The normalization rules are narrow and carry negative controls that preserve changed keys, values, arrays, non-SGR escapes, and line endings.
- **`diagnostics-name-their-cause`: real and reasonable.** The recorded processes differ on message content: upstream emits opaque `ServeError`/generic failures while this port names the bind address or input and cause. Preserving actionable diagnostics is a legitimate deliberate difference, not missing functionality. Observation: its parity-row witness currently asserts only that both sides fail; unlike the other three additions, it does not directly assert the documented stderr texts. That regression-proofing weakness should be fixed, but the measured difference itself is genuine.
- **`session-list-output-shape`: real and substantive.** A shared non-empty database produces upstream's six flat fields versus this port's richer/nested global-list shape, and a dedicated live test pins the `projectId`/`projectID` distinction. It is correctly not treated as presentation normalization.
- **`non-vcs-plan-glob-is-absolute`: real and corrective.** The dedicated dual-binary test measures upstream's relative, unusable non-VCS plans glob and this port's absolute glob. The reason follows directly from upstream assigning `/` as the non-VCS worktree.

The prior 13 entries were also checked against their described implemented surfaces or focused tests: session sorting, attributable tool-output names, lazy directory creation, split identity, execute schema, exactly two C8 endpoints, provider-family refusal, memory-off parity, applied literal subpath filtering, `CONTEXT.md` exclusion, malformed-auth refusal, formatter rollback, and non-pure plugin-generated trees all describe actual choices. The blocker is not that any of the 17 is fabricated; it is the stale public count and the additional unclassified turn-part difference.

### 2. Is todo 134's independent five-minute deadline faithful remediation or scope growth?

**Faithful remediation.** Last-observer disconnect can reject a pending request only if an observer connected first. A request created when no observer ever connects has no disconnect transition, so subscriber awareness alone cannot satisfy the fail-closed objective. The independent deadline closes that logically distinct stranded-request state and always rejects rather than allows. It does not add a public product capability or broaden permission authority. Five minutes is a policy choice that could later be configurable, but having some bounded fail-closed deadline is required by the failure model.

### 3. Is a loud default-suite WASM skip enough for criterion 7?

**A loud skip alone is not proof, but the current HEAD has separate feature-enabled proof.** The default workspace run compiles named skip arms and therefore honestly says what it did not execute. The tracked `cargo test -p oc-plugin --features wasm` run executed the real-tier integration and JS targets successfully (11/11 and 9/9), so criterion 7 is proved for this reviewed HEAD. This is not a current F4 blocker. It is an enforcement observation: the feature-enabled command must be a required final/CI gate, because a future regression could pass the default suite while only printing the skip.

### 4. Do the ten remaining API gaps satisfy amended criterion 4?

**Yes, at the behavior-contract level.** The amended criterion deliberately permits exactly ten named method/path operations to return explicit operation-specific `503 backend_unavailable`; all 58 operations are registered and invoked, none may answer 501, and leaving the frozen set requires gaining compared status/body coverage. That is honest narrowed accounting, not false behavioral parity. Blocker 1 concerns the stale report emitted alongside this valid executable contract; it is not a demand to implement the ten retained backends.

### 5. Is excluding Kiro `effort` an honest boundary or concealment?

**It is an honest and appropriate boundary.** `effort` is selected inside the third-party plugin's private AWS client on a credentialed outbound Kiro request. The permitted deterministic suite cannot observe it without live credentials/network, and the criterion now says so explicitly while retaining an observable real-plugin header behavior. Removing an impossible proof obligation is not laundering. This conclusion does not cure blocker 4: provider/model visibility is a separate retained clause and still needs proof or correction.

## Four explicitly requested implementation properties

All four are satisfied by current source-backed gates:

1. **No first-party `unsafe`: satisfied.** Workspace policy forbids unsafe code and the shipping-source scan covers the current 36-crate roster rather than an old scaffold count.
2. **Rust plugin without JavaScript: satisfied.** The Rust example registers its tool/hooks and passes `ConformanceSuite` without the JS host.
3. **Slim agent design: satisfied.** Built-ins retain delegation boundaries, temperature, deny-by-default permissions, and output envelopes; the shipping-source guard rejects model-id literals in `oc-agent`, while model selection remains inherited/overridable.
4. **Goal behavior: satisfied.** The regression drives two consecutive compactions while preserving objective and counters, and the suite also pins exactly-once guarded idle continuation, system-owned status, and projection objective-edit/status-rejection behavior.

## Independent counts and scope assessment

- **Implementation ledger:** 140 checked implementation entries and no unchecked implementation entry; the duplicated historical todo numbers do not create additional current work.
- **Oracle:** executable evidence resolves release 1.18.15; 1.18.13 remains the separately declared compatibility identity.
- **API:** 58 upstream operations = 48 backed + 10 frozen gaps; 174 dimensions = 93 compared + 81 exempt. The source assertions are right; `known_gaps()` is wrong.
- **Divergences:** 17 TOML entries and 17 generated detail headings. `DECLARED_COUNT = 17` is right; README and the divergence-page prose are wrong.
- **Workspace:** 36 first-party crates in the closed roster. Historical scaffold counts are not current totals.
- **Prune:** ten session-attributable related tables, matching the amended criterion and delete order.
- **Scope growth:** todo 134's broker reclamation, todo 135's parity harness, todo 136's interoperability harness, and todo 137's real-plugin coexistence remain inside approved compatibility and verification scope. No unrelated hosted control plane, billing/share feature, bundled JavaScript runtime, or new public product surface was found.

## Non-blocking observations and disclosed limits

- `diagnostics-name-their-cause` should receive a direct dual-process stderr liveness assertion; `BothFail` alone cannot detect a stale declaration.
- The WASM real-tier command should be mandatory in CI/final verification rather than relying on a default-suite skip message.
- G6 is proved on Linux only. Native Windows containment remains explicitly **NOT EXECUTED**, as the amended criterion permits when disclosed.
- The pre-`oc-process` orphan count remains honestly **NOT MEASURED**; the evidence proves post-guard cleanup, not a quantified pre-existing defect.
- G2 passes its frozen formula, but the 19,472 KiB margin is only 1.29% and only 2,440 KiB wider than the measured 17,032 KiB spread. This is operational fragility, not a scope-contract failure.
- The approximately 100-minute memory gate and two-hour soak were not rerun, per instruction.
- No current wave-5 F2 report was present during this review. That is not an F4 scope blocker by itself, but success criterion 18 and overall completion cannot be claimed until current F1–F4 verdicts all exist, all approve, and the user explicitly accepts them.

## Verification basis

This review inspected the authoritative criteria, current source/test gates, all todo 134–137 evidence, the 17-entry allow-list, generated divergence detail, compatibility report registry, and current git HEAD/status. It relied on the tracked successful targeted/workspace results rather than rerunning the forbidden long memory and soak gates. No source, test, documentation, plan, commit, branch, or remote state was changed; `F4-REPORT.md` is the only intended worktree modification.

F4 VERDICT: REJECT
