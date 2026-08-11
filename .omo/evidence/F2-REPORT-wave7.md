# F2 Code-Quality Review — Seventh Wave (Todos 143–144)

## Review scope

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Audited HEAD: `0e1fe93b354e12705e6a83ba28f09259307e6053`
- Method: targeted source and contract audit, mutation testing of the new production guards, restoration checks, and workspace-wide validation
- Reviewed the sixth-wave blocker correction, the new production `Auth`/`Tool` wiring, the complete `HookInvocation` contract, and the real CLI composition root.
- Frozen performance files were not touched.
- No product or test code was retained as modified. No commit, branch, push, merge, or remote operation was performed.
- Every temporary mutation described below was restored before the final gates.

## Verdict summary

The sixth-wave blocker is closed: todo 143 now distinguishes antigravity from the catalog fixture, and removing antigravity fails its named real-plugin test. Todo 144 also has meaningful production-surface guards: removing `Auth` or `Tool` dispatch fails on the concrete user-visible consequence, and plugin tools pass through the existing permission-governed registry.

The broader plugin contract is nevertheless not production-complete. `HookInvocation` exposes 21 hooks and `docs/plugin-authoring.md` promises that plugin tiers “see the same hooks” and that every tier dispatches all 21. The real CLI composition root dispatches only four: `Config`, `Auth`, `Provider`, and `Tool`. The remaining 17 have no production `HookBus::dispatch` call site. They are exercised by tests that manually construct and dispatch invocations, or by transport/codec tests, but a real turn never creates those invocations.

This is blocking because the documented and planned contract explicitly promises the upstream hook set to plugin authors. A plugin can load successfully and advertise, for example, `chat.headers`, `chat.params`, `permission.ask`, or `tool.execute.before`, while its callback is never invoked by the actual CLI lifecycle. Green dispatcher tests prove that callbacks work *if called*; they do not prove that the product calls them.

## Blocking finding

### F2-B1 — Seventeen documented plugin hooks have no production trigger

**Locations:**

- `crates/oc-plugin/src/hooks.rs:60-137` — the 21-variant typed contract
- `crates/oc-plugin/src/hooks.rs:192-262` — generic/resource dispatch implementation
- `crates/oc-cli/src/cmd/plugin_runtime.rs:68-140` — every real CLI dispatch call
- `crates/oc-cli/src/cmd/turn.rs:163-197,431` — production plugin loading, catalog/tool extraction, and lifetime retention
- `docs/plugin-authoring.md:3-5,151-180` — the public same-hooks/all-21 promise
- `.omo/plans/opencode-rust.md:656-662` — the upstream hook set is declared to be the plugin-author contract

The production CLI has exactly these dispatches:

```text
plugin_runtime.rs:70   HookInvocation::Config
plugin_runtime.rs:82   HookInvocation::Auth
plugin_runtime.rs:103  HookInvocation::Provider
plugin_runtime.rs:137  HookInvocation::Tool
```

There is no CLI production dispatch call for:

```text
Dispose
Event
ChatMessage
ChatParams
ChatHeaders
PermissionAsk
CommandExecuteBefore
ToolExecuteBefore
ShellEnv
ToolExecuteAfter
ChatMessagesTransform
ChatSystemTransform
ProviderSmallModel
SessionCompacting
CompactionAutocontinue
TextComplete
ToolDefinition
```

Occurrences elsewhere do not close the gap:

- `oc-plugin/src/hooks.rs` implements the dispatcher but cannot initiate lifecycle events itself.
- `oc-plugin/src/jsonrpc.rs` serializes and applies invocations supplied by a caller; it is not a production caller.
- `oc-plugin/src/wasm.rs`'s visible `ChatSystemTransform` dispatches are test-module fixtures.
- `oc-plugin/tests/hooks.rs` manually dispatches the full table and therefore proves payload/order behavior only.
- `oc-plugin/tests/integration.rs`, `jsonrpc.rs`, and `js.rs` directly call or dispatch selected hooks and likewise bypass the CLI composition root.

The distinction was observable in mutation testing. Removing `PluginRuntime::load` from the production turn setup left the direct JS Kiro `ChatHeaders` test green, because that test loads the plugin and calls its hook itself. Under the same production-loading mutation, `a_real_plugin_tool_reaches_and_executes_through_the_production_registry` failed, because that test actually crosses the CLI production surface. Thus the suite already demonstrates both test shapes: direct hook tests can survive removal of production wiring, while a real production-surface test catches it. The 17 hooks above currently have only the first shape.

**Closure conditions:**

1. Wire every promised hook to its corresponding real CLI/engine lifecycle event, preserving configuration-order mutation semantics. A transport codec or test helper is not a trigger.
2. Add a contract matrix derived from `HookName::ALL` that maps each advertised hook to a production trigger/witness, so adding or leaving a hook unwired fails structurally.
3. Add failure-sensitive production-surface tests for the externally observable result of each trigger class. At minimum, deleting the production dispatch for a hook must fail a named test that enters through the real CLI/turn/session/tool path; directly calling `HookBus::dispatch` is insufficient.
4. Keep the public docs generated from the same support matrix. If a hook genuinely cannot be supported, the alternative is an explicit scoped divergence and removal of the unconditional “same 21 hooks” claim—not a green dispatcher-only test. Because the frozen plan names the upstream hook set as the plugin-author contract, silently narrowing it is not closure.

## Mutation ledger

### Todo 143 — Distinguishable antigravity evidence

1. **Mutation:** removed antigravity from `production_plugin_specs()` while retaining kiro-auth.
   - **Expected regression:** antigravity's fixture-independent auth method disappears.
   - **Result:** caught by `the_real_antigravity_plugin_registers_a_google_auth_method_no_fixture_supplies`; the observed providers contained only `kiro-auth`.
   - **Restored:** yes.
2. **Assessment:** the sixth-wave false-positive fixture is corrected. The positive evidence is an antigravity-owned auth label absent from the catalog fixture, and the same loader without antigravity is a negative control.

### Todo 144 — Production `Auth` and `Tool` wiring

1. **Mutation:** removed production `HookInvocation::Auth` dispatch.
   - **Expected regression:** antigravity's loader no longer zeroes the Google model cost.
   - **Result:** caught by `antigravity_auth_loader_zeroes_google_cost_on_the_verbose_models_surface`; fixture cost `input=1.25/output=5.0` remained instead of becoming zero.
   - **Restored:** yes.
2. **Mutation:** removed production `HookInvocation::Tool` dispatch.
   - **Expected regression:** `google_search` is absent from the real turn registry.
   - **Result:** caught by `a_real_plugin_tool_reaches_and_executes_through_the_production_registry`.
   - **Restored:** yes.
3. **Mutation:** removed both effective permission-visibility filters for the production registry/dispatcher, then rebuilt the subject binary.
   - **Expected regression:** a denied plugin tool is advertised to the model.
   - **Result:** caught by `a_plugin_tool_is_hidden_by_the_same_permission_layer_as_builtins`.
   - **Control:** removing either filter alone remained green because the other independently enforced the same visibility boundary; this is redundant defense, not a surviving authorization bypass.
   - **Restored:** yes.
4. **Assessment:** adequate for the two hooks todo 144 claims. The tests assert user-visible effects through the real binary rather than merely observing a dispatcher callback.

### Production-composition honesty

1. **Mutation:** replaced turn-side `PluginRuntime::load(...)` with `None`.
   - **Direct-test result:** `js_real_kiro_plugin_injects_its_request_kind_header_for_a_compaction_turn` remained green because it directly loads and invokes the plugin outside the production turn composition root.
   - **Production-test result:** `a_real_plugin_tool_reaches_and_executes_through_the_production_registry` failed under the same mutation, proving the mutation was observable and the direct hook test was not a production-wiring guard.
   - **Restored:** yes.
2. **Assessment:** blocking for the remaining hook contract. The current full-hook table has the direct-test shape, not the production-test shape.

### Repository policy guards

1. **Mutation:** added `pub unsafe fn f2_policy_mutant() {}`.
   - **Result:** caught by `no_first_party_source_file_uses_unsafe`.
   - **Restored:** yes.
2. **Mutation:** added an unjustified `#[allow(dead_code)]`.
   - **Result:** caught by `every_first_party_lint_suppression_has_a_reason`.
   - **Restored:** yes.
3. **History audit:** no deleted tests were found in `b753fb95..HEAD`, and no `--no-verify` use was found.

## Final validation

After all temporary mutations were restored:

- `cargo test --workspace --offline` — first attempt hit the known host-transient `EAGAIN: Resource temporarily unavailable`; the identical retry passed with **3365 passed, 0 failed, 2 ignored** across 210 result groups. The retry log contained 0 `error:` lines, 0 `warning:` lines, and no second `EAGAIN`.
- `cargo clippy --workspace --all-targets` — passed with no warnings.
- `cargo fmt --all --check` — passed.
- The successful workspace test and all-target clippy runs compiled the affected workspace and test targets; no standalone build failure was hidden.
- `git diff --check` and targeted restoration comparisons were clean before the report was written.
- `git status --porcelain` was empty before the report was written.

## Not independently verified

- CodeGraph was unavailable for this isolated worktree because it has no local index. The hook inventory was therefore checked from the authoritative enum, the CLI source, and a crate-wide occurrence search.
- The released upstream binary comparison recorded in todo 144's evidence was not rerun in this review; this review independently mutation-tested the concrete zero-cost behavior instead.
- No mutation was fabricated for each of the 17 absent call sites: there is no production dispatch statement to delete. The production-loading mutation above demonstrates why direct hook tests cannot substitute for those missing call sites.

F2 VERDICT: REJECT
