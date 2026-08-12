# F2 Code Quality Review — Wave 12

- **Audited HEAD:** `79ea3c3c`
- **Role:** F2 code quality and test-honesty reviewer
- **Verdict:** **REJECT**

## Planned probes

1. Re-read the wave-11 blocker and independently verify Todo 166 through a real-turn stalled-socket trace, behavioral mutation of the idle application point, and inspection of whether `ProviderRetryPolicy` has any non-test caller.
2. Verify Todo 165's ten v1 route backings against recorded plugin request/response shapes; separately mutate route behavior and served/default derivation so both capability and registry regressions are observable.
3. Verify Todo 167's plugin-boundary `ResolvedModel` mapping field by field, including `providerID`, optional `family`, optional `variants`, and malformed-entry reporting; behaviorally mutate the mapping.
4. Hunt for a twenty-third seam: zero-production-caller APIs, ignored or silently skipped tests, fixture-guaranteed assertions, unreachable errors, weak/heuristic-friendly test inputs, and silent-drop branches.
5. Restore every mutation, run `cargo test --workspace --offline`, `cargo clippy --workspace --all-targets --offline`, and `cargo fmt --all --check`, then confirm no product/test/plan/documentation mutation remains.

## Incremental evidence

### Baseline and tooling boundary

- `git rev-parse HEAD` independently returned `79ea3c3ca80a5d5a68e2690ba6dc0978911bf355`; initial `git status --porcelain` was empty.
- This sibling worktree has no `.codegraph/` index, so source inspection uses direct reads/searches as allowed by the task. I re-read wave 11 and the wave 56–59 rules before judging the repairs: mutate the producer/application point, re-check the original boundary, distinguish registry deletion from behavioral corruption, and prove a mutant is observable before calling it a test gap.

### Todo 166 — source trace and retry reachability

- `a_stalled_provider_ends_a_real_turn_with_partial_text_and_a_visible_idle_error` no longer stops at `ReqwestTransport::send`. Its fixture runs the production `CompatibleProvider` over a real held-open TCP response, resolves it through `ProviderRegistry`, and calls `oc_engine::loop::run_turn` with a migrated in-memory database and a real event channel. It requires the turn to finish inside one second, requires the already-decoded `PARTIAL_T166` text on the user-visible `TurnEvent::Provider` surface, renders the returned `TurnError` through the same `describe_turn_failure` used by `TurnHost::drive`, and requires both `idle timeout` and `OPENCODE_STREAM_IDLE_TIMEOUT` in that user-facing text. The paired progressing test sends four delayed SSE deltas over a duration greater than one idle window and completes, so this is an idle bound rather than a total deadline.
- `run_turn` calls `provider.stream(completion)` directly and applies `?` to the first `ProviderError` stream item before any replay branch. Repository-wide symbol search found `ProviderRetryPolicy`/`retry_provider*` production definitions in `oc-engine::retry`, but callers only in `oc-engine/tests/retry.rs` and `oc-testkit/src/cassettes.rs`; neither `run_turn` nor `oc-cli` calls them. The no-`RetryRollback` assertion in the real-turn test is therefore consistent with the actual path.
- **Non-blocking ledger finding F2-O1:** `ProviderRetryPolicy` and public `retry_provider` currently have zero production callers. This is not a hidden retry amplifier for Todo 166; it is a dormant advertised recovery facility with honest isolated tests but no product integration. It belongs in the plan ledger so it is either wired deliberately or narrowed/removed rather than later assumed live.

### Todo 166 mutations — both application point and default are guarded

- I discarded an initial command that used `--exact` with an unqualified test name because Cargo reported **0 tests**; it was not counted as evidence. Re-running without `--exact` executed one named test in each target: the real-turn stalled-socket regression passed in 0.11s and the production-default regression passed.
- **Mutation 1 (behavioral application point):** temporarily replaced `tokio::time::timeout(idle.duration(), body.next())` in `oc-provider-compatible/src/transport.rs` with an 86,400-second timeout. `cargo test -p oc-cli a_stalled_provider_ends_a_real_turn_with_partial_text_and_a_visible_idle_error --offline -- --nocapture` ran exactly the named test and failed at its one-second real-turn budget (`Elapsed(())`). This proves the test reaches the transport idle application point through the actual turn, rather than merely asserting an injected fixture value. Restored immediately.
- **Mutation 2 (production default):** temporarily changed `DEFAULT_RESPONSE_IDLE_TIMEOUT` from 120 seconds to 86,400 seconds. `cargo test -p oc-provider-compatible production_transport_installs_a_sane_default_idle_timeout --offline -- --nocapture` ran exactly the named test and failed with `the production default must terminate before the liveness probe`. Restored immediately.
- **Disposition:** the wave-11 Todo 154/166 blocker is closed at the requested boundary. The real turn terminates, its pre-error text remains on the live user-visible event surface, and the same rendering path used by the production host exposes the idle error and override name. The implementation application point and its production default are guarded by distinct, killed mutants.

### Todo 165 — BLOCKER: the route tests use friendlier inputs than the recorded plugin callsites

- The ten `ApiAdapter` registrations are real and the handler table maps all ten to shared `/api` implementations. `v1_coverage().served` is derived from `V1_BACKENDS.len()` (including the toast sink), while diagnostics resolve each route through the same backend table. This closes the old accounting-only defect, but it does not make every wire adaptation faithful.
- **Session-create model is persisted in the wrong schema.** Real OMO callsites send `body.model = {id, providerID, variant?}` (for example installed `@sunerpy/oh-my-openagent@4.21.0` at `dist/index.js:135030-135041` and `143073-143084`). The published SDK's `SessionCreateData` only types `parentID`/`title`, but JavaScript forwards those extra fields and the installed plugin demonstrably sends them. `V1SessionCreateBody.model: Option<Value>` passes that object unchanged to `api::session::create_session`, which serializes it directly into `session.model`. That is correct for the real callsite's `{id,providerID}`, but the Todo 165 route test instead sends `{providerID,modelID}` — the **message** spelling, not the session spelling — and then never reads or executes from the persisted model. The test therefore accepts a row shape this repository already proves the released binary rejects (`oc_db::session::model_reference` documents and tests that session rows require `id`, while `modelID` makes rollback `session list` fail). This is a test-input honesty failure: its fixture would create corrupt session state while remaining green.
- **The recorded recovery prompt is rejected.** The capture cites Antigravity's `client.session.prompt` at `dist/src/plugin/recovery.js:126-130`; that call sends `parts: [{type:"tool_result", tool_use_id, content}]` (with its own `@ts-expect-error`). `prompt_body` accepts only `text`, `file`, `agent`, and `subtask`, returning `400 unsupported v1 prompt part type` for `tool_result`. The shipped test exercises only ordinary `text` plus `file`/`agent`, so the exact plugin call that justified backing the route is never tried. Antigravity catches this failure and returns `false`, silently disabling the advertised crash-recovery behavior.
- **Summarize silently discards the requested model.** The published `SessionSummarizeData` requires `{providerID, modelID}` and installed OMO sends a selected compaction model (`dist/index.js:94257-94263`, including an extra `auto`). `v1_session_summarize` deserializes the body only as `_body: Value` and calls `/api` compact, whose `SessionCompactExecution.model` comes from the session row instead. The route test sends `{providerID,modelID}` but its fake executor records only a call count, so it passes even though the requested model is discarded. This can compact with no model or the wrong session model.
- These are concrete product failures in three recorded plugin shapes, not requests for broader parity. Todo 165's shape test proves ordinary happy paths and response envelopes, but its input selection hides precisely the fields/part kind the installed plugins rely on. **Current provisional verdict is REJECT (F2-B6)** unless later evidence disproves these traces.

### Todo 165 mutations — registry and behavior fail differently, as claimed

- Baseline executions of `compat_v1_backed_sdk_routes_return_expected_catalog_and_session_shapes` and `compat_v1_backed_sdk_prompt_routes_preserve_sync_and_async_contracts` each ran exactly one named test and passed.
- **Behavior mutation:** temporarily changed the registered `/agent` adapter from `V1Adapter::Agents` to `V1Adapter::Providers`, leaving the route registered and counted as served. `cargo test -p oc-server --test compat_v1 compat_v1_backed_sdk_routes_return_expected_catalog_and_session_shapes --offline -- --exact --nocapture` failed because the response was not the bare agent array. This confirms route-level shape behavior is load-bearing, not merely registry presence. Restored immediately.
- **Registry mutation:** temporarily removed the `/agent` entry from `V1_BACKENDS`. `cargo test -p oc-server --test compat_v1 compat_v1_router_derived_coverage_counts_only_registered_backends --offline -- --exact --nocapture` failed with served count 10 versus 11. This separately proves the coverage/default table is guarded. Restored immediately.
- The two mutations validate the owner's methodological claim: table deletion and wrong behavior are caught by distinct tests. They do **not** cure F2-B6 because the behavior tests never submit the three real shapes described above.

### Todo 165 adversarial input probes — the missing contracts are observable

- **Recorded `tool_result` input:** temporarily replaced the sync prompt test's friendly text/file/agent parts with the exact Antigravity recovery shape `{type:"tool_result", tool_use_id, content}`. The named route-level test failed with HTTP 400 versus expected 200. This is not an invented schema case: the recorded installed callsite at `recovery.js:120-130` constructs and sends this part specifically to repair missing tool results. Restored immediately.
- **Load-bearing summarize body:** temporarily removed `Json(_body): Json<Value>` from `v1_session_summarize` while leaving the request body and all downstream behavior unchanged. `compat_v1_backed_sdk_routes_return_expected_catalog_and_session_shapes` still passed exactly. Thus the test's supplied `{providerID,modelID}` is provably ornamental: neither its presence nor its values affect the asserted behavior. Upstream's route handler passes those exact fields into compaction (`handlers/session.ts:282-290`); the Rust adapter instead selects `session.model`. Restored immediately.
- The session-create mismatch is likewise observable from production composition: `create_session` serializes the adapter's `body.model` verbatim, while `session_model` later requires `ModelRefBody {id, providerID}`. The current test submits `{modelID,providerID}` and never drives the child through `session_model`; the installed OMO callsites submit `{id,providerID,variant?}`. This is a false-friendly fixture rather than a failure of the raw pass-through for the real callsite, but it demonstrates why the existing green assertion cannot serve as wire-shape evidence.

### Todo 167 — the SDK model boundary is repaired and mutation-protected

- The released SDK `Model` contract and `ResolvedModel` agree on every retained field except one spelling and two optionalities. `providerID` maps locally to internal `provider_id`; SDK-optional `family` and `variants` deserialize through explicit defaults to `""` and `{}`. `id`, `api.{id,url,npm,endpoint}`, `name`, all capability flags and modalities, `cost`, `limit`, `status`, `options`, `headers`, and `release_date` retain the same spelling and compatible shape. SDK-only cost extras are harmless unknown fields under serde. The conversion is correctly confined to `HandleModelLoader`, preserving the canonical internal snake-case representation and still accepting an existing internal `provider_id` key.
- The two named tests are genuine production-path tests, not hand-built Rust round trips. They launch the real binary, import a JavaScript provider hook that constructs SDK-shaped values from scratch, omit `family` and `variants`, replace the catalog model map, resolve the selected model, and inspect recorded HTTP dispatch. The advertised `responses` endpoint overrides a Chat-hostile heuristic id, while advertised `chat` overrides a Responses-friendly id. Baseline: `cargo test -p oc-cli production_js_sdk_model_advertised --offline -- --nocapture` ran exactly both named tests and passed 2/2.
- **Behavior mutation:** temporarily bypassed the `plugin_model_value` normalizer by returning `model.clone()`. Both production-path tests failed by name with `Model not found: github-copilot/{catalog-id}`. This proves SDK-shape decoding is load-bearing at the requested JavaScript boundary. Restored immediately; source diff returned empty.
- The fixture also returns `malformed: {providerID: "github-copilot"}`. Successful dispatch proves a malformed sibling is isolated rather than rejecting its provider or valid sibling. The skip branch emits a debug event containing plugin, model id, and serde error.
- **Non-blocking test-honesty finding F2-O2:** temporarily removed only that debug event while preserving the malformed-entry skip. Both named production-path tests still passed 2/2. Thus the tests prove malformed-sibling isolation but do not prove the issues ledger's stronger implication that the malformed id/error is reported. The logging statement exists and is concrete, so this is not a product blocker; it is an unguarded observability contract. Restored immediately.
- **Disposition:** Todo 167's frozen acceptance criteria are closed. Both endpoint-precedence directions cross the real JavaScript hook and catalog replacement, removing SDK decoding fails them by name, and the corrected issues entry accurately distinguishes todo 162's direct Rust replay from this plugin-boundary proof.

### Cross-cutting seam hunt

- Repository-wide checks found one deliberately ignored test: the 500-turn real-driver soak, explicitly ignored because it takes at least two hours. The compatibility-suite meta-test rejects any declared behavioral witness whose named test is ignored, so this does not conceal an acceptance witness. I found no additional ignored functional regression masquerading as normal coverage.
- The strongest independently falsifiable seam is F2-O2: malformed JavaScript model entries are only reported through a `tracing::debug!` side effect, while the production-path tests observe only that a valid sibling still dispatches. Removing the event leaves both real-binary tests green. This is a concrete producer/consumer gap—the producer emits model id and decode error, but no test consumes either—and is therefore retained as the additional non-blocking test-honesty finding rather than inventing a second product blocker.
- No further blocker was found in the seam hunt. `F2-B6` remains sufficient for rejection; dormant retry wiring (F2-O1) and unguarded malformed-model diagnostics (F2-O2) remain ledger-quality follow-ups.

## Final validation

- The first unrestricted gate exceeded the 600-second command budget while still compiling/running the workspace and produced no test failure before termination; it is not counted as a pass or failure.
- The one permitted resource-bounded retry completed successfully:
  - `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace --offline --quiet`
  - `cargo clippy --workspace --all-targets --offline --quiet`
  - `cargo fmt --all --check`
  - `cargo build --workspace --offline --quiet`
- Every executed unit, integration, and doctest target passed; the repository's existing deliberately ignored tests remained ignored. Clippy, formatting, and workspace build completed with exit status 0.
- The LSP tool rejects sibling-worktree paths. I therefore byte-compared `bridge.rs`, `resolved.rs`, and `tool_turn.rs` against the primary checkout and required all three comparisons to succeed in the gate command, then ran diagnostics on those byte-identical primary-checkout files. All three returned `No diagnostics found`.
- Every temporary product/test mutation was restored before the gate. The final status emitted no product, test, plan, or documentation change; this ignored evidence report is the sole review artifact.

## Findings

### F2-B6 — BLOCKER: Todo 165 advertises plugin routes whose recorded payloads are not faithfully adapted

Todo 165's registry/accounting work is real, and its route-shape tests guard the friendly cases they submit. They do not guard the installed-plugin contracts used to justify the routes:

1. Antigravity recovery sends a `tool_result` prompt part, but the adapter rejects that recorded payload with HTTP 400. Recovery catches the error and silently returns `false`, so the nominally served prompt route does not provide its measured use case.
2. OMO summarize sends `{providerID, modelID, auto}`, but the adapter discards the body and compacts with `session.model`. Removing the body extractor leaves the existing test green, proving its model fixture is ornamental. The selected compaction model can therefore be ignored or replaced by no/wrong session model.
3. The session-create test submits `{providerID, modelID}`, although recorded OMO session creation uses `{id, providerID, variant?}` and the repository's session-row decoder requires `id`. The adapter's pass-through accepts the real callsite, but the test persists a corrupt fixture and never drives the stored value through its consumer. It cannot substantiate compatibility.

These are boundary-contract defects, not missing breadth or stylistic preferences. The first two are observable product failures at recorded installed-plugin callsites; the third is a concrete false-friendly test that would remain green with invalid persisted state.

### F2-O1 — non-blocking: retry helpers have no production integration

`ProviderRetryPolicy` and `retry_provider*` have callers only in tests/testkit. They do not amplify Todo 166's real-turn timeout path, but their public recovery semantics should either be wired deliberately or narrowed so future work does not assume they are live.

### F2-O2 — non-blocking: malformed plugin-model diagnostics are unguarded

Todo 167 correctly isolates a malformed model sibling and emits a debug event naming plugin, model, and decode error. Removing only that event leaves both production JavaScript boundary tests green. Isolation is protected; reporting is not.

## Required closure

1. Adapt the recorded Antigravity `tool_result` prompt shape through the real v1 prompt route and add a route-level regression using that exact payload.
2. Parse and honor summarize's requested `{providerID, modelID}` through actual compaction execution, with a test that distinguishes the body-selected model from `session.model`.
3. Replace the session-create fixture with the recorded `{id, providerID, variant?}` shape and consume the persisted model through the production decoder/execution path.
4. Re-run the existing registry and route mutants so the closure preserves both coverage derivation and friendly SDK behavior.

## Verdict

**REJECT.** Todos 166 and 167 close their frozen blockers with real production-path tests and killed behavioral mutants, and the complete offline workspace gate is green. Those facts do not override F2-B6: two routes recorded as served fail or ignore the exact installed-plugin payloads that motivated them, while a third compatibility assertion uses an invalid session-model fixture without exercising its consumer. The v1 adapter surface is therefore broader on paper than in executable compatibility.
