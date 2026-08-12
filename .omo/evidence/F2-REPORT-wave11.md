# F2 Code Quality Review — Wave 11

- **Audited HEAD:** `b20ecbc9`
- **Role:** F2, code-quality and test-honesty reviewer
- **Verdict:** **REJECT**

## Planned probes

1. Reconstruct the wave-9 baseline from `F2-REPORT-wave9.md`, the frozen plan, the seam catalogue, and the producer/fix-criterion/seam-closure rules in `WORKTREE.md` waves 56–58.
2. Independently verify F2-B1 through F2-B4 (todos 161, 158, 159, and 160), including real producer/consumer paths and whether tests compare genuine runtime behavior without normalization or placeholders.
3. Verify todos 162–164 and revisit the green-suite-permitted defects in todos 154–157, focusing on producer defaults, production callers, sampling blind spots, and derived rather than hand-transcribed coverage.
4. Mutation-test load-bearing assertions by changing observable producers/defaults/inputs, running the narrowest relevant tests, recording failures immediately, and restoring every mutation.
5. Hunt for an additional seam: deletion-insensitive assertions, fixture/config self-assertions, ignored or silently skipped tests, production paths with zero non-test callers, unreachable errors, and passes guaranteed by construction.
6. Run `cargo test --workspace --offline`, `cargo clippy --workspace --all-targets --offline`, and `cargo fmt --all --check`; then confirm only this report is modified.
7. Classify blockers and non-blocking findings explicitly, identify plan-ledger candidates, state anything not verified, and finalize APPROVE or REJECT.

## Incremental evidence

_Evidence will be appended as each probe or mutation completes._

### Baseline reconstructed before source investigation

- The last delivered F2 review (`F2-REPORT-wave9.md`, audited `c251665a…`) rejected on four concrete product defects: criterion-2 runtime trees absent from `debug config`; transformed canonical `parts` discarded before provider dispatch; `chat.message.output.message` reduced below the advertised identity-bearing shape; and `PluginKind::Tui` constructible only by tests. It also disclosed an incomplete workspace gate after two host `EAGAIN` failures.
- The frozen acceptance criteria for todos 154–164 require production-entry or production-surface witnesses, not codec/unit-only checks. In particular: todo 158 must assert the outgoing model request; todo 159 must preserve live turn values rather than synthesize fields; todo 160 must distinguish a real TUI production selection from Server; todo 161 must compare full pure-mode output; todo 162 must begin with resolved, heuristic-hostile metadata; todo 163 must load every global/project × singular/plural location and expose failures; todo 164 must derive coverage from the route registry and record an unimplemented surface as a known gap.
- The defect catalogue records the claimed repaired shapes but is not treated as proof. It also identifies the historical test traps to reproduce independently: fixture-friendly inputs, consumer-only mutations, exact-route code with no production caller, and assertions over selected endpoints while request bytes remained wrong.
- `WORKTREE.md` waves 56–58 establish three review rules applied here: mutate the producer/default rather than only the consumer; re-check the original criterion rather than the repair diff; and determine whether a seam was closed or merely moved upstream. Wave 58 additionally confirms why this report is being written incrementally.
- The plan currently contains both the resolved criterion-2 paragraph (exact normalized equality: 252,891 bytes, 9 agents, 2 commands, 3 plugin origins) and the older todo-153 historical `UNMET` paragraph immediately after it. I will judge the executable differential and actual normalization, not that contradictory historical prose.

### Tooling boundary

- CodeGraph was attempted first for source tracing and returned `No indexed project found` for this sibling worktree, the expected limitation disclosed by the task. Current source is therefore inspected directly. `git rev-parse HEAD` independently returned `b20ecbc9497cb820929c0cdb9f0507a0b425c9c9`, matching the audited commit.

### F2-B1 / todo 161 — source disposition: repaired, comparison is genuine

- Production `debug config` calls the same `oc_catalog::agent::load_map` and `oc_catalog::command::load_map` discovery functions used to resolve Markdown definitions. Those loaders derive `Layout::config_directories`, recursively scan `{agent,agents}` or `{command,commands}`, merge file-backed config first, Markdown next, and `OPENCODE_CONFIG_CONTENT` last. This is runtime discovery output, not a test-only fixture map.
- `debug.rs::config` inserts the resulting full `agents.agents` and `commands` maps into the serialized live `Context` config. It independently derives source-ordered plugin origins from real config layers and scanned plugin directories, writes both `plugin` and `plugin_origins`, and identity-deduplicates them.
- `criterion_2_pure_debug_config_matches_the_released_binary` starts the released executable and the Rust executable from the same real workspace and environment, captures complete stdout via files, requires both exits to succeed, parses both full documents, removes **only** released 1.18.15's empty deprecated `mode` object (and asserts it was exactly `{}`), then performs one exact `serde_json::Value` equality. It does not delete, sort, summarize, count, or normalize `agent`, `command`, `plugin`, or `plugin_origins`; any hidden field/value difference remains observable.
- A separate isolated production-binary witness creates nested Markdown agent/command files and checks their prompt/template in `debug config`, preventing the full live comparison from being the only evidence. Mutation sensitivity is still to be verified below, but the structural comparison itself closes the original criterion rather than merely guarding its input.

### F2-B2 / todo 158 — source disposition: repaired, canonical parts reach the wire

- The enabled-hook branch in `oc-engine::loop` converts history to `HookMessageWithParts`, invokes `transform_messages`, and now rebuilds `stable_history` with `append_transformed_message_owned`. That function projects `message.parts` by role; it does not read the stale `info.content` mirror that made the old test self-confirming.
- The real-binary lifecycle plugin mutates only `user.parts[0].text` in `experimental.chat.messages.transform`. The production test then parses the captured provider HTTP request and requires the combined `config:raw arguments:command:chat:messages` text. Removing the canonical-part projection therefore loses the final sentinel at the model boundary; observing the hook's own argument is no longer the asserted outcome.

### F2-B3 / todo 159 — source disposition: repaired, fields are live turn values

- `prepare_user_message_with_hooks` creates a real `MessageRecord` before dispatch, using the generated/live message id, selected session, agent, provider/model pair, and current turn time. `ChatMessageOutput.message` carries that record through the plugin codec, whose JSON-RPC adapter serializes it with `MessageRecord::to_json`; there is no reduced parallel payload.
- The production lifecycle test captures both hook input and `output.message` from a JavaScript plugin and requires `id == input.messageID`, `sessionID == input.sessionID`, `agent == input.agent`, and `model == input.model`. The same test must complete a real provider request, so these are values supplied by the running turn rather than fixture placeholders accepted by a codec-only test.

### F2-B4 / todo 160 — source disposition: repaired within the frozen criterion

- Production startup has two distinct selectors: ordinary turns call `PluginRuntimeTarget::server(...)`, while `TurnHost::load_tui_plugins` calls `PluginRuntimeTarget::tui("tui")`; both selectors flow into configured and auto-discovered `JsPluginSpec` construction. Thus `JsPluginKind::Tui` now has a non-test caller.
- The PTY-backed production test configures one module exposing `server` and one exposing `tui`, starts the real interactive command, and requires separate `server` and `tui` marker files. This proves selection differs at module-factory load time. It does not claim that this Rust TUI implements upstream's full renderer/slot/view API; that broader API is outside todo 160's narrowly frozen reachability criterion.

### Todo 154 — BLOCKER: implementation is bounded, but the required turn-level regression is absent

- The production transport does install a 120-second per-chunk idle timeout, caps overrides at 180 seconds, and refreshes the timeout after every received chunk. The implementation is an idle bound rather than a total generation deadline, and the slow-progressing socket test correctly outlives one idle window.
- However, the only stalled-socket regression calls `ReqwestTransport::send` directly, reads the raw `PARTIAL_` body chunk, and then observes a `ProviderError` stream item. No test drives a provider/engine/CLI turn, no SSE frame is decoded, and no user-visible or durable partial assistant text is asserted. A repository-wide search found `PARTIAL_` only in this transport unit test. The acceptance criterion explicitly requires the **turn** to fail within a bound while preserving partial text; the current test proves chunk retention before translation, not the product behavior that previously hung for 200 seconds. This is a test-honesty/acceptance blocker even though the low-level timeout mechanism itself is credible.

### Todo 155 — source disposition: repaired through the production command

- `migration::apply_only` reads the real journal and calls `refuse_future_migrations` before executing any migration SQL. It compares against the maximum production `MIGRATION_IDS` value, rejects only lexically newer ids, and returns the typed error carrying both ceiling and greatest observed id.
- `db_migration_ceiling.rs` initializes an actual file database through the released CLI entry point, inserts journal rows, and starts the real `db --format json` command again. It proves a future id prevents query output and names both ids, while an unknown id immediately below the ceiling and a compatible journal still serve the query.

### Todo 156 — source disposition: repaired at both request and response wire boundaries

- `CompatibleProvider::body_for` resolves quirks for the request surface and `RequestBody::build` chooses disjoint Chat (`messages`, `max_tokens`) and Responses (`input`, `max_output_tokens`) builders. `CompatibleProvider::stream` passes the same resolved surface to `SurfaceTranslator`, which selects the typed `response.*` decoder rather than `choices[].delta` parsing.
- Production-selection tests run through catalog/config resolution, `model_spec`, the provider registry, engine prelude, a loopback HTTP server, and recorded cassettes. They require `input` with no `messages` for Azure/Copilot Responses routes, `messages` with no `input` for Chat routes, and successful text decoded from true `openai-responses/*` recordings. The prior chat-cassette-on-Responses-path seam is closed.

### Todo 157 — source disposition: repaired, with producer-to-render coverage

- The CLI test dispatches an `edit` call through `ToolRegistryDispatcher` and the real permission broker, renders the resulting TUI bridge in collapsed and fullscreen modes, and requires the file path plus both sides of the diff before answering the request. This covers the arguments producer, broker metadata, view description, diff construction, rendering, and reply path rather than injecting a ready-made view only.
- View tests separately cover every currently handled `describe` branch (including unknown and external-directory forms), require a non-empty rendered subject, exercise fullscreen diff rendering, and assert that the footer advertises the actual Up/Down bindings. The branch table is hand-enumerated rather than compiler-derived, but it spans the implementation's complete current match and is paired with the production dispatch witness.

### Todo 162 — source disposition: repaired through resolved/plugin model metadata

- `ModelApi.endpoint` is a typed, serde-preserved `Option<ModelEndpoint>`. `model_spec` converts it into the per-wire-id `MODEL_ENDPOINTS_OPTION`, which `declared_endpoint` reads before Copilot's model-id heuristic. The endpoint follows `api.id`, so aliases do not disconnect the declaration from the outgoing model id.
- The two adversarial tests begin with a resolved Copilot catalog, serialize/deserialize the same `ResolvedModel` shape used by provider model hooks, replace the provider's models through the catalog API, and only then call production `model_spec`. They prove advertised `responses` wins for `mai-code-1-flash-picker` and advertised `chat` wins for heuristic-Responses `gpt-5`, asserting both path and body discriminant against recorded protocol-specific bytes. They do not hand-construct a compatible `Spec`.

### Todo 163 — source disposition: repaired through real binary loading and visible diagnostics

- `PluginRuntime::load`, used by real turn and TUI startup, extends configured modules with `auto_discovered_plugins` for both project/worktree and every resolved global config directory, and forwards the requested Server/TUI kind. Discovery and module-load diagnostics are emitted with plugin, kind, message, and surface.
- The production test writes load-recording modules to project `.opencode/{plugin,plugins}` and global `opencode/{plugin,plugins}`, starts the real `run` command, and requires exactly all four filenames in the load log. It also introduces a broken advertised directory and requires the DEBUG diagnostic to identify that path, closing the old silent-failure seam.

### Todo 164 — source disposition: repaired and honestly frozen as a gap

- The live `V1_SURFACE` table is the single source for router construction, backing status, per-route `/api` alternatives, and `v1_coverage`. Tests drive every registered route and require its actual 501/non-501 answer to match the declared backing; they also reject plan-todo citations or future-work language and require each 501 to name its recorded live alternative or explicitly state that none exists.
- `v1_surface_gap` receives derived live counts rather than restating them and renders the committed known-gap entry. The current matrix honestly reports 19 of 20 measured routes unbacked, ten redirected to served `/api` alternatives, and nine stranded. This is classified as a frozen gap rather than laundered into `divergences.toml`.

### Mutation 1 — todo 158 canonical-parts producer: killed as intended

- Temporarily replaced `append_transformed_message_owned(&mut messages, message)` in the production hook path with the old `messages.push(message.info)` behavior, then ran `cargo test -p oc-cli --test tool_turn ordinary_plugin_lifecycle_hooks_run_through_the_real_binary --offline`.
- The named test failed at its captured-provider assertion: the outgoing user text was `config:raw arguments:command:chat`, missing the canonical-parts-only `:messages` mutation. This is the exact historical defect, observed at the model request rather than inside the hook. The mutation was immediately restored.

### Mutation 2 — todo 159 live chat-message producer: killed as intended

- After first rejecting an accidental hit on the `#[cfg(test)]` persistence helper, temporarily changed the real `prepare_user_message_with_hooks` producer's `agent` field from `input.agent` to `"mutation-agent"`. Ran `cargo test -p oc-cli --test tool_turn ordinary_plugin_lifecycle_hooks_run_through_the_real_binary --offline`.
- The production test failed with `output.message.agent = "mutation-agent"` versus live hook input `"build"`. This proves the assertion is not satisfied by presence alone or by a shared fake value. The mutation was immediately restored.

### Mutation 3 — todo 162 advertised endpoint propagation: killed in both directions

- Temporarily removed the production `model.api.endpoint` → `MODEL_ENDPOINTS_OPTION` mapping in `model_spec`, leaving Copilot's id heuristic as the only selector, then ran `cargo test -p oc-cli production_copilot_advertised --offline`.
- Both adversarial tests failed: heuristic-hostile `mai-code-1-flash-picker` selected Chat while the fixture supplied Responses events (`the summary model returned no text`), and heuristic-Responses `gpt-5` selected Responses while the fixture supplied Chat events (`unrecoverable provider failure`). The inverse pair therefore detects loss of declared metadata rather than merely pinning one favorable id. The mutation was immediately restored.

### Additional seam F2-B5 — BLOCKER: the selected TUI factory receives the server API, not `TuiPluginApi`

- Todo 160 repaired only factory selection. The production TUI now starts a second resident host with kind `tui`, but `shim.mjs` still calls `buildInput(params)` and invokes every factory as `factory.fn(input, options)`. That `input` is the ordinary server `PluginInput` (`client`, `project`, `directory`, `worktree`, `serverUrl`, `experimental_workspace`, `$`). It has none of upstream 1.18.15's required TUI API (`renderer`, `slots`, `ui`, `state`, `theme`, `keymap`, routes, lifecycle, etc.), and the shim supplies no third `TuiPluginMeta` argument.
- The shipped PTY test's TUI fixture ignores its first argument and returns `{}`, so it proves only that a function named `tui` was invoked. I temporarily strengthened that same production fixture to require `api.renderer`, `api.slots`, `api.ui`, `api.state`, and non-null `meta`, then ran `cargo test -p oc-cli --test tui_turn interactive_tui_selects_tui_plugin_factory_from_production_config --offline`. The test failed after ten seconds with an empty TUI marker: a contract-using TUI plugin cannot initialize. The fixture mutation was immediately restored.
- This is not declared in `docs/divergences.toml` or the known-gap inventory, while `PluginKind::Tui` and the production selector advertise support. The code also retains the incompatible TUI host only to shut it down; it has no bridge into `oc-tui`. Therefore the old zero-caller seam was moved from selecting the variant to consuming the selected plugin API. Either implement the TUI host contract, or remove/narrow the advertised TUI plugin capability and record that decision honestly.

### Ignored and conditionally skipped-test audit

- One explicit `#[ignore]` exists: the documented two-hour, 500-turn soak. It is not used as acceptance evidence for todos 154–164 and is not a blocker for this review.
- Several oracle/installed-plugin differentials return successfully after printing `SKIPPED`. The skip messages are explicit rather than silent, and this host currently contains the pinned oracle and supported plugin cache required by the frozen gate. They remain an environmental weakness for portable CI, but no todo disposition above relies solely on a conditionally skipped witness: the repaired paths have isolated production tests, except todo 154 whose missing turn-level test is already a blocker.
- The zero-non-test-caller scan reconfirmed two already-declared gaps rather than finding hidden green evidence: `StreamProjector` is test-only and the missing step parts are recorded as `assistant-turn-step-parts`; `CompatV1State::with_toast_forwarder` is test-only and the bare-server sink limitation is included in `v1-surface-unbacked`. F2-B5 is the new undeclared seam.

## Workspace gate

- Gate attempt 1 ran the required chain `cargo test --workspace --offline && cargo clippy --workspace --all-targets --offline && cargo fmt --all --check`. Product tests progressed through the workspace without an assertion failure until the `oc-testkit` library harness could not enumerate tests: `io error when listing tests: Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`. The harness's worker senders then reported `SendError`; because the test command failed, the `&&` chain correctly did not run Clippy or fmt.
- This is a host process/thread exhaustion failure, not a green gate and not a product assertion. In accordance with the two-attempt ceiling, one final serialized rerun uses `CARGO_BUILD_JOBS=1` and `RUST_TEST_THREADS=1`; its result is recorded below.
- Gate attempt 2 used `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace --offline && CARGO_BUILD_JOBS=1 cargo clippy --workspace --all-targets --offline && cargo fmt --all --check`. It got farther, including the 138 `oc-testkit` library tests, 16 compatibility-suite tests, all five session-interop tests, and the expected single ignored two-hour soak, but the host again returned `EAGAIN` while the `oc-tools` library harness was listing its tests. There was no product assertion failure before that host error. The `&&` chain again correctly did not run Clippy or fmt.
- Per the mandated two-attempt ceiling, the gate was not retried. Therefore the required 3,426-pass total, Clippy zero-warning result, and fmt-clean result are **not verified** in this review and must not be reported as passing.
- Final cleanup checks returned empty output for `git status --short`, `git diff --check`, and `git diff -- crates`: no tracked product, test, documentation, or plan mutation remains. This report is intentionally retained under the repository-ignored `.omo/` evidence directory (`.gitignore:1:.omo`).
- No source file remains changed, so there is no changed Rust file requiring language-server diagnostics. Diagnostics for this evidence-only Markdown file could not be run: it is outside the request workspace root and no Markdown LSP is configured. The report was instead checked by direct inspection plus `git diff --check` before the final evidence-only append.

## Findings and verdict

### Blocking findings

1. **F2-B5 — advertised TUI plugins cannot consume the upstream TUI API.** Production selects `tui()`, but invokes it with server `PluginInput`, only two arguments, and no bridge into `oc-tui`. A contract-sensitive production PTY fixture fails. This is undeclared and makes the advertised capability unusable for a real upstream TUI plugin.
2. **Todo 154 acceptance evidence is incomplete.** The low-level transport timeout is credible and directly tested, but no turn/engine/CLI regression proves a stalled SSE response fails while retaining already-decoded partial assistant text. The named historical product behavior remains unverified at the required boundary.

### Non-blocking conclusions

- F2-B1 through F2-B4 are repaired at their frozen criteria; mutations killed the todo 158 and 159 regressions at production boundaries.
- Todos 155–157 and 161–164 have credible production-path evidence. Todo 162's inverse metadata tests both fail when endpoint propagation is removed. Todo 163 loads all four advertised directories and exposes discovery failures. Todo 164 derives and publishes its 19/20 unbacked v1 status honestly as a known gap.
- The contradictory historical todo-153 `UNMET` paragraph remains stale plan prose after todo 161's resolved paragraph. It should be cleaned up by the plan owner, but the executable exact comparison is authoritative and this reviewer did not edit the frozen plan.

### Verdict

**REJECT.** The suite's existing green tests do not close F2-B5, and todo 154 does not meet its explicitly frozen turn-level acceptance criterion. Both are concrete, falsifiable, in-scope failures rather than implementation preferences.
