# F4 Scope Fidelity Review — Final Verification Wave 4

## Verdict: REJECT

Wave 4 closes the implementation defects that drove the previous F4 rejection. The HTTP permission/question broker now exercises the production turn path, the API gap inventory has fallen from fourteen to ten, the Kiro package references converge on the installed `0.20.6`, and the goal regression drives two consecutive compactions with its objective and counters intact. All short, targeted verification run for this review passed, including the complete compatibility target and the workspace build.

The artifact is nevertheless not scope-consistent enough to approve. Two statements in the authoritative success criteria remain materially different from the behavior and evidence that now pass. These are frozen-contract failures, not requests to implement extra features.

## Blockers

### 1. Success criterion 4 still freezes fourteen gaps and the compatibility report still publishes 14/44, while the executable contract is 10/48

Todo 132 explicitly requires the four permission/question reply/reject operations to leave the frozen gap inventory, and its completed record reports **48 backed operations, 10 explicit gaps, and 93 compared / 81 exempt dimensions** (`.omo/plans/opencode-rust.md:1257-1263`). The implementation agrees: `FROZEN_API_GAPS` contains ten named operations, the live subject gate requires ten `503 backend_unavailable` responses and 48 backed operations, and the matrix requires 93/81 dimensions (`crates/oc-testkit/tests/compat_suite.rs:1842-1849,1893-1905,1908-1935`). The targeted compatibility run observed all sixteen tests passing, including `criterion_4_freezes_the_backend_unavailable_operations_by_name`.

The authoritative success criterion was not updated with that contraction. It still says the **fourteen** current gaps are frozen and still includes permission/question reply/reject among them (`.omo/plans/opencode-rust.md:1294`). The compatibility report registry and `known_gaps()` also still describe fourteen missing backends and forty-four backed operations (`crates/oc-testkit/tests/compat_suite.rs:138-144,2841-2847`). Consequently one green target emits mutually incompatible truths: its executable assertions enforce 10/48 while its generated report data publishes 14/44.

This is more than harmless historical prose. Criterion 4 says the members are frozen, so its exact inventory determines whether closing or opening an operation is a contract edit. The plan, executable set, and machine-readable report must name the same ten operations and totals before F4 can approve.

### 2. Success criterion 6 still requires Kiro `effort` behavior that its replacement test expressly does not verify

The version and invalid `client.middlewareStack.add` portions are correctly repaired. The support table, plan, capture, installed manifest, and user's config converge on `@sunerpy/opencode-kiro-auth@0.20.6`; the real plugin loads and its `chat.headers` hook injects `x-opencode-kiro-request-kind: compaction`. Both targeted Kiro tests passed.

However, criterion 6 defines the behavioral replacement as “a real Kiro request proving the injected header **and effort fields**” and also retains provider visibility through `models --format json` (`.omo/plans/opencode-rust.md:1297-1299`). The actual test states that `effort` is **deliberately NOT asserted** because observing it would require an outbound credentialed Kiro request (`crates/oc-plugin/tests/js.rs:456-473`). It drives `chat.headers`, not the plugin's internal `makeSdkClient(auth, region, effort)` request. The nearby real-plugin load test proves auth-provider registration and SDK initialization, but does not assert the resulting models through the CLI catalog (`crates/oc-plugin/tests/js.rs:319-355`).

The evidence candidly records this limit (`.omo/evidence/task-133-opencode-rust.txt:169-194`), which is preferable to fabricated proof, but evidence cannot silently narrow the criterion. Either prove the retained effort/model-catalog clauses with a deterministic non-live boundary, or obtain an explicit plan amendment that limits criterion 6 to the behavior actually tested.

## Mandatory Scope Determinations

1. **Do the four narrowing hooks make later scope growth visible? — Mostly yes, for the scopes they actually encode.** Criterion 2 pins pure mode plus both measured non-pure tree sizes; criterion 4 compares the live `503` set by method and path rather than count; criterion 6 pins one Kiro version and a real header-hook behavior; criterion 15 pins both the platform source gates and the `NOT EXECUTED` disclosure. The committed mutation evidence shows each named guard failing when its encoded scope is changed. The limitation is blocker 2: criterion 6's guard encodes the header but not the still-written effort clause. Criterion 4 also needs its authoritative prose/report reconciled with the ten-member guard.

2. **Do the ten remaining operation-specific `503 backend_unavailable` responses satisfy narrowed criterion 4? — Yes under the intended post-todo-132 narrowing, but the authoritative artifacts do not consistently state that narrowing.** All 58 upstream operations are registered and invoked; none returns `501`; the ten gaps are named, return `backend_unavailable`, and produce no side effect; 48 operations have local backends. This is honest gap accounting rather than false parity. Approval is blocked because criterion 4 and the report still say fourteen, not because the ten explicit responses need invented success behavior.

3. **Is todo 132 unauthorized scope expansion? — No.** It implements four operations already present in the 58-operation upstream contract and required by the HTTP turn workflow. The process-local broker is injected only into HTTP-driven turns; existing ordinary headless and TUI paths retain their prior collaborators. No new public route, hosted service, or unrelated product surface was introduced.

4. **Does body-first parsing plus ownership-checked RAII resolution preserve both upstream parity and fail-closed safety? — Yes.** Reply bodies are size-bounded and deserialized before normal ownership claim, preserving the upstream validation order. If parsing fails, cleanup attempts to claim only the exact `(session_id, request_id)`; a cross-session id therefore remains untouched. Successful claims remove one session-owned pending request, and dropping an unresolved `PermissionResolution` or `QuestionResolution` sends rejection. The production tests passed for permission park/resume, question park/resume, cross-session refusal, malformed-body ordering, and disconnect denial without the forbidden tool side effect (`crates/oc-server/src/api/request.rs:110-190`; `crates/oc-server/src/request_broker.rs:1-9,121-165,198-234`; `crates/oc-server/tests/api.rs::api_reply_routes_validate_bodies_before_rejecting_cross_session_requests`; `crates/oc-cli/tests/session_mutation.rs`).

5. **Are the four explicitly requested implementation properties satisfied? — Three fully; goal behavior now also satisfies its previously missing compaction clause, so all four implementation properties are satisfied.** First-party `unsafe` is forbidden and source-scanned; the Rust plugin conformance suite passes without JavaScript; shipping `oc-agent` source contains no model-id literal; and the goal suite now covers two consecutive compactions, objective/counter persistence, exactly-once guarded continuation, status ownership, and projection edit/rejection behavior. These implementation results do not override the two contract blockers above.

## Independent Count and Divergence Audit

- **Oracle:** runtime and recorded pin agree on the latest installed release, `1.18.15`; the compatibility target's live `/doc` and journal tests passed. The source compatibility identity remains the separately declared `1.18.13` baseline.
- **API:** independently recomputed as 58 upstream operations, 48 backed, 10 explicit gaps, and 174 matrix dimensions split 93 compared / 81 exempt. The residual 14/44 report text is blocker 1.
- **Prune:** `PRUNE_TABLES` and `DELETE_ORDER` cover the same ten session-attributable tables, matching amended criterion 13. No fictitious eleventh or twelfth related table is needed.
- **Workspace:** the current closed roster contains 36 crates and is bidirectionally checked against `crates.expected`. Historical todo-1 text still describes the 33-crate scaffold that existed at that time; the current roster section and live gate are authoritative.
- **Historical count corrections:** the todo-119 audit's count replacements were checked against the current artifacts: the API surface is 58 rather than 61, the live roster is 36 rather than 34, and the declared divergence total is now 13 after todo 133's plugin-tree addition. One deliberately preserved historical commit subject still says 61 and is correctly labeled as history. The present factual defect is not one of those preserved historical lines; it is the current criterion/report 14/44 drift identified above.

`docs/divergences.toml` contains exactly thirteen non-empty, reasoned entries, and `oc_testkit::divergence::DECLARED_COUNT` is 13. Each decision was checked against a live implementation or focused regression: default session sorting; session-attributable tool-output names; lazy directory creation; split version identity; the live `execute` schema; exactly two C8 maintenance operations; provider-family refusal; memory-off parity; applied literal subpath filtering; excluded `CONTEXT.md`; malformed-auth refusal; formatter rollback; and the measured non-pure plugin trees. The decisions are plausible scope choices rather than undeclared gaps.

One non-blocking precision issue remains in the gate's wording: `every_declared_behavioural_difference_names_a_test_that_exists_and_runs` iterates seven `BehaviouralDifference` records resolving to only six of the thirteen declaration ids. Other tests and manual inspection cover the remaining entries, but this function does not literally establish a behavior-test binding for every declared entry. The count/reason/docs gates are still effective; the assertion name and comments should not overstate its coverage.

## Scope-Creep Assessment

- C8 maintenance, slim native agents, durable goals, cross-session memory, `oc-process`, and `oc-reaping-fixture` are all explicitly approved scope or verification support.
- Todo 132 closes existing upstream HTTP operations and does not broaden non-HTTP approval behavior.
- The thirteenth non-pure plugin-tree divergence is an honest declaration of the criterion-2 narrowing, not an implementation gap laundered as a choice.
- No prohibited hosted/cloud control plane, billing/share feature, bundled JavaScript runtime, first-party unsafe implementation, or unrelated public surface was found.

## Verification Performed

The following completed successfully with `OC_MEMORY_GATE_MODE=skip` where applicable:

- criterion-2 pure-mode narrowing test;
- complete `oc-testkit --test compat_suite` target: 16 passed;
- Kiro version-convergence and real header-hook tests;
- two-consecutive-compactions goal regression;
- criterion-15 Linux/Windows disclosure test;
- route body-validation/ownership test;
- production HTTP permission, question, cross-session, and disconnect tests;
- first-party unsafe scan;
- shipping `oc-agent` model-literal scan;
- Rust plugin SDK conformance tests;
- `cargo build --workspace --offline`.

Per instruction, the approximately 100-minute memory gate and two-hour soak were not rerun. Native Windows containment was not executed on this Linux host and is not reported as passing.

## Required Resolution

1. Update current criterion 4 and every report/known-gap row to the same ten named gaps, 48 backed operations, and 93/81 matrix split already enforced by the executable test.
2. Reconcile criterion 6 with evidence: either prove Kiro effort behavior and model visibility at a deterministic boundary, or explicitly narrow the criterion to the real `0.20.6` load plus header-hook behavior that is currently tested.

This review changed no source, test, documentation, plan, commit, branch, or remote state. `F4-REPORT.md` is the sole intended deliverable.

F4 VERDICT: REJECT
