# F4 Scope Fidelity Review — Wave 12

**Verdict: REJECT**

**Audited HEAD:** `79ea3c3c` (`79ea3c3ca80a5d5a68e2690ba6dc0978911bf355`)

## Scope and method

This report compares the frozen plan and the user's standing constraints with the implementation at the audited HEAD. Evidence and judgments are recorded incrementally. Product source, tests, plans, documentation, and other evidence files are not modified.

## Incremental findings

- Audit initialization: `git rev-parse HEAD` returned `79ea3c3ca80a5d5a68e2690ba6dc0978911bf355`; `git status --short` was empty before this report was created.
- Wave-11 baseline: F4 rejected `b20ecbc9` because the real JavaScript provider boundary decoded `ResolvedModel` directly and therefore dropped the SDK spelling `providerID`; the required closure was modern SDK-shape decoding plus both endpoint-precedence directions through a real JavaScript hook, catalog replacement, `model_spec`, and recorded wire dispatch (`.omo/evidence/F4-REPORT-wave11.md:31-41,79-83`).

## Frozen request baseline

- Compatibility is defined as observable files/CLI/HTTP/database behaviour, and the product promises the complete modern plugin boundary rather than a reduced v1 subset (`.omo/plans/opencode-rust.md:33-50,65`). Existing JavaScript auth plugins must keep supplying the user's models (`:39`), and the six measured v1 SDK methods plus `/tui/show-toast` must be served (`:1463`).
- Todo 36 says provider retry is driven by typed `ProviderError`, emits `RetryRollback`, and ends only after a finite budget; its happy-path QA is specifically “a transient 503 is retried and the turn completes” (`.omo/plans/opencode-rust.md:481-487`).
- Todo 166 requires not only a visible timeout but that “the partial assistant text survives to the transcript” (`.omo/plans/opencode-rust.md:1435-1437`). Todo 167 requires the SDK model shape through a real JavaScript provider hook in both precedence directions and asks that all SDK/Rust differences be audited (`:1439-1441`).

## Todo 167 and the seventh layer

**Disposition of wave-11 blocker: the exact `HandleModelLoader` blocker is closed, but the plugin model boundary remains incomplete on two other already-shipped paths.**

- The direct repair is real and correctly local. `HandleModelLoader` now calls `plugin_model_value` before `ResolvedModel` deserialization (`crates/oc-plugin/src/js/bridge.rs:445-487`); that helper maps `providerID` to canonical `provider_id` only when the internal spelling is absent (`:554-565`). `ResolvedModel.family` and `.variants` now default (`crates/oc-llm/src/catalog/resolved.rs:47-78`). This preserves one internal snake-case serialization rather than adding a global alias.
- The field audit for the **v2 provider-hook return** is accurate. Released 1.18.15 declares `ProviderHook.models(provider: ProviderV2) -> Record<string, ModelV2>` (`/config/.bun/install/cache/@opencode-ai/plugin@1.18.15@@@1/dist/index.d.ts:164-170`); `ModelV2` spells the field `providerID`, makes `family` and `variants` optional, and otherwise matches the retained Rust fields apart from ignored SDK extras (`/config/.bun/install/cache/@opencode-ai/sdk@1.18.15@@@1/dist/v2/gen/types.gen.d.ts:1650-1729`).
- The two real-binary regressions construct SDK models from scratch, omit `family`/`variants`, import the actual `.mjs` hook, and prove advertised `responses` and `chat` each beat the hostile id heuristic (`crates/oc-cli/tests/tool_turn.rs:95-137,745-819`). Running `cargo test -p oc-cli --test tool_turn production_js_sdk_model_advertised --offline -- --nocapture` passed both tests. The corrected issues prose now says todo 162's direct Rust replay could not cover this seam (`.omo/notepads/opencode-rust/issues.md:7389-7411`), so the wave-11 overstatement was corrected honestly.
- **Seventh layer A — outgoing chat contexts still violate the same SDK spelling.** `chat_context_value` serializes `context.model` and `context.provider.info` directly (`crates/oc-plugin/src/jsonrpc.rs:1220-1236`), so a `ResolvedModel` reaches `chat.params`/`chat.headers` as `provider_id`. The released plugin contract requires legacy SDK `Model.providerID` on those hooks (`/config/.bun/install/cache/@opencode-ai/plugin@1.18.15@@@1/dist/index.d.ts:201-224`; legacy `Model` at `/config/.bun/install/cache/@opencode-ai/sdk@1.18.15@@@1/dist/gen/types.gen.d.ts:1278-1334`). The repository has a test that deliberately preserves the defect: it requires a model-only Kiro context to inject **no** headers and explains that Kiro survives only through `provider.info.id` fallback (`crates/oc-plugin/tests/js.rs:872-887`). Running that real-Kiro test passed, confirming the wrong spelling is current behaviour, not stale prose.
- **Seventh layer B — `Auth.loader` is a second plugin model ingress and has no SDK conversion.** Released 1.18.15 declares `loader(auth, provider: Provider)` (`plugin/index.d.ts:62-65`), whose legacy `Provider` requires `source`, optional `key`, and a model map of legacy SDK models (`sdk/dist/gen/types.gen.d.ts:1335-1347`). Production instead sends `ResolvedProvider` via raw serde (`provider_value`) and overwrites the internal provider with raw `serde_json::from_value` after JavaScript mutation (`crates/oc-plugin/src/js/bridge.rs:296-327,550-552`). `ResolvedProvider` has no SDK `source`/`key`, adds internal `availability`, and nests `ResolvedModel.provider_id` (`crates/oc-llm/src/catalog/resolved.rs:29-78`). A loader written to the declared SDK type therefore receives a non-SDK provider and cannot safely construct or insert an SDK model. Todo 167 normalized only the provider-hook return path.

These are demonstrable contract mismatches, not stylistic objections. The boundary-local design is sound only if every JavaScript SDK boundary explicitly projects to/from that canonical internal shape; current code does not.

## Todo 166 — idle timeout, transcript, and retry

**Judgment: the timeout reaches a real turn, but the checked todo quietly narrows two explicit promises.**

- The new tests exercise production `CompatibleProvider`/`ReqwestTransport` through `run_turn`, prove a stalled stream ends with visible partial text and an idle-timeout error, and prove per-chunk progress refreshes the allowance (`.omo/evidence/task-166-opencode-rust.txt:46-60`). The source shows the bound is consumed in the real loop.
- The promised transcript preservation is absent. `run_turn` inserts the assistant row, then `let event = next?` returns immediately on a stream error (`crates/oc-engine/src/loop.rs:704-712,768-785`); every `checkpoint_assistant` call is later (`:793-847`). The task's own real-binary database capture found the assistant row but no assistant text part (`.omo/evidence/task-166-opencode-rust.txt:19-44`). Calling durable preservation a “new seam above the requested user-visible proof” contradicts the frozen wording “survives to the transcript” (`.omo/plans/opencode-rust.md:1436`). This is omission relabelled as a scope boundary.
- The retry finding is also not honest completed scope. `ProviderRetryPolicy` and `retry_provider` are production-capable APIs (`crates/oc-engine/src/retry.rs:139-260`), but the only callers are tests/testkit; production `run_turn` calls `provider.stream(completion)` directly and returns the first provider error (`crates/oc-engine/src/loop.rs:768-785`). That violates Todo 36's explicit transient-503 retry contract and its recorded integration direction (`.omo/notepads/opencode-rust/issues.md:1881-1893`; `decisions.md:2526-2532`). Unlike an intentionally no-retry policy, this zero-production-caller surface is neither removed nor declared as a scoped divergence/gap.

## Todo 165 — pre-`/api` SDK routes

**Judgment: the ten adapters are appropriately scoped and shape-adapt existing handlers, but project-level v1 compatibility remains unmet and is only honestly recorded as a gap.**

- `V1_BACKENDS` is the actual registration source and contains ten `ApiAdapter`s plus the toast sink (`crates/oc-server/src/compat_v1.rs:474-526`); `v1_coverage()` derives `served` from that table (`:583-606`). Router selection uses the same registration (`:810-855`). This is stronger than editing a declaration.
- Each adapter invokes existing `/api` handlers/state rather than duplicating storage or turn logic: catalog adapters at `:931-964`, session list/create/get/abort/compact/messages at `:966-1054`, and sync/async prompt admission at `:1056-1155`. The tests send SDK field spellings and assert bare v1 envelopes, synchronous assistant return, asynchronous 204, and real side effects (`crates/oc-server/tests/compat_v1.rs:280-470`). Adapting these equivalent handlers is faithful to Todo 165's “shape adapter” rule, not a convenient redirect approximation.
- The remaining nine routes are correctly classified as a frozen **gap**, not a divergence. The shipped matrix derives 11/20 served and names all nine (`docs/compatibility-matrix.md:85-90`); the capture explicitly says installed auth plugins cannot authenticate because `auth.set` and both OAuth calls remain unbacked (`docs/v1-surface-capture.md:180-202`). No divergence entry launders them.
- Honest classification does not make the standing requirement true. The success criterion says the measured v1 SDK methods plus toast “are served” (`.omo/plans/opencode-rust.md:1463`), while the product itself reports that the installed auth plugins cannot authenticate (`docs/v1-surface-capture.md:198-202`). The implementation chose not to invent lower-level semantics, which was the correct local decision, but checking the overall work complete while retaining those user-required routes as gaps is still an omission against the drop-in/plugin promise (`plan:5,39,58`).

## Completed-todo count and standing constraints

- An anchored count of numbered checked rows (`^- \[x\] [0-9]+\.`) in the frozen plan returns **167**. The rows contain every implementation number 1 through 167; F1-F4 remain four separate unchecked review rows (`.omo/plans/opencode-rust.md:1431-1446`). The check marks therefore report 167 completed implementation todos, but the findings above show that at least Todos 36 and 166 do not satisfy their frozen observable acceptance.
- The newest-installed-release rule remains correctly represented: criterion 1 names released 1.18.15 as the current differential oracle while retaining 1.18.13 only as the plugin compatibility identity (`.omo/plans/opencode-rust.md:1458-1459`; wave-11 baseline at `.omo/evidence/F4-REPORT-wave11.md:20`). I found no attempt in Todos 165-167 to replace that oracle with a stale package-manager path or self-authored fixture.
- The modern `/api` scope is implemented rather than converted into declarations. The executable gate compares the generated operation set with the captured 58-operation oracle and requires no missing operation; the only additions are the two declared C8 methods (`crates/oc-testkit/tests/compat_suite.rs:2268-2309`). The shipped matrix still truthfully labels ten registered operations as explicit `503 backend_unavailable` gaps (`docs/compatibility-matrix.md:61-65`), matching the owner-approved narrowing rather than pretending they behave.
- The no-legacy-compatibility-layer rule is not violated by Todo 165's adapters: the plan itself requires the measured pre-`/api` minimum for resident JavaScript plugins (`.omo/plans/opencode-rust.md:58,631-636`). The problem is incomplete required behavior, not the existence of those narrowly registered adapters.
- `execute` remains the requested jcode-style structured composition, not an undeclared JavaScript interpreter. Its production schema is derived from `ExecuteParams`/`Subcall`, bounded at ten calls, executes dependency levels in parallel, supports binding/fan-out, and rejects recursive `execute` (`crates/oc-tools/src/batch.rs:17-49,68-184`). The compatibility gate validates that live schema against the declared contract (`crates/oc-testkit/tests/compat_suite.rs:2385-2464`).
- The built-in omo surface remains intentionally slim rather than silently absent. `builtin.rs` records why six named agents survive the nine-agent slim reference and why designer/council roles were dropped, while representing negative delegation boundaries and output envelopes as required data (`crates/oc-agent/src/builtin.rs:1-52,180-280`). Model selection inherits the session model unless user preset data overrides it and keeps categories as preset shorthand rather than compiled model chains (`crates/oc-agent/src/model_policy.rs:1-66,186-226`). The `task` schema exposes agent/category selection, per-child model and effort, background execution, and `task_id` continuation (`crates/oc-tools/src/task.rs:104-164`). These are reasoned product choices, not an omitted plugin load.

## Intentional-divergence audit

The registry contains **17** entries and `DECLARED_COUNT` is 17 (`docs/divergences.toml:61-154`; `crates/oc-testkit/src/divergence.rs:40-65`). I reviewed each entry against its stated surface, implementation, and available liveness assertion:

1. `session-list-default-sort` — accepted affirmative default choice, with an explicit created-time opt-out.
2. `tool-output-filename-carries-session` — accepted added attribution required by retention GC.
3. `no-eager-directory-creation` — accepted lazy side-effect policy, not a missing path API.
4. `split-version-identity` — accepted compatibility/build identity split.
5. `execute-parameter-contract` — accepted requested jcode contract; its live schema is machine-checked.
6. `c8-maintenance-endpoints` — accepted added scope; the operation-set gate fixes the extra set to exactly two.
7. `provider-coverage-by-wire-family` — accepted only as the explicit rejection policy for an **unknown** `api.npm` transport. It cannot excuse the SDK-shape defects on already implemented provider/plugin families identified above.
8. `cross-session-resident-memory` — accepted added subsystem with one strict-parity kill switch.
9. `session-subpath-is-applied` — accepted deliberate implementation of upstream's live no-op, including literal prefix semantics.
10. `context-md-excluded` — accepted deliberate modern-only instruction cascade.
11. `malformed-auth-json-is-an-error` — accepted fail-closed data-preservation policy.
12. `failed-format-restores-pre-format-bytes` — accepted rollback policy after a failed formatter.
13. `non-pure-plugin-generated-trees` — accepted solely for the explicitly narrowed non-pure third-party-generated trees; it does not excuse missing declared SDK fields on hooks this host executes.
14. `plain-cli-presentation` — accepted four bounded presentation differences, each re-derived from both binaries.
15. `diagnostics-name-their-cause` — accepted actionable diagnostic policy with two-sided witnesses.
16. `session-list-output-shape` — accepted added global-list information and measured non-empty shape difference.
17. `non-vcs-plan-glob-is-absolute` — accepted correction of a nonfunctional non-VCS relative glob, measured on both binaries.

No entry directly launders the current omissions. The gate's `behavioural_differences()` binds seven concrete records to entries and named tests (`crates/oc-testkit/tests/compat_suite.rs:2761-2839`), but that registry is not an exhaustive discovery mechanism. The current compatibility matrix itself records three implementation gaps beyond the ten owner-approved `/api` backends: assistant step-boundary parts, nine v1 routes, and a channel-dependent database filename disclosure (`docs/compatibility-matrix.md:73-89`). Of these, the required auth/OAuth v1 routes and the missing durable partial-text checkpoint are scope blockers; neither should become an eighteenth divergence.

## Required closure

1. **Complete the JavaScript SDK projection in both directions.** Project `ResolvedModel` to the declared SDK `Model` shape (`providerID`) before `chat.params`/`chat.headers`, and give `Auth.loader` the declared SDK `Provider` shape with a boundary-local conversion back into canonical Rust models. Add real JavaScript tests that construct and read those declared shapes; tests must fail on `provider_id` leakage, missing `source`/`key` semantics, or SDK models being dropped. Do not add global serde aliases that blur internal and plugin wire formats.
2. **Preserve partial assistant text durably on stream failure.** Before propagating an idle timeout or other stream error, checkpoint already accumulated assistant parts so reopening/listing the session shows the same partial text the user saw. The real-turn stalled-socket test must inspect the database transcript, not only `TextDelta`/stdout, and the slow-progress control must remain green.
3. **Wire typed provider retry into the one production turn loop.** A transient retryable 503 must enter the finite `ProviderRetryPolicy`, emit `RetryRollback`, clear attempt-local accumulation as designed, and complete when the next attempt succeeds; budget exhaustion must remain finite and actionable. A production-path test must fail if `run_turn` again calls `provider.stream` once and returns the first retryable error.
4. **Serve the frozen plugin-critical v1 auth surface or explicitly renegotiate the standing criterion.** At minimum `auth.set` and both provider OAuth operations used by the installed auth plugins must do real work with their measured SDK envelopes. Do not replace their 501s with adapters to unavailable `/api` stubs or reclassify them as divergences. If the owner chooses to narrow this requirement instead, the success criterion and day-one plugin promise must be changed explicitly before F4 can approve.

## Validation gates

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --offline`, and `cargo build --workspace --offline` all completed successfully in the first chained validation command. Clippy emitted no warning.
- Workspace test status is **not a completed pass**. Attempt 1 used `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace --offline -- --test-threads=1`; every reported test passed, but the command was terminated by its 600-second outer timeout while `oc-config` differential tests were still running. Attempt 2, the final status check permitted by the review instructions, used `CARGO_BUILD_JOBS=1 cargo test --workspace --offline --quiet`; it advanced through the workspace with zero assertion failures until the host again exhausted its process/thread allowance in `oc-tools`, reporting `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }` while listing tests and a consequent harness `SendError`. No third attempt was made. This is a host-resource limitation, but the incomplete suite is not represented as green.
- The focused Todo 167 command had already passed both real-JavaScript endpoint-precedence tests during this audit, and the real-Kiro context control passed as recorded above. These focused results do not substitute for the incomplete workspace gate.

## Final disposition

**REJECT.** Wave 11's exact `HandleModelLoader` defect is closed with real JavaScript endpoint-precedence tests, all 17 registered divergences remain defensible choices, the modern `/api` operation set is implemented under its frozen narrowing, and the jcode `execute` plus slim built-in omo choices remain faithful. Approval is nevertheless impossible: the JavaScript boundary still emits/accepts non-SDK model/provider shapes on two production hooks; Todo 166 visibly returns partial text but does not preserve it in the transcript; Todo 36's retry implementation has no production caller; and plugin-critical v1 auth/OAuth routes remain explicit gaps despite the standing day-one compatibility promise. The four closure conditions above are concrete and independently testable. This audit makes no product, plan, or documentation fix.
