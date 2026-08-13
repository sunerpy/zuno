# F2 Code Quality Review — Round 2

- Audited HEAD: `647a2d64`
- Scope: delta-only verification of the frozen six-entry Round 1 ledger.
- Governing protocol: `.omo/plans/opencode-rust.md:1487-1569`.

## Verdict

**APPROVE**

All six frozen Round 1 ledger entries are closed. No admissible delta-introduced
Blocker was found, and the required workspace gate is clean.

## Closure answers

1. Criterion 4 / v1 plugin SDK routes: **YES — CLOSED**
2. `tool.definition` host truncation and plugin disablement: **YES — CLOSED**
3. `auth.loader` isolation and credential semantics: **YES — CLOSED**
4. F2-B7 / `engines.opencode` version gate: **YES — CLOSED**
5. Top-level config `model` wiring: **YES — CLOSED**
6. `PluginInput.client` projection: **YES — CLOSED**

## Gate

- `cargo test --workspace --offline`: **PASS** — 217 test summaries, 3,473
  passed, 0 failed, 2 ignored.
- `cargo clippy --workspace --all-targets --offline`: **PASS** — completed the
  workspace `dev` profile with zero warnings.
- `cargo fmt --all --check`: **PASS** — clean.

## Evidence

### 1. Criterion 4 / v1 plugin SDK routes — CLOSED

`crates/oc-server/src/compat_v1.rs:282-345` declares the recorded Antigravity
`auth.set` and Kiro OAuth routes as locally backed, while
`crates/oc-server/src/compat_v1.rs:483-508` registers those backends in the
router. The positive contracts do more than accept a shaped response:
`compat_v1_auth_set_persists_the_recorded_antigravity_oauth_payload`
(`crates/oc-server/tests/compat_v1.rs:1320-1355`) asserts the recorded OAuth
credential reaches the shared store, and the Kiro authorize/callback tests at
`:1357-1435` assert method-zero invocation plus callback credential persistence.

Mutation evidence:

- Replacing the registered Antigravity auth backend with `NotImplemented` made
  `compat_v1_auth_set_persists_the_recorded_antigravity_oauth_payload` fail at
  `crates/oc-server/tests/compat_v1.rs:1337` with `left: 501`, `right: 200`.
- Replacing both registered Kiro OAuth backends with `NotImplemented` made
  `compat_v1_kiro_oauth_authorize_invokes_method_zero_with_the_recorded_payload`
  and
  `compat_v1_kiro_oauth_callback_invokes_method_zero_and_persists_its_credential`
  fail at `:1372` and `:1410`, each with `left: 501`, `right: 200`.
- After restoration, the Antigravity positive test passed (`1 passed; 0 failed`),
  and `git diff --exit-code -- crates/oc-server/src/compat_v1.rs` was clean.

### 2. `tool.definition` host truncation and plugin disablement — CLOSED

The rewritten fixture is not accommodating the new behavior. At
`crates/oc-cli/tests/session_mutation.rs:17-31`,
`FAILING_TOOL_DEFINITION_PLUGIN` now throws an explicit, plugin-owned error only
on the `question` definition; the separate fixture at `:33-44` is a genuine
no-op. The failure test at `:920-989` requires one deliberate failure, a
diagnostic naming its cause, turn completion, and no later hook calls. The
positive HTTP test at `:991-1103` compares every provider-visible tool schema
byte-for-byte to a plugin-free baseline, rejects any disablement diagnostic, and
requires hook calls for every advertised tool. Those are real behavioral
contracts, not relaxed assertions.

The implementation carries host/plugin provenance out-of-band in
`crates/oc-plugin/src/js/shim.mjs:868-889`, parses that authoritative sideband at
`crates/oc-plugin/src/js/bridge.rs:607-622`, restores only host-owned cutoffs at
`:625-655`, and only disables a plugin for plugin-owned/protocol loss in
`crates/oc-plugin/src/js/plugin.rs:163-218,348-399`.

Mutation evidence:

- An initial mutation that classified every sideband entry as plugin-owned did
  **not** fail the real-schema HTTP test. I treated this as an unobservable
  (equivalent for that fixture) mutant rather than calling the test weak: the
  current built-in schemas did not exercise a depth cutoff in that run.
- The same fix-breaking mutation was observable in the dedicated deep host-data
  contract: `js_noop_hook_restores_host_truncated_input_without_blaming_the_plugin`
  failed at `crates/oc-plugin/tests/js.rs:943`, reporting the no-op plugin as the
  owner of `/args/deep/...` truncation. This proves the provenance/restoration
  branch is guarded.
- After restoration, that named test passed (`1 passed; 0 failed`) and
  `git diff --exit-code -- crates/oc-plugin/src/js/bridge.rs` was clean.

### 3. `auth.loader` isolation and credential semantics — CLOSED

`PluginRuntime::apply_catalog` now skips `auth.loader` before constructing
`getAuth` when the configured provider has no stored credential
(`crates/oc-cli/src/cmd/plugin_runtime.rs:191-212`). This is faithful to the
recorded upstream branch `provider.ts:1548-1563` (`if (!stored) continue`) and
to the SDK's `getAuth(): Promise<Auth>` type; it neither fabricates nor passes a
null credential. A genuine callback failure after a real credential reaches the
loader is converted into plugin disablement and an actionable diagnostic rather
than escaping catalog resolution (`:213-220`).

The rewritten lifecycle setup is therefore necessary, not convenient. The
production lifecycle fixture supplies `test` with a real API credential at
`crates/oc-cli/tests/tool_turn.rs:663-674`; without it, upstream-faithful code
must skip that provider's loader and the fixture would no longer exercise the
loader lifecycle. The three explicit failure fixtures likewise provide a real
credential before requiring containment: CLI `run` at `tool_turn.rs:515-535`,
HTTP at `session_mutation.rs:331-347`, and `models` at
`plugin_models.rs:353-390`.

Mutation evidence:

- Replacing the missing-credential `continue` with a fatal return made
  `configured_provider_without_stored_auth_does_not_invoke_auth_loader` fail at
  `crates/oc-cli/src/cmd/plugin_runtime.rs:1550` with `missing credential for
  groq`.
- Replacing the `auth.loader` disable-and-continue branch with a returned error
  made
  `failing_auth_loader_is_disabled_and_catalog_resolution_continues_with_a_diagnostic`
  fail at `:1578`, reporting the fixture callback error as fatal catalog
  resolution.
- Both mutations were restored; `git diff --exit-code --
  crates/oc-cli/src/cmd/plugin_runtime.rs` was clean.

### 4. F2-B7 / `engines.opencode` version gate — CLOSED

Yes, F2-B7 is closed. `PackageManifest` reads the `engines` object and
`gate_for` selects only its `opencode` string at
`crates/oc-plugin/src/js/spec.rs:344-361,464-483`; dependency and peer-dependency
versions are no longer inputs to compatibility. `load_one` returns a
`Compatibility` diagnostic before creating `JsPluginInput`, starting the host,
importing the module, or building the plugin
(`crates/oc-plugin/src/js/loader.rs:148-164`). This now matches the documented
skip-before-import contract at `docs/plugin-authoring.md:37-46`.

Mutation evidence:

- Removing only the early return while leaving the computed diagnostic as an
  ignored value made
  `js_version_gate_skips_an_excluding_engines_opencode_package_before_activation`
  fail at `crates/oc-plugin/tests/js.rs:1155` and
  `js_version_gate_rejects_a_non_semver_engines_opencode_range` fail at `:1202`.
  The satisfying-range test still passed, proving the mutant was observable and
  specifically removed exclusion rather than breaking all loading.
- The fixture manifest contains `engines: { opencode: range }` and no dependency
  surrogate (`crates/oc-plugin/tests/js.rs:272-304`), while the tests require an
  empty loaded-plugin list, no activation marker, and a `Compatibility`
  diagnostic (`:1139-1207`).
- The mutation was restored; `git diff --exit-code --
  crates/oc-plugin/src/js/loader.rs` was clean.

I agree that `REPORTED_PLUGIN_API_VERSION = 1.18.13` is deliberate and is not the
hard-coded dependency version F2-B7 objected to. It is the port's JavaScript API
compatibility claim (`crates/oc-plugin/src/js/spec.rs:25-30`), and the short CLI
version is pinned to the same compatibility identity by
`crates/oc-cli/tests/surface.rs:74-88`. The differential oracle is independently
pinned to installed release `1.18.18`; the two-pin distinction is explicit at
`crates/oc-testkit/src/oracle.rs:30-40,81`.

### 5. Top-level config `model` wiring — CLOSED

The production turn resolver now selects in the explicit order surface option,
agent model, top-level `config.model`, then deterministic catalog fallback at
`crates/oc-cli/src/cmd/turn.rs:200-213`. The value is read after plugin config
hooks run (`:163-188`) and directly determines the provider/model sent into the
resolver (`:212-230`), so it is no longer merely parsed and echoed.

The production tests are resistant to catalog-order coincidences. They launch
two independent loopback providers, keep `model: "zzz/zzz-model"` fixed, swap
the physical ALPHA/BETA labels in both directions, and assert captured request
counts plus rendered provider output
(`crates/oc-cli/tests/configured_model.rs:1-8,251-310`). They also retain the
unset deterministic fallback at `:275-291,313-320`.

Mutation evidence:

- Removing only `.or(config.model.as_deref())` made both
  `cli_honors_configured_model_when_zzz_is_beta` and
  `cli_honors_configured_model_when_zzz_is_alpha` fail at
  `crates/oc-cli/tests/configured_model.rs:257`. Each captured two requests at
  catalog-first `aaa` and zero at configured `zzz`, with the wrong route marker
  rendered.
- The mutation was restored; `git diff --exit-code --
  crates/oc-cli/src/cmd/turn.rs` was clean.

### 6. `PluginInput.client` projection — CLOSED

The generated-client boundary is now explicitly classified: `ProviderList`
maps to `LegacyCatalogHttp`, the V2 operations map to their typed V2
projections, and the unbacked config-provider operation is named rather than
silently inheriting a default
(`crates/oc-plugin/src/js/projection.rs:23-97,342-380`). The v1 `/provider`
adapter delegates to a typed canonical-catalog projection at
`crates/oc-server/src/compat_v1.rs:1161-1165`; that projection reads the original
catalogue model directly (`crates/oc-server/src/api/provider.rs:616-645,808-837`)
rather than reverse-projecting the already reduced V2 wire value.

The principal regression is a real generated-client test, not a hand-authored
HTTP assertion. `crates/oc-cli/tests/generated_sdk_provider.rs:9-117` starts the
production router, loads installed `@opencode-ai/sdk@1.15.13`, invokes
`PluginInput.client.provider.list()`, and checks provider metadata plus exact
model date, capabilities, modalities, limit, and cache cost. The direct router
contract at `crates/oc-server/tests/compat_v1.rs:425-476` independently pins the
canonical projection and long-context cost semantics.

Mutation evidence:

- Replacing the canonical `release_date` copy at
  `crates/oc-server/src/api/provider.rs:812` with a second epoch-millisecond
  conversion made
  `plugin_input_client_provider_list_observes_the_production_sdk_projection`
  fail at `crates/oc-cli/tests/generated_sdk_provider.rs:107`: observed
  `"1764547200000"`, expected `"2025-12-01"`.
- After restoration, the same real generated-client test passed (`1 passed; 0
  failed`) and `git diff --exit-code -- crates/oc-server/src/api/provider.rs`
  was clean.

The unguarded mutation at `crates/oc-plugin/src/js/projection.rs:275` is a
**Follow-up, not an admissible regression**. That line is the pre-existing
legacy hook/resource reverse-projection path, was unchanged by todo 176, and is
not the generated HTTP client boundary this fix introduced. A mutation with no
failing test also does not itself demonstrate a concrete wrong answer. Under the
delta-only protocol it cannot become a new Blocker; recording the missing guard
does not block this ledger entry.

## Follow-ups

- Non-blocking: add a direct guard for the pre-existing legacy
  hook/resource reverse-projection mutation at
  `crates/oc-plugin/src/js/projection.rs:275`. This was not introduced by todo
  176, does not govern the generated HTTP client boundary fixed here, and no
  concrete wrong answer was demonstrated; the delta-only protocol therefore
  excludes it as a Round 2 Blocker.
