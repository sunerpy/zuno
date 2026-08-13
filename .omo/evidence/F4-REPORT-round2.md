# F4 Scope Fidelity Review — Round 2

- Audited HEAD: `647a2d64`
- Review mode: Round 2 delta-only against the frozen six-entry ledger
- Final verdict: **REJECT — frozen ledger entry 6 remains open**

## Governing protocol

Read first at `.omo/plans/opencode-rust.md:1487-1569`. The review is limited to the six frozen entries; unchanged areas are not re-audited, and a new Blocker is admissible only when directly introduced by one of the six fixes and independently satisfies the frozen admission threshold.

## Incremental judgments

### Ledger accounting

Command-derived result from `.omo/plans/opencode-rust.md`: `checked_lines=178 unique_checked_ids=178 min=1 max=178`, with `missing_1_to_max=[]` and `duplicate_ids=[]`. The checked implementation-todo count is therefore **178 unique IDs**.

| Frozen entry | Closure |
|---|---|
| 1 — measured v1 plugin SDK routes | **YES** |
| 2 — `tool.definition` truncation accountability | **YES** |
| 3 — `auth.loader` failure isolation | **YES** |
| 4 — `engines.opencode` version gate | **YES** |
| 5 — top-level config `model` | **YES** |
| 6 — `PluginInput.client` model/provider boundary | **NO** |

### Entry 1 — measured v1 plugin SDK routes answered 501: **CLOSED (YES)**

- The measured route table now assigns real backends to `PUT /auth/{providerID}` and both provider OAuth routes (`crates/oc-server/src/compat_v1.rs:282-343`); router construction dispatches those backings rather than `seam_handler` (`compat_v1.rs:927-940,999-1004`).
- The handlers consume the bodies and perform observable work: `set_auth` writes the shared auth store (`compat_v1.rs:1083-1095`), authorize invokes the resident OAuth backend with provider/method/inputs (`:1098-1119`), and callback invokes it and persists the returned credential (`:1122-1151`).
- Recorded-payload regressions assert effects, not status alone: Antigravity auth persistence (`crates/oc-server/tests/compat_v1.rs:1321-1354`), Kiro method-zero authorize invocation (`:1358-1392`), and Kiro callback invocation plus credential persistence (`:1396-1434`). Todo 169's exact OMO/Antigravity shapes are effect-checked at `:713-840`; todo 171's `slug` is projected at `compat_v1.rs:1405-1415` and checked against the served `/doc` required-key set across create/list/get at `tests/compat_v1.rs:486-590`.
- Command: `cargo test -p oc-server --test compat_v1 --offline` → **31 passed, 0 failed**, including every named regression above.

No regression directly introduced by these fixes was observed within this ledger entry.

### Entry 2 — `tool.definition` unusable and host blamed the plugin: **CLOSED (YES)**

- The fix is not merely a moved depth cliff. The encoder records host-owned depth boundaries and classifies truncation as host- versus plugin-origin based on mutation at that boundary (`crates/oc-plugin/src/js/shim.mjs:94-186`). Rust rejects plugin-origin truncation first, restores host-origin branches from the original arguments, and reports an un-restorable host loss without naming the plugin (`crates/oc-plugin/src/js/plugin.rs:348-430`).
- The focused unit regressions prove both directions: a plugin-created over-depth mutation is refused atomically with plugin/hook/path attribution (`crates/oc-plugin/tests/js.rs:848-905`), while a no-op callback over host-owned deep input restores it (`:908-942` and continuation exercised by the named test).
- Production CLI and HTTP regressions compare every real built-in tool schema byte-for-byte against no-plugin baselines, require turn completion, require no disable/blame diagnostic, and require the hook to stay enabled for all definitions (`crates/oc-cli/tests/tool_turn.rs:1520-1595`; `crates/oc-cli/tests/session_mutation.rs:992-1099`).
- Commands: the two exact `oc-plugin` truncation tests and the exact CLI/HTTP `tool.definition` tests each passed (**4/4, 0 failed**).

No regression directly introduced by this fix was observed within this ledger entry.

### Entry 6 — `PluginInput.client` model/provider boundary: **NOT CLOSED (NO)**

The concrete wrong-answer path is repaired: `/provider` now projects a typed legacy document directly from the canonical catalogue (`crates/oc-server/src/api/provider.rs:282-327,808-837`), `compat_v1` delegates to it (`crates/oc-server/src/compat_v1.rs:1161-1165`), and a real installed generated SDK client observes the intended provider/model semantics (`crates/oc-cli/tests/generated_sdk_provider.rs:10-117`). The targeted behavior tests all pass.

However, the claimed durable completeness guard does **not** meet frozen todo 176's criterion at `.omo/plans/opencode-rust.md:1477`: “a new unprojected arrival path [must be] a compile error.” `GeneratedClientArrival` and its `ALL` list are handwritten (`crates/oc-plugin/src/js/projection.rs:35-54`). Their exhaustive match classifies those handwritten variants (`:67-96`), and the tests/debug assertion only iterate that same list (`:346-380`; `crates/oc-plugin/src/js/host.rs:1208-1223`). The production generated-client routes do not consume this classification: repository-wide references to `LegacyCatalogHttp`/`GeneratedClientArrival` occur only in `projection.rs` and the host's self-check, while the actual `/provider` route calls `api::provider::legacy_provider_list` directly (`compat_v1.rs:1161-1165`). Thus adding a model/provider-bearing generated HTTP operation or server route does not require a `GeneratedClientArrival` change and does not produce a compile error. The enum makes a **new enum variant** exhaustive, not a **new arrival path** exhaustive.

Judgment on the completeness method: the declaration search and route matching in `.omo/evidence/task-176-opencode-rust.txt:20-73` are a defensible one-time audit of the currently pinned clients, but the method stops short of the promised structural enforcement because its result is not linked to the generated response types or router registrations. This is not a ninth-layer search; it is the explicit frozen closure criterion for the eighth layer.

Commands: the exact legacy projection, real generated SDK client, generated-arrival classifier, and non-hook resource classifier tests passed (**4/4, 0 failed**). Passing confirms current behavior, but the two classifier checks are self-referential and do not close the compile-error criterion.

**Owner's `projection.rs:275` mutation: Follow-up, non-blocking.** `git blame -L 268,279` attributes the unguarded `release_date` restoration to `35cda9514` on 2026-08-12, before todo 176's `e234fe94`; it was not directly introduced by any frozen-ledger fix. Under the Round 2 admission rule it cannot be a new Blocker. It is still a real missing mutation guard worth recording for later.

### Entry 5 — top-level config `model` parsed and echoed but ignored: **CLOSED (YES)**

- Turn planning now resolves model precedence as explicit request, then agent override, then top-level `config.model`, before deterministic catalog selection (`crates/oc-cli/src/cmd/turn.rs:200-213`). The genuinely unset case still reaches `select_model(..., None, ...)` and retains its catalog fallback.
- The production-path suite launches the real binary against two independently captured loopback providers; configured cases swap physical ALPHA/BETA endpoints to prevent accidental catalog-order success, and unset cases assert only catalog-first `aaa` receives title plus turn (`crates/oc-cli/tests/configured_model.rs:1-8,251-290`). It exercises both CLI and real PTY TUI paths in both endpoint directions and both unset fallbacks (`:293-320`).
- Command: `cargo test -p oc-cli --test configured_model --offline` → **6 passed, 0 failed**.

No regression directly introduced by this fix was observed within this ledger entry.

### Entry 4 — version gate read the wrong field and activated incompatibles: **CLOSED (YES)**

- Package resolution derives the gate solely from `package.json.engines.opencode` (`crates/oc-plugin/src/js/spec.rs:215-235,344-360,464-482`); the prior `dependencies`/`peerDependencies["@opencode-ai/plugin"]` concern is absent from the manifest model and no longer overlaps the runtime gate.
- `load_one` returns a compatibility diagnostic before constructing or starting the JavaScript host when the range is unsatisfied (`crates/oc-plugin/src/js/loader.rs:148-187`). The documentation states exactly that behavior (`docs/plugin-authoring.md:37-46`).
- The production-loader regressions prove excluding ranges activate zero callbacks, satisfying ranges activate, and invalid ranges skip (`crates/oc-plugin/tests/js.rs:1138-1207`).
- Keeping `REPORTED_PLUGIN_API_VERSION = "1.18.13"` is faithful, not a stale oracle pin. It is explicitly the port's JavaScript-API compatibility identity (`spec.rs:25-30`) and matches the separately asserted CLI compatibility identity (`crates/oc-cli/src/version.rs:1-12`; `crates/oc-cli/tests/surface.rs:74-88`). The oracle's 1.18.18 pin tests an external released binary; it does not redefine this port's claimed plugin API surface.
- Commands: all three exact version-gate regressions and the split-version identity regression passed (**4/4, 0 failed**).

No regression directly introduced by this fix was observed within this ledger entry.

### Entry 3 — `auth.loader` failure killed `run`, `models`, and HTTP turns: **CLOSED (YES)**

- Production catalog application now skips `auth.loader` when the provider has no stored credential, invokes it only with a real credential, and on callback failure disables that plugin while continuing (`crates/oc-cli/src/cmd/plugin_runtime.rs:191-220`). This is faithful to upstream rather than convenient: a direct read of `anomalyco/opencode` tag `v1.18.18`, `packages/opencode/src/provider/provider.ts:1548-1563`, shows `const stored = ...`, `if (!stored) continue`, then `if (!plugin.auth.loader) continue` before loader invocation.
- The no-credential case is a named no-call/no-diagnostic regression (`plugin_runtime.rs:1535-1557`); an authenticated failure is a named continue/disable/actionable-diagnostic regression (`:1560-1596`). The 67-applicable-case hook × `run`/`models`/TUI/HTTP classification has no fatal boundary and dynamically exercises hook-bus/shutdown cases (`:1502-1532`).
- Production surfaces independently prove continuation and diagnostic attribution: CLI `run` (`crates/oc-cli/tests/tool_turn.rs:1598-1632`), `models` with useful output (`crates/oc-cli/tests/plugin_models.rs:353-413`), and HTTP SSE with `turn.completed` and no `session.error` (`crates/oc-cli/tests/session_mutation.rs:1105-1162`).
- Giving the lifecycle fixture `test` credentials (`tool_turn.rs:655-678`) is faithful: that fixture asserts the auth loader actually enriches the request, so satisfying upstream's stored-credential precondition preserves the intended coverage rather than masking a failure.
- Commands: six exact closure tests passed (**6/6, 0 failed**), including no-credential skip, authenticated disable-and-continue, the hook/surface matrix, and all three affected production surfaces.

No regression directly introduced by this fix was observed within this ledger entry.

## Verification gate

- `cargo test --workspace --offline` → exit 0; **3473 passed, 0 failed** across 217 result blocks. No retry was needed.
- `cargo clippy --workspace --all-targets --offline` → exit 0; **0 warnings**.
- `cargo fmt --all --check` → exit 0; **clean**.
- No product source file was modified by this reviewer. The sole changed artifact is this Markdown report; `lsp_diagnostics` rejected the sibling-worktree path as outside its request cwd, and no Markdown language server is applicable. Rust diagnostics are covered by the clean all-target Clippy gate.

## Final verdict

**REJECT.** Entries 1-5 are closed. Entry 6 is not closed because the handwritten arrival taxonomy is disconnected from the generated SDK response types and server route registrations, so it does not make a new unprojected arrival path a compile error as frozen todo 176 requires. No new directly introduced regression was admitted. The owner's unguarded `projection.rs:275` mutation is explicitly a non-blocking Follow-up.
