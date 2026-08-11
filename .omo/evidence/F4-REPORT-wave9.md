# F4 Scope Fidelity Review — Final Verification Wave 9

## Verdict: REJECT

Audited HEAD `c251665ac3b6fda21c276fe6814cf2ab17006a27` (`c251665a`). The implementation ledger contains exactly 153 checked, unique, contiguous numeric todos (`1..153`), with no duplicate, missing, or unchecked implementation row. Todos 150 and 151 close their stated permission-dialog and ordinary-hook truncation defects. Todo 152 fixes provider-identity preservation and production selection, but its Azure and GitHub Copilot acceptance tests prove only endpoint selection: they send Chat Completions bytes to `/responses` and replay Chat Completions responses there. Todo 153 honestly discloses that success criterion 2 is unmet, but disclosure does not satisfy the criterion.

F4 therefore rejects the artifact for two independent scope-fidelity failures:

1. the required pure-mode merge of the user's actual config **and its adjacent agent/command trees** is still not byte-identical; and
2. the checked todo 152 claim that Azure and Copilot `/responses` profiles “select and dispatch” is backed by the wrong request and response protocol.

The mandatory workspace verification could not complete on this host. Both permitted attempts were interrupted by the same OS resource failure, `EAGAIN` / `WouldBlock` while libtest enumerated tests. No product assertion failed before either interruption, but the full workspace test result is not green, and the `&&` chain consequently did not reach Clippy or rustfmt.

## Blocking findings

### 1. Success criterion 2 is explicitly and reproducibly unmet

**Requested scope.** Success criterion 2 requires the Rust binary to read `/config/.config/opencode/opencode.json` **and its full skill and agent trees**, then produce a normalized pure-mode `debug config` result byte-identical to the released binary (`.omo/plans/opencode-rust.md:1404`). Pure mode excludes external plugin execution; it does not exclude adjacent Markdown config files.

**Delivered scope.** Todo 153 refreshed `crates/oc-config/tests/fixtures/user-config.json` and added a byte-for-byte drift guard against the live `opencode.json` (`crates/oc-config/tests/differential.rs:313-353`). That correctly closes the stale-file defect. The main matrix, however, still copies only that JSON file into an isolated `ConfigFixture` (`crates/oc-config/tests/differential.rs:299-309`), so its green `real-user-global-config-capture` case does not contain the real host's adjacent `agent/` and `command/` directories.

The plan now records the resulting gap accurately: criterion 2 “currently does not hold,” because upstream discovers nine files under `/config/.config/opencode/agent/powerapps/`, two under `/config/.config/opencode/command/`, and three `plugin_origins` entries, while Rust emits empty `agent` and `command` objects and omits `plugin_origins` (`.omo/plans/opencode-rust.md:1405`). Independent same-cwd probes during this review reproduced that exact result for both `OPENCODE_PURE=1` and explicit `--pure`:

- released `opencode 1.18.15`: 266,233 output bytes, 9 agent entries, 2 command entries, 3 plugin-origin entries;
- `opencode-rust 1.18.13`: 25,581 output bytes, 0 agent entries, 0 command entries, no `plugin_origins` field;
- after removing only upstream's diagnostic `mode` field and recursively canonicalizing JSON keys: unequal.

The targeted test `cargo test --offline -p oc-config --test differential -- --nocapture` passed all five tests, but that is evidence of the test-scope gap, not evidence that the live-tree criterion holds: the matrix's isolated capture is identical while the required same-directory process outputs are not.

Todo 153 was allowed to establish the truth rather than hide it, and its plan amendment is commendably explicit. The project-level success criterion nevertheless remains a required, executable condition. A checked diagnostic todo cannot turn an unmet success criterion into completion.

**Required to satisfy:**

1. Make production `debug config` discover the same pure-mode adjacent Markdown agent and command trees as the released binary when run in `/config/.config/opencode`, preserving their merge precedence and resolved values.
2. Resolve `plugin_origins` honestly: emit equivalent metadata if it belongs to the `debug config` contract, or obtain an owner-approved narrowing that specifically excludes it. Do not normalize the field away without such a decision.
3. Add a same-directory live-tree differential that invokes both binaries with pure mode against the real config directory and fails on any structural or value difference. Retain the existing fixture-to-live byte drift check for reproducibility, but do not use an isolated one-file fixture as the proof of the adjacent-tree criterion.
4. Change criterion 2's status to satisfied only after that live-tree differential is green; do not add the current pure-mode omission to the intentional-divergence allow-list.

### 2. Todo 152 routes Chat Completions bytes through `/responses`

**Requested scope.** Todo 94 commits distinct OpenAI Chat Completions and Responses behavior and explicitly calls out Azure model-selection and GitHub Copilot's model-dependent endpoint rules (`.omo/plans/opencode-rust.md:37`). Todo 152 requires production-path tests that prove identity-preserving **selection and dispatch** for Azure and Copilot (`.omo/plans/opencode-rust.md:1379-1381`). A `/responses` dispatch must therefore use the Responses request and event vocabulary, not only the Responses URL.

**Delivered scope.** The identity half of todo 152 is fixed: resolved compatible models preserve `Spec::provider`, select the shared compatible factory separately, and reject unknown transports. The fourth wire layer remains Chat Completions-only:

1. `oc-provider-compatible::request` documents itself as “Assembling a chat-completions request body,” always writes `messages`, and translates tool results as `role: "tool"` / `tool_call_id` (`crates/oc-provider-compatible/src/request.rs:1-22,109-121,170-180,288-301`). It has no Responses `input` construction.
2. `CompatibleProvider::body_for` invokes that one `RequestBody::build` regardless of the selected `ApiSurface`; only `endpoint` branches on the surface (`crates/oc-provider-compatible/src/provider.rs:147-200`). Thus Azure and Copilot `gpt-5` send a Chat Completions body to a Responses URL.
3. `ChunkTranslator` is explicitly a Chat Completions translator and reads `choices[].delta`, `finish_reason`, `prompt_tokens`, and `completion_tokens` (`crates/oc-provider-compatible/src/stream.rs:1-24,89-136`). It contains no decoder for Responses events such as output-text deltas, output items, or response completion.
4. The production Azure test mounts the Chat Completions cassette `openai-compatible-chat/deepseek-streams-text` on `/v1/responses` (`crates/oc-cli/src/cmd/turn_tests.rs:517-535`). The Copilot `gpt-5` case does the same (`crates/oc-cli/src/cmd/turn_tests.rs:539-559`). The mock routes by path and serves those recorded Chat Completions bytes, so the tests pass precisely because both fixture and decoder share the wrong protocol.

The dedicated OpenAI implementation demonstrates the missing distinction. It writes `messages` for Chat Completions but `input` and `max_output_tokens` for Responses (`crates/oc-provider-openai/src/request.rs:56-136`), and selects a surface-aware decoder (`crates/oc-provider-openai/src/provider.rs:341-382`). The compatible provider has no equivalent branch.

This is not a cosmetic provider-specific quirk. A real Azure or Copilot Responses endpoint will reject the Chat Completions request shape or return Responses events that `ChunkTranslator` cannot decode. The todo 152 acceptance wording says “select and dispatch”; URL-only selection against a protocol-mismatched cassette does not satisfy it. It also makes `provider-coverage-by-wire-family` overstate implemented behavior: Azure and the Responses arm of Copilot are claimed as implemented profiles while their selected surface is not implemented end to end.

**Required to satisfy:**

1. Add surface-aware compatible request construction: Chat surfaces retain `messages`; Responses surfaces use the actual Responses `input`, tool, token-limit, reasoning, and stream shape required by the pinned counterpart.
2. Add a Responses event decoder, or reuse a protocol-appropriate shared decoder without collapsing provider identity or provider-specific headers/URL rules.
3. Replace the Azure and Copilot `gpt-5` tests with recorded Responses traffic from the real relevant counterpart. Assert captured request-body discriminants (`input` present, `messages` absent) as well as the endpoint and decoded event sequence. Keep the Copilot chat-model case on a genuine Chat Completions cassette.
4. Ensure a protocol mismatch fails: mounting a Chat Completions cassette on `/responses` must no longer count as proof of Responses dispatch.
5. Until the Responses path is implemented, classify the Azure/Copilot Responses profiles as frozen known gaps rather than intentional divergences or completed compatible profiles.

## Latest remediation audit

### Todo 150 — closed

The production permission producer places real tool arguments in `PermissionAsk.metadata`, and `PermissionBridge` forwards them to the view rather than rendering `Value::Null`. Focused dialog scopes expose the production choice keys. Tests enter through production dispatch, assert the command/URL is rendered, and prove every offered action resolves the waiter. The initially missed producer-side metadata seam is disclosed rather than hidden.

### Todo 151 — closed

The ordinary JavaScript hook boundary checks every encoded argument for `$truncated` before `apply_hook_output`; rejection occurs before any argument mutation, preserving all originals atomically. Coverage uses the real deep built-in `tool.definition` schema and proves provider dispatch does not receive truncated state. This closes the class of write-back paths todo 147 had fixed only for auth loading.

### Todo 152 — partially closed, still blocking

Factory selection and provider identity are now separate, all fifteen named compatible identities reach their profile, real Groq and Mistral transport spellings select the compatible factory, and an unknown transport is refused. OpenRouter's router identity survives. Azure/Copilot endpoint selection also survives, but the `/responses` request/decoder layer is not implemented; blocker 2 above prevents this checked todo from satisfying its dispatch claim.

### Todo 153 — honest diagnosis, criterion still blocking

The stale committed capture was refreshed and is now drift-checked byte for byte against the live JSON. The invalid `agent list --format json` historical acceptance criterion was explicitly amended to the real plain `agent list` surface. Todo 153 also recorded the live-tree failure accurately. It therefore closes its two observer/contract-correction defects, but its finding leaves project success criterion 2 unmet as blocker 1.

No `.omo/evidence/task-153-opencode-rust.txt` exists at the audited HEAD. The plan does not use that absence to claim parity, so this is recorded as an evidence-packaging omission rather than a third product blocker; the executable drift test and plan disclosure are available, and this review independently reproduced the live result.

## Full ledger and divergence assessment

The implementation ledger has 153 checked rows, 153 unique numeric identifiers, and no gap in `1..153`. Todos 150–153 are focused remediation or truth-correction for frozen requirements, not unrelated product scope. The four final F1–F4 rows remain unchecked review gates and are not included in the numeric implementation ledger.

The declared divergence registry still contains exactly 17 entries and its generated page agrees with the TOML source. Assessment at this HEAD:

1. `session-list-default-sort` — accepted.
2. `tool-output-filename-carries-session` — accepted.
3. `no-eager-directory-creation` — accepted.
4. `split-version-identity` — accepted.
5. `execute-parameter-contract` — accepted; live schema remains machine-checked.
6. `c8-maintenance-endpoints` — accepted explicit added scope.
7. `provider-coverage-by-wire-family` — **rejected as currently stated** for blocker 2; valid profile selection does not make the selected Responses wire protocol exist.
8. `cross-session-resident-memory` — accepted; strict parity can disable its three surfaces.
9. `session-subpath-is-applied` — accepted.
10. `context-md-excluded` — accepted.
11. `malformed-auth-json-is-an-error` — accepted deliberate data-preservation behavior.
12. `failed-format-restores-pre-format-bytes` — accepted.
13. `non-pure-plugin-generated-trees` — accepted only for the declared non-pure plugin synthesis. It does **not** cover blocker 1's pure-mode adjacent Markdown trees.
14. `plain-cli-presentation` — accepted; normalization remains bounded by negative controls.
15. `diagnostics-name-their-cause` — accepted.
16. `session-list-output-shape` — accepted measured content difference.
17. `non-vcs-plan-glob-is-absolute` — accepted.

No eighteenth intentional divergence is required for the current pure-mode config gap: the plan correctly labels it unmet rather than laundering it into the allow-list.

## Standing requested properties

- **No first-party `unsafe`: satisfied by source and release-surface gates.**
- **Rust plugin without JavaScript: satisfied.**
- **Slim agent design: satisfied.**
- **Goal behavior: satisfied.**
- **Cross-session memory and structured `execute`: satisfied under their declared contracts.**
- **Deprecated config handling: satisfied apart from documented deliberate exceptions.**
- **Implement rather than declare: not satisfied.** Pure-mode adjacent config trees remain omitted, and compatible `/responses` is URL selection without Responses wire implementation.
- **All success criteria complete: not satisfied.** Criterion 2 explicitly says it does not hold, and criterion 18 requires all F1–F4 reviews to approve.

## Verification

- Targeted config differential: `cargo test --offline -p oc-config --test differential -- --nocapture` — **PASS, 5 passed**, while also demonstrating that the green real-user case is the isolated one-file capture rather than the required same-directory live tree.
- Manual same-cwd pure-mode comparison — **FAIL as expected from the plan disclosure**: 266,233 upstream bytes versus 25,581 Rust bytes; agent entries 9 versus 0; command entries 2 versus 0; `plugin_origins` present upstream and absent in Rust.
- Mandatory workspace gate, attempt 1: `cargo test --workspace --offline && cargo clippy --workspace --all-targets --offline && cargo fmt --all --check` — **INCOMPLETE**. Test execution reached `oc-tools`, then libtest failed to enumerate tests with `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`; secondary `SendError` panics followed from the aborted harness. No product assertion failed. Clippy and fmt were not reached.
- Mandatory workspace gate, permitted retry: same command — **INCOMPLETE**. Test execution progressed farther and then failed while enumerating `oc-config` tests with the same `EAGAIN/WouldBlock`; secondary harness `SendError` panics followed. No product assertion failed. Clippy and fmt were not reached.
- Per the two-status-check cap, no third attempt was made. Therefore this review does **not** claim a full workspace-test pass, zero Clippy warnings, or clean rustfmt at audited HEAD.
- The prohibited approximately 100-minute memory gate and two-hour soak were not rerun. Windows-only containment remains not executed on this Linux host, consistently with the disclosed narrowing.

This review changed no product source, test, plan, product documentation, commit, branch, or remote state. `.omo/evidence/F4-REPORT-wave9.md` is the only intended worktree modification.

F4 VERDICT: REJECT
