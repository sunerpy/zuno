# F4 Scope Fidelity Review — Wave 13

- **Audited HEAD:** `67167fe681e6bd954a5a6fd5e2e6dd8384a74364` (required short SHA: `67167fe6`)
- **Verdict:** **REJECT**
- **Role:** Scope fidelity reviewer; no product, test, plan, documentation, or pre-existing evidence files modified.

## Scope and method

Comparison of the frozen plan and stated constraints against the delivered implementation, with independent source inspection, released `@opencode-ai/plugin@1.18.15` / `@opencode-ai/sdk@1.18.15` declaration inspection, and executable verification. The starting worktree was clean apart from this report: `git rev-parse HEAD` returned `67167fe681e6bd954a5a6fd5e2e6dd8384a74364`, and the initial `git status --short` was empty.

## Frozen request baseline

- The headline promise is a swappable binary whose existing sign-in plugins “keep working unchanged” (`.omo/plans/opencode-rust.md:5`), and C5 requires the JavaScript compatibility host plus all advertised hooks (`:39`). Compatibility is observable files/CLI/HTTP/database behavior (`:33`), not an internal-type resemblance.
- Todo 170 explicitly requires projection at “EVERY JavaScript boundary,” both ingress and egress, forbids fixing only F4's two examples, and requires an enumeration plus a structural check (`.omo/plans/opencode-rust.md:1451-1453`).
- Todo 168 requires failed hooks to disable the plugin with a visible diagnostic rather than fail the turn (`:1443-1445`). Todo 169 requires the three exact recorded plugin bodies and says a route whose real callsite still fails must be demoted (`:1447-1449`). Todo 171 requires `slug` and says `/agent` drift must be fixed or frozen with a reason (`:1455-1457`).
- Standing constraints include the newest installed release as oracle (`:1475`), all upstream `/api` operations registered and no 501 with the frozen backend-gap policy (`:1479`), no legacy/deprecated compatibility (`:52-65`), zero first-party `unsafe` (`:61,1487`), jcode-style `execute` (`:44`), and built-in but deliberately slimmed omo capability (`:45`).
- Wave 12's exact F4 blocker was boundary incompleteness: outgoing chat contexts and `Auth.loader` still crossed the JavaScript SDK boundary in canonical Rust spelling (`.omo/evidence/F4-REPORT-wave12.md:24-32`). The acceptance bar was explicit projection at every such boundary, not merely the two named examples (`:84-87`).

## Landed change map

The post-wave-12 implementation commits are `b220c546` (Todo 168), `b24d50f2` (169), `35cda951` (170), `33107207` (171), and `d3539b65` (zero-unsafe policy message). `git diff --name-only 79ea3c3c..67167fe6` shows their product changes are confined to the plugin runtime/bridge, v1 adapters/session execution, compatibility-gap inventory, and policy test named by the tasks; there is no unrelated product expansion.

## Todo 170 — callback repair is real, but the enumeration is not complete

**Judgment: the twelve mutation points repair the callback paths they name, but Todo 170's claim to cover “EVERY JavaScript boundary” is false. An eighth provider/model layer exists: the real SDK client injected as `PluginInput.client`.**

The delivered callback codec is locally sound. `HookModelBoundary::classify` exhaustively classifies every `HookInvocation` and forces the five model/provider-bearing ordinary hook families through `model_value`, `provider_value`, or `chat_context_value` (`crates/oc-plugin/src/js/projection.rs:14-49`; `crates/oc-plugin/src/jsonrpc.rs:959-1080`). `Auth.loader` now receives and returns a projected legacy `Provider`, while `ProviderHook.models` receives a projected `ProviderV2` and decodes returned `ModelV2` values (`crates/oc-plugin/src/js/bridge.rs:299-341,454-503`). The shared codec emits `providerID`, removes internal-only egress fields, restores legacy-inexpressible canonical fields after a loader mutation, and preserves the canonical internal snake-case representation (`projection.rs:59-205,226-255`). The real-JavaScript tests and the twelve recorded projection mutants are credible evidence for those paths.

However, the completeness argument stops at `JsHost::call`/`call_mutating` and `HookInvocation`. The host also loads the plugin's own real `@opencode-ai/sdk` and constructs `PluginInput.client` with `baseUrl: params.serverUrl` (`crates/oc-plugin/src/js/shim.mjs:273-310,342-380`). The released plugin contract declares that client as part of `PluginInput` (`@opencode-ai/plugin@1.18.15/dist/index.d.ts:36-45`). Calls through it cross JavaScript ↔ Rust over HTTP and can carry providers and models, so they are the same observable SDK boundary class even though they are not callback arguments.

At least one omitted boundary is live, not hypothetical: installed `@sunerpy/oh-my-openagent@4.21.0` calls `client.provider.list()` at `dist/index.js:26958,84674`, reads `data.all[*].models`, and caches the returned model metadata. The repository itself records this callsite and serves it as `GET /provider` (`crates/oc-server/src/compat_v1.rs:311-319`; `docs/v1-surface-capture.md:55-76`). The generated SDK response is a specialized catalog projection with model fields including `release_date`, `reasoning`, `temperature`, `tool_call`, `cost`, `limit`, and `modalities` (`@opencode-ai/sdk@1.18.15/dist/gen/types.gen.d.ts:2576-2644`). It is therefore unambiguously a JavaScript model/provider egress.

That live boundary is neither routed through `projection.rs` nor covered by the structural guard. Its separate `v1_model` adapter converts the already-epoch-millisecond `/api/model` release value with `.to_string()`, hardcodes `reasoning: false` and `temperature: true`, and keeps only the first cost band (`crates/oc-server/src/compat_v1.rs:1260-1300`; the source catalogue retains date strings and actual reasoning/temperature values, e.g. `crates/oc-llm/tests/fixtures/models-dev-pinned.json:33-74`). The only route assertion checks that `id` is a string and `models` is an object (`crates/oc-server/tests/compat_v1.rs:358-373`); no real plugin test validates the declared model fields, and removing or corrupting any of these projections would not trip `HookModelBoundary`.

The broader omitted class also includes generated client methods such as legacy `config.providers()` and v2 `client.v2.model.list()` / `client.v2.provider.list()` when a plugin calls them; the installed OMO call above is sufficient to establish the blocker without relying on those currently unrecorded methods. Thus there is no defensible twelve-out-of-twelve proof: the delivered evidence proves twelve callback projection mutations, not all JavaScript model/provider boundaries. This is an **in-scope omission and observable compatibility defect**, not an intentional divergence.

## Todo 168 — the reported turn-killing callback defect is closed

**Judgment: faithful and complete for the frozen acceptance.** The exact `tool.definition` failure that previously killed every configured turn is now contained at the plugin boundary. `JsPlugin::call` disables the plugin on encode, host-call, returned-output, or write-back failure and records the hook plus underlying cause; later calls are no-ops, and disabled resource collectors no longer expose that plugin's tools, auth hook, or provider hook (`crates/oc-plugin/src/js/plugin.rs:96-139,141-188`). `HookBus` returns a callback error only when the failing implementation did **not** disable itself, preserving hard failures for plugin types that do not claim containment while allowing the JavaScript host's documented degradation policy (`crates/oc-plugin/src/hooks.rs:196-264`).

The diagnostic is not log-only. `PluginRuntime::take_diagnostics` derives a default-visible message naming plugin, hook, and cause (`crates/oc-cli/src/cmd/plugin_runtime.rs:130-151`), and the turn driver publishes it on the ordinary event stream (`crates/oc-cli/src/cmd/turn.rs:860-884`). The production CLI regression requires a successful turn, one failing callback, no later callback, and stderr containing plugin id, `tool.definition`, and `truncated`; the HTTP regression requires `turn.completed`, no `session.error`, one callback, and the same diagnostic over SSE (`crates/oc-cli/tests/tool_turn.rs:1408-1474`; `crates/oc-cli/tests/session_mutation.rs:827-896`). The direct timeout regression also proves permanent disablement and no restart (`crates/oc-plugin/tests/js.rs:793-849`). Todo 151's refusal to commit a truncated write-back remains intact (`crates/oc-plugin/src/js/plugin.rs:316-339`).

## Todo 169 — the three recorded installed-plugin payloads are consumed

**Judgment: faithful and complete for the three frozen callsites.** The adapter no longer tests friendly substitutes:

- Session creation decodes the recorded `{id, providerID, variant?}` model as `ModelRefBody`, persists that exact SDK spelling, and the regression consumes it again through a child prompt (`crates/oc-server/src/compat_v1.rs:900-911,999-1024`; `crates/oc-server/tests/compat_v1.rs:657-724`).
- Summarize requires `{providerID, modelID, auto?}` and forwards both the selected model and `automatic` flag into the shared compact execution (`compat_v1.rs:932-940,1044-1061`; `crates/oc-server/src/api/session.rs:781-811`; regression at `crates/oc-server/tests/compat_v1.rs:726-758`).
- The Antigravity `tool_result` shape validates `tool_use_id` and string `content`, then carries the cancellation content into the real prompt executor; removing the arm returns to the recorded HTTP 400 failure (`compat_v1.rs:1136-1197`; regression at `crates/oc-server/tests/compat_v1.rs:760-786`).

These routes remain classified as served because all three named effects are observed, not merely because deserialization succeeds. I found no payload substitution or route-status laundering in Todo 169.

## Todo 171 — `slug` is repaired; `/agent` is honestly frozen as a gap

**Judgment: faithful.** `v1_session` carries the stored `SessionInfo.slug` rather than synthesising it (`crates/oc-server/src/compat_v1.rs:1200-1225`). Its regression obtains the required `Session` keys independently from the running build's `/doc` and the committed oracle, first requires those sets to agree, then checks non-null presence on POST, GET-by-id, and every list element (`crates/oc-server/tests/compat_v1.rs:423-537`). Removing `slug` therefore fails on the actual published contract, not a hand-copied key list.

The `/agent` observation is not hidden. The witness proves all oracle-required fields are present, pins the extra and absent optional sets, and deliberately fails if the build begins publishing an `Agent` schema or drops a required field (`crates/oc-server/tests/compat_v1.rs:539-655`). Because the only committed Agent schema capture is older than the target and the port publishes no Agent schema of its own, the remaining optional-field difference is correctly recorded as `v1-agent-projection-unverified` in `known_gaps()` rather than invented as a divergence (`crates/oc-testkit/src/compat_report.rs:488-540,568-612`; `docs/compatibility-matrix.md:91-95`).

## Wave-12 blockers outside Todos 168–171 remain open

The four new todos did not claim to repair all prior blockers, and source inspection confirms three earlier omissions remain:

1. **Partial streamed text is still not durable on stream error.** `run_turn` inserts the assistant row, accumulates streamed text, then propagates `next?` immediately; every `checkpoint_assistant` call occurs after that propagation point (`crates/oc-engine/src/loop.rs:704-712,768-846`). The stalled-provider regression asserts only visible `TextDelta` events and an idle-timeout error; its helper owns an in-memory database but returns neither connection nor hydrated transcript, and the test explicitly expects no retry (`crates/oc-cli/src/cmd/turn_tests.rs:2293-2438`). Thus Todo 166's frozen phrase “the partial assistant text survives to the transcript” remains false.
2. **Typed provider retry still has no production turn-loop caller.** `ProviderRetryPolicy` and `retry_provider` remain isolated in `retry.rs` and tests, while the production turn still performs one `provider.stream(completion)` and returns the first `ProviderError` (`crates/oc-engine/src/loop.rs:768-785`; `crates/oc-engine/src/retry.rs:140-260`). The unit happy path for a transient 503 does not establish Todo 36's required production behavior (`crates/oc-engine/tests/retry.rs:162-203`).
3. **Plugin-critical v1 authentication remains unimplemented.** The measured `client.auth.set`, `client.provider.oauth.authorize`, and `client.provider.oauth.callback` routes are still `V1Backing::NotImplemented` (`crates/oc-server/src/compat_v1.rs:274-336`), excluded from `V1_BACKENDS`, and answer structured 501s (`:475-527,1303-1349`). The generated gap inventory says plainly that installed auth plugins “cannot authenticate through this surface” (`docs/compatibility-matrix.md:85-89`). This is honest accounting, but it does not satisfy the standing day-one plugin promise or criterion 4's statement that the measured v1 methods are served.

None of these is declared as an intentional divergence, which is correct. They remain in-scope omissions and independent approval blockers.

## Standing-constraint audit

- The frozen plan has exactly **171** checked numbered implementation rows; F1-F4 remain separate unchecked reviews. Check marks do not override the observable omissions above.
- The oracle pin remains the newest installed release named by the plan: `PINNED_RELEASE = "1.18.15"`, while the source/plugin compatibility identity remains separately documented as 1.18.13 (`crates/oc-testkit/src/oracle.rs:30-40,60-81`).
- The modern `/api` gate derives 58 upstream operations from the captured OpenAPI, invokes all 58, rejects any 501, freezes the exact ten `503 backend_unavailable` operations by name, and permits exactly the two declared C8 additions (`crates/oc-testkit/tests/compat_suite.rs:1852-2008,2268-2309`). This narrowed modern-API scope remains faithful.
- The pre-`/api` adapters do not violate the modern-only rule by their existence: the frozen plan expressly requires the measured resident-plugin SDK surface. The defect is that required auth/OAuth behavior is absent, not that narrowly justified adapters exist.
- Zero first-party unsafe remains structurally enforced by workspace `unsafe_code = "forbid"`, a scan over shipped first-party source, and a second test requiring every workspace member to inherit workspace lints (`Cargo.toml:14-18`; `crates/oc-cli/tests/release_surface.rs:465-501,552-597`). Commit `d3539b65` only strengthened the policy diagnostic; it did not weaken the rule.
- `execute` remains the declared jcode-style structured composition: bounded to ten declared/expanded calls, dependency levels execute in parallel, binding/fan-out are supported, and recursion is rejected (`crates/oc-tools/src/batch.rs:17-49,68-184`). Its live schema is tied to the `execute-parameter-contract` divergence.
- The built-in omo surface remains intentionally slim rather than missing: six retained named roles have data-backed negative boundaries, deny-by-default permissions and output contracts; model ids are excluded from built-ins, and model policy inherits the session model unless user preset/per-agent data overrides it (`crates/oc-agent/src/builtin.rs:1-52,180-316`; `crates/oc-agent/src/model_policy.rs:1-66,186-245`). `task` exposes specific agent or category, per-call model and effort, background execution, and `task_id` continuation (`crates/oc-tools/src/task.rs:104-164`).
- `docs/divergences.toml` contains **17** decisions and `DECLARED_COUNT` is 17 (`docs/divergences.toml:61-154`; `crates/oc-testkit/src/divergence.rs:40-65`). The allow-list has not absorbed Todo 170's omitted SDK-client projection, the durable transcript omission, production retry, or unimplemented v1 auth. Those remain gaps/defects, as required.

## Unverifiable or overclaimed assertions

- Todo 170's “twelve out of twelve” and “EVERY JavaScript boundary” assertions are disproved by the live `PluginInput.client.provider.list()` path described above. The structural guard covers `HookInvocation`, not generated SDK HTTP calls.
- Todo 166's evidence proves user-visible partial text before timeout but not transcript survival; the existing test cannot inspect the database after its helper returns.
- The transient-503 retry evidence proves the standalone retry helper, not the production `run_turn` integration required by Todo 36.
- The v1 inventory accurately reports auth/OAuth as unbacked; any broader statement that the installed auth plugins can authenticate through the compatibility host is therefore unverifiable and contradicted by the shipped matrix.

## Required closure

1. **Complete the JavaScript SDK boundary inventory, including `PluginInput.client`.** Treat generated SDK HTTP methods that carry models/providers as JavaScript ingress/egress, route their adapters through an explicit SDK-shape projection, and extend the structural guard beyond `HookInvocation`. At minimum, a real installed-plugin or equivalent real-SDK regression must consume `client.provider.list()` and assert the full released model/provider contract, including meaningful `release_date`, reasoning/temperature/tool capabilities, modalities, limits, and cost semantics. Removing any projection must fail a named test.
2. **Preserve partial assistant text durably before propagating stream errors.** Hydrate the database after a stalled real turn and require the partial text part to match the visible deltas; keep the slow-progress and interruption controls green.
3. **Wire finite typed provider retry into production `run_turn`.** A retryable transient 503 must emit `RetryRollback`, clear attempt-local accumulation, replay within a finite budget, and complete on a succeeding attempt; exhaustion must remain finite and actionable.
4. **Serve the installed plugins' v1 authentication surface or explicitly renegotiate the frozen promise.** `auth.set` and both provider OAuth operations must perform real measured work with their SDK envelopes. Structured 501s and honest gap prose are not substitutes for the day-one compatibility criterion.

## Validation gates

The workspace gate is **not a completed pass**, and no green status is claimed. Status check 1 ran `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 cargo test --workspace --offline -- --test-threads=2`; all reported tests passed, but the 600-second outer limit terminated the command while the workspace was still running. Status check 2 ran the required test, Clippy, and fmt commands as one `&&` chain with a 1,200-second limit. Tests again reported no assertion failure through most of the workspace, including all 27 `compat_v1` tests and all 16 `compat_suite` tests, but the host exhausted its process/thread allowance while listing `oc-tools` tests: `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`, followed by a libtest `SendError`. Because the test stage failed, chained Clippy and fmt did not run. Per the review's two-status-check ceiling, no third attempt was made.

`lsp_diagnostics` could not inspect this Markdown report because the tool is rooted at `/config/workspace/ProdDir/AI/opencode-rust` and rejects the sibling audit worktree `/config/workspace/ProdDir/AI/oc-wt/tF4` before invoking a language server. The report is the sole intended modification. This host/tool limitation is recorded rather than represented as a clean diagnostic or successful gate.

## Final disposition

**REJECT.** Todos 168, 169, and 171 faithfully close their exact frozen findings, and Todo 170 repairs all twelve enumerated callback mutations. Approval is still impossible because Todo 170's universal-boundary claim omits a live eighth layer used by installed OMO and serves observably lossy model metadata. Independently, durable partial-text preservation, production provider retry, and plugin-critical v1 auth/OAuth remain open from wave 12. The modern `/api` narrowing, newest-release oracle, zero-unsafe enforcement, seventeen declared divergences, jcode-style `execute`, and slim built-in omo remain faithful and do not offset those omissions. This audit modifies no product source, test, plan, documentation, or prior evidence file.
