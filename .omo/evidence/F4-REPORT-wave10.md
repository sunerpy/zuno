# F4 Scope Fidelity Review — Final Verification Wave 10

## Verdict: REJECT

Audited HEAD `2e742986206d5a8707508b4008d2b56d651f0864` (`2e742986`). The implementation ledger contains exactly 161 checked, unique, contiguous numeric todos (`1..161`). Todos 154–161 close the defects they name, including genuine compatible Responses request/stream handling and pure-mode `debug config` parity. The mandatory offline workspace gate also passed in full on the permitted retry.

One production-path provider defect remains blocking: GitHub Copilot's advertised per-model endpoint is discarded before provider selection. The released implementation gives `model.api.endpoint` precedence over its model-id heuristic. This port models only `id`, `npm`, and `url`, so a real non-GPT Copilot model advertised as Responses is routed to Chat Completions. That contradicts todo 94's Copilot endpoint rule, todo 152's production selection claim, and the `provider-coverage-by-wire-family` statement that implemented families are not gaps.

## Blocking finding

### 1. Copilot's advertised model endpoint never reaches production selection

**Required behavior.** The pinned oracle checks a model's declared endpoint before applying its fallback model-id rule: `model.api.endpoint === "responses"` selects Responses, and `model.api.endpoint === "chat"` selects Chat (`packages/opencode/src/provider/provider.ts:225-236`). This precedence is observable and necessary for model ids that do not encode their protocol. The pinned upstream regression witness records `mai-code-1-flash-picker` with `supported_endpoints: ["/responses"]` and asserts its resolved `model.api.endpoint` is `responses` (`packages/opencode/test/plugin/github-copilot-models.test.ts:244-280`).

**Delivered behavior.** The compatible provider contains a correct declared-endpoint reader, but production never supplies its input:

1. `ModelApi` contains only `id`, `npm`, and `url`; it has no endpoint field (`crates/oc-llm/src/catalog/resolved.rs:78-87`).
2. Catalog resolution constructs exactly those three fields and therefore cannot preserve an advertised endpoint (`crates/oc-llm/src/catalog/merge.rs:165-188`).
3. The compatible selector expects declarations in the synthetic `modelEndpoints` option (`crates/oc-provider-compatible/src/surface.rs:63-68,242-255`), but the only non-test references to that key are its declaration and reader. No catalog or production composition path populates it.
4. `model_spec` forwards provider/model options and the npm transport, but never derives `modelEndpoints` from resolved model metadata (`crates/oc-cli/src/cmd/turn.rs:1405-1469`).
5. With no declaration, `copilot_surface` falls back to the model-id heuristic (`crates/oc-provider-compatible/src/surface.rs:193-213`). `mai-code-1-flash-picker` does not match `^gpt-(\d+)`, so this port selects Chat despite Copilot advertising Responses.
6. The production Copilot test covers only heuristic-friendly ids (`gpt-5` and `gpt-5-mini`) and manually exercises neither advertised-endpoint precedence nor a non-GPT Responses model (`crates/oc-cli/src/cmd/turn_tests.rs:587-625`). The lower-level declared-endpoint unit test injects the synthetic option directly, so it proves the reader but not the missing production transport.

Todo 156 correctly implements the fifth-layer wire behavior once a Responses surface has been selected: Responses requests carry `input`, recorded typed Responses events decode, and Chat retains `messages`. That does not repair the earlier metadata loss. On a real Copilot catalog response, this defect chooses `/chat/completions` before the correct body and decoder can be used.

**Required to satisfy:**

1. Preserve Copilot's resolved per-model endpoint metadata through catalog/plugin model resolution. An explicit typed field on `ModelApi`, or an equivalently lossless production mapping into `MODEL_ENDPOINTS_OPTION`, is acceptable; a user-authored option required to reconstruct metadata the provider already advertised is not.
2. Carry that metadata through `model_spec` and retain oracle precedence: explicit `responses`/`chat` first, then the existing model-id fallback.
3. Add production-path tests beginning with resolved Copilot model metadata, not a hand-built compatible `Spec`. At minimum, prove a non-GPT model such as `mai-code-1-flash-picker` advertised as Responses dispatches to `/responses`, sends `input` without `messages`, and decodes a recorded Responses stream.
4. Add the inverse precedence case: an explicit Chat endpoint on an id the heuristic would otherwise send to Responses must dispatch Chat. Removing endpoint propagation must fail these tests by name.
5. Until this path is implemented, do not describe Copilot's compatible family as fully implemented; record it as a frozen gap rather than broadening an intentional divergence.

## Latest remediation audit

- **Todo 154 — closed.** Compatible streaming reads are bounded by an inter-chunk idle timeout rather than a total-generation deadline. The error names provider/model and preserves already emitted partial text; progressing streams remain alive.
- **Todo 155 — closed.** Database open refuses a journal id above the compiled maximum and names both observed and maximum ids, while unknown ids below the ceiling remain tolerated. Production `db` tests cover future, compatible, and below-ceiling journals.
- **Todo 156 — closed.** Compatible request construction and translation are surface-aware. Production Azure and Copilot Responses tests use the real `openai-responses/gpt-5-5-streams-text` recording and assert `input`; Chat tests assert `messages`. The remaining blocker is selection metadata before this layer, not Responses wire handling.
- **Todo 157 — closed.** Production-shaped edit permission tests render both path and diff in collapsed and fullscreen modes, and the advertised selection keys are functional.
- **Todo 158 — closed.** Canonical `parts` mutations from `experimental.chat.messages.transform` are applied to the continuation and asserted at the outgoing model request.
- **Todo 159 — closed.** Production `chat.message` payloads carry the live `id`, `sessionID`, `agent`, and `model` values.
- **Todo 160 — closed.** A real TUI construction path selects `PluginKind::Tui`; the PTY acceptance test reaches it from production configuration.
- **Todo 161 — closed.** `debug config` merges runtime-discovered file-backed agents and commands and carries plugin origins. The pure-mode criterion-2 differential passes against released 1.18.15 with equal normalized documents (9 agents, 2 commands, 3 plugin origins).

## Full ledger and divergence assessment

The numeric implementation ledger has 161 checked rows, 161 unique identifiers, and no gap in `1..161`. Todos 154–161 are scoped remediations of previously demonstrated failures, not unrelated additions. The four F1–F4 rows are review gates and are not part of that numeric implementation ledger.

The divergence registry contains exactly 17 entries, and the generated divergence index agrees with its TOML source. Assessment at this HEAD:

1. `session-list-default-sort` — accepted.
2. `tool-output-filename-carries-session` — accepted.
3. `no-eager-directory-creation` — accepted.
4. `split-version-identity` — accepted.
5. `execute-parameter-contract` — accepted; the live schema remains machine-checked and follows the requested jcode-style composition contract.
6. `c8-maintenance-endpoints` — accepted explicit added scope.
7. `provider-coverage-by-wire-family` — the declared policy is accepted, but its claim that implemented families are not gaps is not currently true for Copilot's advertised endpoint path; blocking finding 1 must be closed or honestly reflected in the known-gap inventory.
8. `cross-session-resident-memory` — accepted; `memory: false` removes the three added surfaces for strict parity.
9. `session-subpath-is-applied` — accepted.
10. `context-md-excluded` — accepted modern-form-only behavior.
11. `malformed-auth-json-is-an-error` — accepted deliberate data-preservation behavior.
12. `failed-format-restores-pre-format-bytes` — accepted.
13. `non-pure-plugin-generated-trees` — accepted only for non-pure third-party plugin synthesis; pure-mode config parity is now independently green.
14. `plain-cli-presentation` — accepted; normalization is bounded by negative controls.
15. `diagnostics-name-their-cause` — accepted.
16. `session-list-output-shape` — accepted measured content difference.
17. `non-vcs-plan-glob-is-absolute` — accepted.

No eighteenth intentional divergence is warranted for the Copilot defect. It is a missing production metadata path, not a design decision.

## Standing requested properties

- **Newest installed release oracle:** satisfied; differential coverage is pinned to installed release 1.18.15 as required.
- **58 upstream `/api` operations:** satisfied under the narrowed criterion: 48 have local compared backends and the frozen 10 explicitly return operation-specific `503 backend_unavailable`; the two C8 maintenance operations remain declared additions.
- **Modern-form-only configuration:** satisfied; deprecated forms are rejected with actionable replacements rather than silently normalized.
- **`execute` modeled on jcode:** satisfied; its structured composition schema is declared and live-checked rather than misrepresented as upstream's interpreter.
- **Slimmed omo-style agents:** satisfied; the slim roster, negative delegation boundaries, deny-by-default permissions, inheritance, and category/reasoning controls remain represented and tested.
- **No first-party `unsafe`:** satisfied by workspace lints and the release-surface scanner.
- **Implement rather than declare:** not fully satisfied because Copilot's advertised endpoint metadata is modeled by an isolated reader but not wired from production model resolution.
- **All success criteria complete:** not satisfied; this F4 review rejects, so criterion 18 cannot pass.

## Verification

- Mandatory workspace gate, attempt 1: `cargo test --workspace --offline && cargo clippy --workspace --all-targets --offline && cargo fmt --all --check` — **host-resource interruption**, not a product assertion failure. Libtest reached `oc-config` and then failed to enumerate tests with `EAGAIN` / `WouldBlock` (`Resource temporarily unavailable`); secondary `SendError` panics followed. The chain therefore did not reach Clippy or rustfmt.
- Mandatory workspace gate, permitted retry: the same command — **PASS**. All workspace test and doc-test result sections completed with zero failures (3,421 passed in aggregate; two documented tests ignored), Clippy completed all targets with no warnings, and rustfmt check completed cleanly.
- The approximately 100-minute memory gate and two-hour soak were not rerun in this review. Windows-only containment was not executed on this Linux host; those limitations remain the explicitly disclosed scope of criterion 15 rather than being reported as newly verified here.

This review changed no product source, tests, plan, product documentation, commit, branch, or remote state. `.omo/evidence/F4-REPORT-wave10.md` is the only intended worktree modification.

F4 VERDICT: REJECT
