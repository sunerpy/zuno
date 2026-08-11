# F2 Code-Quality Review — Eighth Wave (Todos 145–149)

## Review scope

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Audited HEAD: `2e57e490c84224f44ff3ba8469cf9dd8dfa1b9e8`
- Method: source/contract audit, hook-by-hook production-boundary review, adversarial payload probes, mutation testing, restoration checks, and workspace-wide validation
- Reviewed the wave-7 production-hook blocker correction, the `HookName::ALL` support matrix and generated documentation, JavaScript bridge depth handling, all eight provider wire-family registrations, and pinned-oracle routing.
- Frozen performance files were not touched.
- No product or test change was retained. No commit, branch, push, merge, or remote operation was performed.
- Every temporary probe described below was restored before the final gates.

## Verdict summary

Wave 8 substantially improves production coverage. The support matrix is derived from all 21 `HookName` variants, each hook now has a production adapter, and the eight provider wire families have dispatch-plus-recorded-decode tests selected from catalog metadata rather than hard-coded model IDs. Differential tests also use the centralized pinned oracle and fail loudly unless absence is explicitly allowed.

The result is still not approvable. Ordinary mutable JavaScript hooks pass their arguments through a bounded encoder with `MAX_DEPTH = 8`, but their write-back path does not reject the encoder's synthetic `$truncated` values. `tool.definition` sends real, deeply nested built-in JSON Schemas through this path. A no-op JavaScript plugin can therefore replace valid portions of the actual provider request with truncation-marker objects merely by participating in the hook. The auth-loader path already detects and rejects the same condition, proving that the required guard exists but is not applied to ordinary hook write-back.

This is silent production corruption, not only a test limitation: the hook returns successfully and the lossy value is committed to the request. The production witness remains green because it does not assert byte/structure preservation of the deep schema. The suite therefore claims successful lifecycle coverage while accepting a provider request damaged by the bridge itself.

Two additional observations reinforce the test-honesty concern: `experimental.chat.messages.transform` can mutate `user.parts[0].text` without that mutation reaching the real continuation, while its lifecycle witness observes only the legacy `user.info.content[0].text` route; and production `chat.message` input omits upstream fields including `id`, `sessionID`, `agent`, and `model`. Separately, production plugin construction always defaults to `PluginKind::Server`; no production caller constructs `PluginKind::Tui`, even when the TUI reuses the runtime.

## Blocking finding

### F2-B1 — Ordinary JavaScript hook write-back silently commits bounded-encoder truncation

**Locations:**

- `crates/oc-plugin/src/js/shim.mjs` — bounded object encoder and `MAX_DEPTH = 8`
- `crates/oc-plugin/src/js/plugin.rs` — ordinary JavaScript hook invocation and mutation write-back
- `crates/oc-plugin/src/js/bridge.rs` — auth-loader-specific `truncated_path()` rejection
- `crates/oc-cli/src/cmd/plugin_runtime.rs` — production hook adapters, including `tool.definition`
- `crates/oc-cli/tests/tool_turn.rs` — real-turn lifecycle witness that currently permits the damaged deep schema

The bridge deliberately bounds JavaScript conversion depth. Once the depth is exceeded it emits a JSON object containing `$truncated`. This can be a valid defensive encoding policy only if a value containing that marker is never mistaken for an ordinary plugin mutation and committed back to production state.

That rejection exists for auth-loader output: `bridge.rs` checks `truncated_path()` and returns a path-bearing error. The normal hook path in `plugin.rs` does not perform the corresponding check before deserializing and applying mutated arguments. As a result, the following production sequence is possible:

1. The turn builds a valid built-in tool definition containing a JSON Schema deeper than eight object levels.
2. `tool.definition` exposes that real definition to a JavaScript plugin.
3. The plugin callback performs no meaningful mutation and returns normally.
4. The bounded bridge representation already contains `$truncated` below the depth limit.
5. Ordinary write-back accepts that representation and overwrites the valid deep schema in the actual provider request.

The corruption is silent: there is neither an error nor a preserved original value. Memory and handle limits in `host.rs` constrain resource use but cannot restore data already replaced by the encoder. Per-argument encode roots and JSON Pointer reporting are otherwise sound; the missing ordinary-hook rejection is the defect.

**Closure conditions:**

1. Before applying any mutable JavaScript hook result, recursively detect bridge truncation markers in every write-back argument and reject the hook with the precise argument-relative JSON Pointer, using semantics equivalent to the auth-loader guard.
2. Do not partially commit a multi-argument mutation when any argument is truncated. Preserve all original production values on rejection.
3. Add a production-shape regression test using the real deep built-in `tool.definition` schema. A no-op plugin must either preserve the schema exactly or fail explicitly before provider dispatch; `$truncated` must never reach the provider request.
4. Add the same preservation/error assertion to other mutable ordinary-hook argument shapes so the safety property is attached to the shared write-back boundary rather than only one fixture.

## Additional contract/test-honesty findings

### `experimental.chat.messages.transform` does not preserve the canonical parts mutation

An adversarial plugin mutation to `user.parts[0].text` was visible inside the hook but did not affect the real continuation. The lifecycle test observes mutation through `user.info.content[0].text`, so it can remain green while the canonical parts payload is discarded. Closure requires defining the authoritative message shape, applying mutations from that shape to the continuation, and adding a production-surface assertion on the resulting model request.

### `chat.message` production input is structurally incomplete

The production adapter supplies a reduced message object without the upstream `id`, `sessionID`, `agent`, and `model` fields. A dispatch witness proves that the callback runs, but not that a plugin written to the advertised contract receives the promised payload. Closure requires either supplying the full shape or explicitly documenting and testing a scoped divergence.

### `PluginKind::Tui` has no production construction path

`configured_plugins()` constructs default `JsPluginSpec` values, whose kind is `Server`. Production code has no call to `JsPluginSpec::with_kind(PluginKind::Tui)`; the TUI reuses the server-created runtime. Therefore TUI-specific host semantics are currently test-constructible but not production-selectable. This should either be wired at the TUI composition root or removed from the advertised production capability.

## Audit results by todo

### Todo 145 — Plan/history scope

- The merged scope matched the plan-text claim; no unintended product implementation was attributed to the plan-only change.
- No retained source or test mutation was introduced by this review.

### Todo 146 — Pinned differential oracle

- Active differential routes use the centralized pinned-oracle resolver.
- Missing oracle behavior is fail-loud unless `OC_TESTKIT_ALLOW_MISSING_ORACLE` is explicitly set.
- No silent skip seam was found in the audited routes.

### Todo 147 — JavaScript shim and bridge bounds

- Per-argument roots avoid one argument consuming another argument's depth budget.
- The auth-loader path rejects truncation and reports a useful JSON Pointer.
- Runtime memory/handle supervision is present.
- The ordinary mutable-hook write-back omission remains blocking as F2-B1.

### Todo 148 — Provider wire-family registration

- All eight production registrations have dispatch-plus-recorded-decode coverage.
- Selection is derived from catalog `npm` metadata rather than hard-coded model-ID matching.
- No provider-family registration blocker was found.

### Todo 149 — All-hook production lifecycle

- The matrix and generated authoring documentation cover all 21 `HookName::ALL` variants from a shared source.
- Every hook has an identifiable production adapter and lifecycle witness, closing the wave-7 zero-trigger inventory at the structural level.
- Payload preservation is not fully established: the deep-schema corruption, dropped canonical message mutation, incomplete `chat.message` shape, and unreachable TUI kind show that callback observation alone is insufficient evidence of contract-correct production behavior.

## Probe and mutation ledger

1. **Deep-schema preservation probe:** asserted that the real `tool.definition` provider request contains no `$truncated` marker after an ordinary no-op JavaScript hook.
   - **Expected:** the bridge preserves the valid production schema or rejects loss explicitly.
   - **Observed:** the assertion failed because truncation markers were written into the actual request.
   - **Restored:** yes.
2. **Canonical message mutation probe:** changed `experimental.chat.messages.transform` to mutate `user.parts[0].text`.
   - **Expected:** the continuation/model-facing message reflects the mutation.
   - **Observed:** the mutation was discarded; the existing witness only observed the legacy `user.info.content[0].text` path.
   - **Restored:** yes.
3. **Compaction provider-context mutation:** changed the fixture provider from `groq` to `openai-compatible` in `compaction_plugin_hooks_mutate_the_real_summary_request_and_continuation`.
   - **Expected:** a production-valid provider context remains available to the hook.
   - **Observed:** the test failed with `plugin hook provider context 'openai-compatible' is unavailable`, showing the witness is coupled to its synthetic provider fixture rather than freely exercising another production catalog context.
   - **Restored:** yes.
4. **TUI construction audit:** searched production construction/call paths for `PluginKind::Tui` and `with_kind`.
   - **Observed:** only test/explicit construction can select TUI kind; no production mutation was fabricated because there is no production call to delete.

## Final validation

After every temporary mutation was restored:

- `cargo test --workspace --offline` — passed on the first attempt with **3390 passed, 0 failed**.
- `cargo clippy --workspace --all-targets --offline` — passed with no warnings or errors.
- `cargo fmt --all --check` — passed.
- The workspace was clean before this report was created; no product or test file remained modified.

## Not independently verified

- CodeGraph was unavailable for this isolated worktree because it has no local index. Source was inspected directly.
- No external network, released-binary, or upstream-runtime comparison was rerun; this review focused on the audited local HEAD and its offline gates.
- The production TUI-kind gap was established structurally rather than by deleting a call site, because no production TUI-kind construction call exists.

F2 VERDICT: REJECT
