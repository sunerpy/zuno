# F1 Round 2 Plan Compliance Audit

- Audited HEAD: `647a2d64`
- Scope: Round 2 delta-only review of the frozen six-entry ledger
- Governing protocol: `.omo/plans/opencode-rust.md:1487-1569`
- Verdict: **APPROVE**

## Ledger closure rulings

### 1. Criterion 4 / measured pre-`/api` plugin SDK routes — **YES, CLOSED**

Criterion 4 is now satisfied for this frozen finding. `V1_BACKENDS` contains 14
registered backends (13 SDK/API routes plus `/tui/show-toast`) at
`crates/oc-server/src/compat_v1.rs:483-550`; `v1_coverage()` derives `served`
from `V1_BACKENDS.len()` at lines 607-622. The executable accounting test pins
20 measured / 14 served / 6 unbacked at
`crates/oc-server/tests/compat_v1.rs:326-333` and drives every declared route
against the router at lines 1053-1124.

The side effects and corrected contracts are exercised directly: auth persistence
(`compat_v1_auth_set_persists_the_recorded_antigravity_oauth_payload`), Kiro OAuth
authorize/callback invocation and callback credential persistence, OMO summarize
model/`auto` propagation, Antigravity `tool_result` prompt propagation, and required
session fields including `slug` (`compat_v1_session_projection_satisfies_the_published_session_schema`).
The projection carries `slug` at `crates/oc-server/src/compat_v1.rs:1390-1416`.

Command run: `cargo test -p oc-server --test compat_v1 --offline` — **31 passed,
0 failed**. No regression directly introduced by this ledger fix was observed.

### 2. `tool.definition` unusable by JS plugins / host blamed plugin — **YES, CLOSED**

The JS bridge now distinguishes host-side from plugin-side truncation metadata and
restores only host-truncated values from the original argument
(`crates/oc-plugin/src/js/bridge.rs:595-655`). `invocation_output` applies that
restoration before scanning for genuine plugin truncation and refuses only an
unrestorable or plugin-originated mutation
(`crates/oc-plugin/src/js/plugin.rs:348-399`).

Targeted commands passed:

- `js_noop_hook_restores_host_truncated_input_without_blaming_the_plugin` — 1 passed.
- `noop_tool_definition_hook_preserves_real_schemas_and_stays_enabled` — 1 passed;
  the test compares all provider-visible tool definitions byte-for-byte with a
  plugin-free baseline and rejects leaked `$truncated` markers
  (`crates/oc-cli/tests/tool_turn.rs:1519-1596`).
- `noop_tool_definition_hook_preserves_real_schemas_and_stays_enabled_over_http`
  — 1 passed; the HTTP turn completed without `session.error` or plugin-fault
  diagnostic and preserved schemas byte-for-byte
  (`crates/oc-cli/tests/session_mutation.rs:991-1103`).

No regression directly introduced by this ledger fix was observed.

### 3. Failing plugin `auth.loader` killed `run`, `models`, and HTTP turns — **YES, CLOSED**

`PluginRuntime::apply_catalog` now catches each `auth.loader` error, disables only
that plugin through `disable_after_callback_failure`, and continues catalog
resolution (`crates/oc-cli/src/cmd/plugin_runtime.rs:191-221`). The corresponding
unit test confirms catalog resolution returns success, emits one diagnostic naming
plugin/hook/cause, and leaves the failed plugin disabled
(`crates/oc-cli/src/cmd/plugin_runtime.rs:1560-1597`).

All four direct tests passed independently:

- `failing_auth_loader_is_disabled_and_catalog_resolution_continues_with_a_diagnostic`
- `failing_auth_loader_is_disabled_and_cli_run_completes_with_a_diagnostic`
- `failing_auth_loader_is_disabled_and_models_lists_models_with_a_diagnostic`
- `failing_auth_loader_is_disabled_and_http_turn_completes_with_a_diagnostic`

The `models` test asserts useful stdout remains `test/test-model` and the HTTP
test asserts `turn.completed`, no `session.error`, and a completed assistant
message (`crates/oc-cli/tests/plugin_models.rs:353-413` and
`crates/oc-cli/tests/session_mutation.rs:1105-1162`). No regression directly
introduced by this ledger fix was observed.

### 4. Version gate read the wrong manifest field and loaded incompatibles — **YES, CLOSED**

The gate reads only the string at `package.json.engines.opencode`
(`crates/oc-plugin/src/js/spec.rs:344-368,464-479`), computes the verdict during
npm package resolution (`spec.rs:218-235`), and rejects `Unsatisfied` before the
JS host is started or the plugin factory runs
(`crates/oc-plugin/src/js/loader.rs:148-187`).

`cargo test -p oc-plugin --test js --offline js_version_gate_` passed all three
tests: an excluding range is skipped before activation, a satisfying range loads,
and a non-semver range is rejected. The exclusion test explicitly confirms the
factory marker was never created (`crates/oc-plugin/tests/js.rs:1138-1207`).

I agree that `REPORTED_PLUGIN_API_VERSION = "1.18.13"` is deliberate, not a stale
oracle pin. The source defines it as this port's JavaScript-API compatibility
claim (`crates/oc-plugin/src/js/spec.rs:25-30`), while the CLI test independently
asserts that compatibility version and Rust build identity are distinct and that
short `--version` reports `1.18.13`
(`crates/oc-cli/tests/surface.rs:74-94`). That targeted test also passed. No
regression directly introduced by this ledger fix was observed.

### 5. Top-level config `model` was parsed and echoed but ignored — **YES, CLOSED**

The production turn path now resolves model precedence as command option, then
agent model, then top-level `config.model`, before deterministic catalog fallback
(`crates/oc-cli/src/cmd/turn.rs:200-229`).

`cargo test -p oc-cli --test configured_model --offline` — **6 passed, 0
failed**. These are production-binary tests with two live loopback providers. The
four configured cases swap the physical ALPHA/BETA endpoints while retaining
`model: "zzz/zzz-model"`, assert zero requests reach catalog-first `aaa`, assert
both title and turn reach `zzz`, and cover CLI plus PTY-backed TUI. The two unset
cases preserve the prior deterministic fallback
(`crates/oc-cli/tests/configured_model.rs:1-8,251-321`). No regression directly
introduced by this ledger fix was observed.

### 6. `PluginInput.client` was an unprojected model boundary — **YES, CLOSED**

The boundary vocabulary now includes all six generated-client model/provider
arrival classes. Its exhaustive `JsModelArrival::projection()` match classifies
legacy `/provider`, v2 model/provider operations, lightweight selections, and the
honestly unbacked config-provider operation
(`crates/oc-plugin/src/js/projection.rs:35-97`). Adding an enum variant without a
projection has no wildcard fallback and therefore fails compilation; the current
complete mapping is pinned by
`every_generated_client_model_arrival_has_an_explicit_projection`
(`projection.rs:342-361`). That test passed.

The production legacy route now projects directly from the canonical catalogue
instead of reverse-projecting the reduced v2 shape
(`crates/oc-server/src/api/provider.rs:469-476,616-639`). The already-run
`oc-server` compatibility suite passed
`compat_v1_provider_projection_preserves_catalog_model_semantics`, which checks
SDK spelling and meaningful provider/model fields including exact release date,
capabilities, modalities, limits, and cache cost
(`crates/oc-server/tests/compat_v1.rs:425-476`).

`plugin_input_client_provider_list_observes_the_production_sdk_projection` passed.
The installed SDK entry at
`/config/.bun/install/cache/@opencode-ai/sdk@1.15.13@@registry.npmmirror.com@@@1/dist/index.js`
was verified present, so the test did not take its skip branch. It starts the
production router, loads that real generated SDK, invokes
`input.client.provider.list()` from JavaScript, and asserts the SDK-observed model
and provider shape (`crates/oc-cli/tests/generated_sdk_provider.rs:7-117`). The
full non-hook inventory and completeness method are recorded at
`.omo/evidence/task-176-opencode-rust.txt:20-73`. No regression directly
introduced by this ledger fix was observed.

## Verification gate

- `cargo test --workspace --offline`, attempt 1: **HOST-TRANSIENT / INCOMPLETE**.
  The run progressed through many passing targets, then failed while listing
  `oc-config` tests with `Os { code: 11, kind: WouldBlock, message: "Resource
  temporarily unavailable" }`; the subsequent `SendError` panics were fallout
  from the failed test-listing channel. No product assertion reported `FAILED`.
- `cargo test --workspace --offline`, permitted retry: **HOST-TRANSIENT /
  INCOMPLETE** at the same `oc-config` test-listing boundary with the same code 11
  `WouldBlock`. No third run was made. Therefore this audit did **not** independently
  reproduce the expected aggregate `3473 passed / 0 failed` despite all targeted
  closure tests passing.
- `cargo clippy --workspace --all-targets --offline`: **PASS**, completed with no
  warnings.
- `cargo fmt --all --check`: **PASS**, no output.

## Checked-todo ledger

Parsed checked implementation lines with `^- \[x\] (\d+)\.` from
`.omo/plans/opencode-rust.md`: **178 checked entries, 178 unique IDs, range
1-178, no duplicates, no missing IDs**.

## Unverified items

The complete workspace-test aggregate was not independently verified because both
the initial run and the one permitted retry exhausted the host's process/thread
resource while the Rust test harness was listing `oc-config` tests (`EAGAIN`, code
11). The targeted closure tests, clippy, and fmt all passed. G1/G2 and G3/G4 were
not rerun, as explicitly prohibited for this delta-only round.

`lsp_diagnostics` could not inspect the only changed file (this Markdown report):
the tool is rooted at the main checkout and rejected the sibling-worktree path as
outside its request cwd. No product source file was changed. This limitation is
reported rather than represented as a clean LSP result.

Final repository check: `git rev-parse HEAD` returned
`647a2d64d2b34a602f59b0189e613d957b40882b`; `git status --porcelain` produced no
output. The audited worktree is clean and remains at the required HEAD.

## Final verdict

**APPROVE.** All six frozen Blockers are closed. Round 2 produced no new
threshold-passing Blocker and no directly introduced admissible regression, so
the convergence condition is met. The host-transient inability to finish the
aggregate workspace test is disclosed above rather than represented as a green
3473-test run.
