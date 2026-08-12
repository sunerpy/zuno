# F2 Code-Quality Review — Wave 13

- **Audited HEAD:** `67167fe6`
- **Role:** F2, code-quality and test-honesty reviewer
- **Verdict:** **REJECT**
- **Scope constraint:** Review only. All temporary product/test mutations will be restored; no fixes will be made.

## Planned probes

1. Re-read wave 12's blocker and observations, then map todos 168–171 against the frozen plan, issue catalogue, and owner notes.
2. Verify todo 169 with the exact recorded Antigravity, OMO summarize, and session-create payloads; mutate producer/default/route behavior rather than registry entries and require the named regressions to fail.
3. Scrutinize todo 168's rewritten timeout test and both CLI/HTTP hook-failure recovery; mutate disable/restart/error behavior and the version-gate branch.
4. Independently enumerate every JS model/provider ingress and egress boundary behind todo 170, then mutation-test the exhaustive classifier and diagnostic observability.
5. Verify todo 171's v1 `Session.slug` projection and required-key derivation from both schemas; preserve the `/agent` unverifiable ruling unless evidence closes it.
6. Re-check F2-O1's zero-production-caller retry helpers, F2-O2's previously unguarded malformed-model diagnostic, ignored/silently skipped tests, unreachable errors, silent-drop branches, and tests asserting only their own fixtures.
7. Audit the workspace-wide zero-unsafe policy (`unsafe_code = "forbid"`, 36 opted-in crates, zero first-party `unsafe`) and judge the lint-inheritance failure message as a policy guard.
8. Hunt for a twenty-third seam by questioning payload shapes and mutating real producers, defaults, and behaviors with observable, non-equivalent mutants.
9. Restore every mutation, require clean status except this report, and run `cargo test --workspace --offline`, `cargo clippy --workspace --all-targets --offline`, and `cargo fmt --all --check` once successfully.

## Incremental findings

### Baseline and prior-ruling freeze

- `git rev-parse HEAD` returned `67167fe681e6bd954a5a6fd5e2e6dd8384a74364`, matching the assigned audited HEAD. Initial status contained only this newly created review report.
- Wave 12's blocker is frozen as three independently testable contracts: Antigravity's exact `tool_result` prompt part must reach prompt execution; OMO summarize's exact `{providerID, modelID, auto}` choice must override a different `session.model`; and session create's exact `{id, providerID, variant?}` value must survive persistence and its production decoder/consumer. Friendly neighboring payloads do not count.
- Wave 12's observations remain separate probes: F2-O1 is about production call reachability, while F2-O2 is about a diagnostic consumer. Neither will be inferred from the wave-60 prose.
- The wave-60 ledger claims 3443/0 and records both the prior `2088/1` output anomaly and the zero-unsafe mutations. I treat those as leads, not verification; this report will record its own runs and mutants.

### Todo 169 — recorded session-create contract is load-bearing

- Baseline ran each of the three named route regressions exactly once; all passed. The create test submits the installed `{id, providerID, variant}` model inside the recorded create envelope, asserts the exact persisted JSON, then prompts the child without an explicit model and inspects the production `session_model` result passed to the mutation executor. It therefore no longer stops at its own fixture.
- **Mutation 169-A:** changed the actual `SessionCreateInput.model` application point to `None` while leaving route registration and the test body intact. `compat_v1_omo_session_create_persists_and_consumes_the_recorded_model_shape` failed by name at the persisted-row assertion (`None` versus `{"id":"deepseek-chat","providerID":"deepseek","variant":"fast"}`). The source was restored immediately. This is an observable producer/application mutation; the compile warning was incidental, while the named behavioral assertion was the failure relied upon.

### Todo 169 — summarize body is consumed, not ornamental

- The exact installed shape `{providerID:"deepseek", modelID:"deepseek-chat", auto:true}` reaches `compact_session`, whose execution request is recorded after model selection. The test inspects both the selected model and `automatic`; it is not a call-count assertion.
- **Mutation 169-B:** replaced the body-selected model and `auto` application with `None`/`false`, preserving the route and typed extractor. `compat_v1_omo_summarize_uses_the_recorded_body_model` failed by name: the captured `SessionCompactExecution` contained `model: None, automatic: false` instead of the body's DeepSeek selection and `true`. Restored immediately. This directly closes wave 12's proof that the request body could previously be removed without consequence.

### Todo 169 — exact Antigravity shape is accepted, with a proof-scope caveat

- **Mutation 169-C:** removed only the production `tool_result` arm from `prompt_body`, preserving route registration and every friendly prompt arm. `compat_v1_antigravity_recovery_tool_result_reaches_the_prompt_executor` failed by name with HTTP 400 versus 200. The arm and its pre-existing rationale comment were restored byte-for-byte immediately. This independently confirms the plan owner's named mutation claim.
- The test submits the exact installed object and requires its cancellation content at the shared prompt executor, so wave 12's caller-swallowed 400 is closed. Its executor is nevertheless a `CompletingMutationExecutor`, not `run_turn`, and the fixture seeds no unfinished tool call; consequently this test does **not by itself** prove that the supplied `tool_use_id` identifies and repairs a stored call. The adapter validates but discards that id and relies on `run_turn`'s general `repair_missing_tool_outputs` pass. I retain this as a seam-hunt probe rather than treating route acceptance as end-to-end recovery proof.

### F2-B7 — BLOCKER: the documented incompatible-plugin skip is still the opposite of production

- Todo 168 correctly identified this contradiction but did not repair it. Current `load_one` starts the host and builds `JsPlugin` **before** examining `resolved.gate()`. Its `VersionGate::Unsatisfied` arm creates a `Compatibility` warning and then returns `Ok((plugin, host, warning))`; `load_js_plugins_ordered` pushes that plugin into the active list.
- The production policy is explicit in `spec.rs`: `Unsatisfied` says **"Load anyway, but say so."** The public contract at `docs/plugin-authoring.md:37-40` says an npm plugin whose `engines.opencode` excludes the running version is **skipped, upstream's behaviour**. Upstream 1.18.15's `checkPluginCompatibility` reads `package.json.engines.opencode` and throws when the range excludes the running version.
- The Rust gate does not model `engines` at all. `PackageManifest` instead reads `dependencies`/`peerDependencies` and compares `@opencode-ai/plugin` against `1.18.13`. This is not an equivalent spelling: the installed Antigravity package has no `engines.opencode` (so upstream admits it) but depends on `@opencode-ai/plugin ^0.15.30` (so Rust classifies it unsatisfied and admits it only through the documented-opposite load-anyway branch).
- Test honesty is incomplete at precisely this seam. `js_version_gate_records_an_incompatible_peer_range` stops after asserting the `Unsatisfied` enum; it never calls `load_js_plugins_ordered` or asserts the documented skip. Conversely, `js_real_supported_plugins_load_with_their_own_sdk_clients` requires both installed plugins active and therefore demonstrates current loading, but does not reconcile that with the claimed `engines.opencode` rule. A user package that truly declares an excluding `engines.opencode` range is not even classified by this gate and will load.
- This is a product/documentation compatibility contract, not style. It also invalidates the startup diagnostic's implied containment: incompatible code executes and can reach hooks; todo 168 only contains it after its first runtime failure. **Provisional verdict is REJECT.**

### Todo 168 failure-containment test honesty

- Baseline passed for the JS timeout test and both CLI/HTTP failing-`tool.definition` integration tests.
- Mutation: removed `&& !plugin.is_disabled()` from `HookRunner::run`, allowing the generic hook error to escape after `JsPlugin::call` had already permanently disabled and diagnosed the plugin. The CLI regression failed exactly at its containment assertion: stderr gained `plugin hook failed: plugin production-noop-tool-definition failed in hook tool.definition` after the expected one-time disable diagnostic. This kills the mutation and proves the integration test distinguishes contained plugin failure from a turn-level failure. The short-circuit prevented the HTTP test from running, but one killed mutation is sufficient for this shared branch; the source was restored byte-for-byte immediately afterward.
- Disposition: timeout permanent-disable and CLI/HTTP continuation coverage are credible. This does not close F2-B7, which concerns incompatible code being admitted before runtime containment.

### Todo 170 — exhaustive JavaScript SDK boundary projection is credible

- The implementation evidence enumerates eleven model/provider crossings with direction, SDK generation, and pre-fix projection status. Independent source review found the ordinary-hook authority in the exhaustive `HookModelBoundary::classify` match, the five carrying hook classes routed through `projection.rs`, and the two resource callback families (`Auth.loader`, `ProviderHook.models`) reusing the same typed projectors/decoders. No second raw `ResolvedModel`/`ResolvedProvider` JavaScript codec was found in the audited source set.
- Baseline `cargo test -p oc-plugin --test js js_sdk_boundary --offline -- --nocapture` passed 3/3 real-`.mjs` regressions, covering ordinary hooks, `Auth.loader`, and `ProviderHook.models` in both directions.
- Independent mutation: changed only shared legacy chat-context `provider.info` projection from `SdkGeneration::Legacy` to `V2`. `js_sdk_boundary_ordinary_hooks_read_declared_shapes_and_supply_small_model` failed by name in the real plugin at `chat.params.provider.info.models.sdk-input-model`, identifying leaked `family,release_date,variants`. This is a non-equivalent SDK-generation error rather than field removal, and proves the callback asserts the precise released shape. The source was restored immediately.
- Disposition: todo 170's boundary enumeration, compile-time classification guard, and real-plugin projection tests are sufficient for this review.

### Todo 171 — v1 `Session.slug` projection and `/agent` disposition are credible

- Source and test review confirms `v1_session` carries `session.slug` directly rather than deriving it from `id`. The regression fetches this build's `/doc` through the real router, independently parses the committed oracle, maps only the documented legacy ID-key spellings, first requires both derived `Session.required` sets to agree, and then requires every derived key to be present and non-null on POST, GET-by-id, and a non-empty GET-list response. It is not a hand-written `slug` assertion.
- Independent mutation: removed only `"slug": session.slug` from the production projection. `compat_v1_session_projection_satisfies_the_published_session_schema` failed by name on `POST /session`, naming the missing schema-required key and the exact served key set. The source was restored immediately.
- The `/agent` observation is correctly kept separate and falsifiable: the witness proves all oracle-required keys are served, fails if this build begins publishing an `Agent` schema, and pins the optional extra/missing sets against the only committed capture. The corresponding `v1-agent-projection-unverified` entry is generated from `known_gaps`, not misclassified as a chosen divergence. With no target-version capture, retaining it as a frozen gap is more honest than inventing a shape.
- Disposition: todo 171 closes the lossy required-field defect and records the lower-confidence `/agent` drift at the appropriate evidence level.

### Prior observations and zero-unsafe policy

- **F2-O1 remains open and non-blocking:** repository-wide caller inspection still finds `ProviderRetryPolicy`/`retry_provider*` only in `oc-engine`'s implementation, `oc-engine/tests/retry.rs`, and `oc-testkit/src/cassettes.rs`. The production turn loop continues to call the provider stream directly. This dormant public recovery facility neither hides nor amplifies the real-turn timeout behavior, but it still must not be described as integrated product retry.
- **F2-O2 remains open and non-blocking:** independently removed only the malformed-model `tracing::debug!` event from `HandleModelLoader`, preserving sibling isolation. Both `production_js_sdk_model_advertised_*` real-binary tests still passed 2/2. They prove a malformed sibling does not suppress a valid model, but no test consumes the promised plugin id, malformed model id, or decode error diagnostic. The event was restored immediately.
- The zero-unsafe policy has two independent executable layers: root `unsafe_code = "forbid"` plus a manifest-inheritance guard, and a first-party `crates/*/src/**` source scan that also protects a crate which failed to inherit workspace lints. `no_first_party_source_file_uses_unsafe` and `every_workspace_member_inherits_the_workspace_lints` both passed. Scanner self-tests cover keyword positions, prose/identifier false positives, string literals containing `//`, and missing/incorrect lint inheritance. This is a credible workspace policy rather than a root-manifest assertion alone.

### F2-O3 — Antigravity route test does not prove `tool_use_id` pairing

- The installed Antigravity recovery code extracts every `tool_use` id from the failed assistant message and submits `{type:"tool_result", tool_use_id, content}`. The adapter requires a string id but discards it, converts only `content` into a new user prompt, and relies on `run_turn`'s generic session-wide `repair_missing_tool_outputs` pass.
- The engine regression `loop_repairs_a_missing_tool_result_before_the_provider_sees_history` passed and proves that generic pass pairs an actual unfinished `call-orphaned` record with an interrupted synthetic result before provider dispatch. However, it does not run through the v1 route.
- Independent test-input mutation changed the route fixture's `tool_use_id` from `call_recorded_antigravity` to `call_deliberately_wrong`; `compat_v1_antigravity_recovery_tool_result_reaches_the_prompt_executor` still passed. Thus the named todo-169 regression proves exact object acceptance and cancellation-text delivery, not that the submitted id selects or repairs its stored call. The fixture was restored immediately.
- This is retained as a non-blocking proof-scope gap because the production composition repairs all unfinished calls after a session abort and the exact recorded call succeeds; no contrary live product outcome was demonstrated. A stronger regression should seed multiple/targeted unfinished calls and drive the real `ServerSessionMutationExecutor` so the intended equivalence is explicit.

## Final validation

- The first `cargo test --workspace --offline` run reached the workspace tests but the host exhausted its process/thread allowance: Rust's test harness reported `Resource temporarily unavailable` / `WouldBlock`, followed by secondary `SendError` panics after the harness channel closed. This was an environmental execution failure, not a product assertion failure.
- The one permitted resource-bounded retry, `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace --offline`, completed successfully across unit, integration, compatibility, and doctest targets. The documented two-hour soak remained ignored, and one documentation example remained ignored; neither is an acceptance witness masquerading as an ordinary pass.
- `CARGO_BUILD_JOBS=1 cargo clippy --workspace --all-targets --offline` completed successfully with no warning or error.
- `cargo fmt --all --check` completed successfully with no output.
- Changed-file LSP diagnostics are not applicable to the sole persistent change, this Markdown evidence report. The LSP tool was attempted and explicitly rejected the sibling-worktree path as outside its configured request cwd. Every temporary Rust source/test mutation was restored before the successful workspace gates, so there is no changed Rust file requiring a diagnostic waiver.

## Findings

### F2-B7 — BLOCKER: plugin compatibility policy is implemented against the wrong manifest contract and does not skip incompatibles

The loader's actual gate reads `dependencies`/`peerDependencies["@opencode-ai/plugin"]`, compares it with a hard-coded reported SDK version, and deliberately loads `VersionGate::Unsatisfied` plugins with a warning. The documented/upstream contract instead reads `engines.opencode` and skips a package whose range excludes the running OpenCode version. Consequently an actually incompatible `engines.opencode` declaration is ignored and loaded, while an ordinary SDK dependency can be classified as incompatible under a different rule. Existing tests assert the enum classification and that installed plugins load, but never drive an excluding `engines.opencode` package through the loader and require exclusion. Runtime disable-after-first-failure does not repair admission of code the compatibility gate promised to skip.

### F2-O1 — non-blocking: provider retry helpers remain unintegrated

`ProviderRetryPolicy` and `retry_provider*` still have no production caller. Their isolated behavior is tested, but product turn execution does not use them.

### F2-O2 — non-blocking: malformed plugin-model diagnostics remain unguarded

Malformed sibling isolation is protected, but deleting the debug event that names the plugin, model id, and decode error leaves both real provider-hook tests green.

### F2-O3 — non-blocking: Antigravity recovery's submitted tool id is not covered end to end

The route accepts and validates `tool_use_id` but discards it; changing the fixture to a wrong id leaves the route regression green. A separate engine test proves session-wide unfinished-call repair, not the composed v1 route plus real server executor.

## Required closure

1. Model the released compatibility contract from `package.json.engines.opencode`, and make an excluding range prevent plugin activation as documented/upstream does.
2. Add a production loader regression with an excluding `engines.opencode` package; require zero active plugin callbacks and an actionable compatibility diagnostic. The regression must fail if the loader merely warns and continues.
3. Reconcile or remove the `@opencode-ai/plugin` dependency/peer-dependency gate so it is not presented as equivalent to the OpenCode runtime-version contract.
4. Ledger F2-O1/F2-O2 and strengthen the Antigravity recovery test with stored unfinished calls plus the real server mutation executor; these observations are not required to clear F2-B7.

## Verdict

**REJECT.** Todos 168–171 otherwise have credible behavior tests and killed mutations: failure containment, exact recorded v1 payload handling, exhaustive JS SDK projection, and schema-derived `Session.slug` coverage all held under independent probes. The complete resource-bounded workspace test gate, Clippy, and formatting are green. Those strengths do not override F2-B7: the public compatibility policy says incompatible npm plugins are skipped, while production checks a different manifest field and explicitly loads the `Unsatisfied` result. This is a user-visible product/documentation contract failure at the plugin admission boundary.
