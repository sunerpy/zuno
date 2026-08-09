# F4 Scope Fidelity Review — Final Verification Wave 3

## Verdict: REJECT

Wave 3 materially improves the artifact: 44 of 58 upstream `/api` operations now have local backends, session mutation uses the production turn path, the oracle is honestly pinned to the newest installed release, and the ten-table prune correction is now an explicit owner-approved contract amendment. The artifact still does not close three frozen success criteria. These are missing promised behavior or proof, not requests for additional scope.

## Blockers

### 1. Fourteen upstream `/api` operations still have no backend, and 89 required comparison dimensions remain exempt

Success criterion 4 requires every upstream path+method to exist **and behave**, with status, normalized body, and observable side effect compared against the real binary (`.omo/plans/opencode-rust.md:1270`). The matrix does have one row for each of the 58 upstream operations and rejects `501`, but it pins only 85 of 174 dimensions as compared and exempts 89 (`crates/oc-testkit/tests/compat_suite.rs:1762-1808`). More decisively, it asserts that 14 operations return `503 backend_unavailable` and only 44 have local backends (`:1811-1864`). Those 14 routes are enumerated by `unsupported_routes()` (`crates/oc-server/src/api/mod.rs:148-193`) and are correctly reported as a known gap rather than laundered into a divergence (`crates/oc-testkit/tests/compat_suite.rs:2677-2683`). Honest accounting does not satisfy the behavioral contract.

The matrix also deliberately removes `/api/session/{sessionID}/compact` and `/wait` from `Compared` because the isolated oracle returns `503` without a provider (`compat_suite.rs:929-943`). That is the correct evidentiary choice: comparing two unavailable fixture paths would be false parity. It nevertheless leaves their cross-process status, body, and side-effect parity unproved, as do the remaining visible fixture exemptions.

**Required resolution:** implement the 14 missing backends or obtain an explicit narrowing of success criterion 4. Seed deterministic shared fixtures for every non-divergent operation, including provider-backed compact/wait behavior, so all three required dimensions are compared rather than exempted.

### 2. The day-one Kiro plugin criterion is internally stale and its corrected intended behavior is not proved

Success criterion 6 still names `@sunerpy/opencode-kiro-auth@0.18.0`, requires `client.middlewareStack.add`, and requires providers to appear in `models --format json` (`.omo/plans/opencode-rust.md:1273-1275`). Todo 60 correctly explains that `middlewareStack` is not on `PluginInput.client`; it belongs to the Kiro plugin's internal AWS client, so the valid assertion is a real Kiro request proving the injected header and effort fields (`:680-686`). Todo 60 also says the user's config pins `0.20.6`, while the committed surface capture and executable test use `0.20.1` (`docs/v1-surface-capture.md:29-38`; `crates/oc-plugin/tests/js.rs:279-301`). The correction therefore did not reach a single consistent acceptance contract.

Even under the corrected behavioral reading, `js_real_supported_plugins_load_with_their_own_sdk_clients` silently returns when either cache directory is absent, then proves only that two plugins load, expose auth providers, and have SDK init reports (`crates/oc-plugin/tests/js.rs:279-319`). It does not assert model-catalog visibility and does not issue a Kiro provider request to prove the AWS middleware header or effort fields. Thus neither the literal success criterion nor its documented intended replacement is met.

**Required resolution:** amend criterion 6 to the actual user-pinned package version and the already-approved behavioral middleware requirement, then add a non-vacuous integration gate that loads that version, proves its models appear through the supported catalog/CLI surface, and observes the Kiro AWS middleware effects on a real or recorded provider request.

### 3. Goal persistence is proved across one compaction for objective text, not across two compactions with counters intact

Success criterion 11 requires a goal to survive **two compactions** with objective and counters intact (`.omo/plans/opencode-rust.md:1277-1281`). The relevant test constructs one compaction boundary, confirms the old synthetic goal fragment is discarded, changes the SQL objective, and checks one regenerated injection (`crates/oc-goal/src/continuation_tests.rs:41-78`). It performs no second compaction and does not assert `tokens_used`, `time_used_seconds`, or budget counters across the sequence. The other goal tests independently cover persisted counters, projection ownership, guarded continuation, and one-shot deferral, but they do not compose the required two-compaction scenario.

**Required resolution:** add an end-to-end goal regression that records non-zero counters, forces two consecutive real compactions, and after each one proves that the next request is regenerated from authoritative SQL with the same objective and counters.

## Explicit Scope Determinations

1. **Is real turn execution behind `POST /api/session/{sessionID}/prompt` scope expansion? — No.** `serve` delegates through `TurnHost::drive_with_message_id`, which reaches the existing `run_turn` path; it implements the promised session-continuation behavior instead of introducing a second engine (`.omo/evidence/task-129-opencode-rust.txt:16-28,44-54`).
2. **Is removing `/compact` and `/wait` from `Compared` honest? — Yes, but it is not closure.** The isolated oracle's `503` cannot establish upstream success semantics. Dedicated local tests are valuable, but they do not replace the success criterion's required oracle differential.
3. **Do operation-specific `503 backend_unavailable` responses satisfy the `/api` contract? — No.** They are preferable to fabricated success and are correctly labeled as gaps, but criterion 4 promises behavior, not only registration and diagnosability.
4. **Are the latest-oracle and ten-table amendments honest scope corrections? — Yes.** Criterion 1 now follows the plan owner's “latest installed release” instruction and binds the recorded `1.18.15` pin to the executed binary and live `/doc` capture (`.omo/evidence/task-130-opencode-rust.txt:14-24,42-78,133-176`). Criterion 13 explicitly records that the pinned schema has ten session-attributable tables and aligns the contract with `PRUNE_TABLES`/`DELETE_ORDER` (`.omo/plans/opencode-rust.md:1283-1286`). Neither correction conceals missing implementation.

## The Four Requested Implementation Properties

- **No first-party `unsafe`: satisfied.** The workspace lint forbids unsafe code, the roster gate covers all 36 members, and `release_surface.rs` independently scans first-party source while also checking every member inherits workspace lints (`crates/oc-cli/tests/release_surface.rs:465-501,552-589,592-681`).
- **Rust-authorable plugins: satisfied.** `examples/rust_plugin.rs` defines one Rust tool and three hooks and runs the reusable `ConformanceSuite` without JavaScript (`examples/rust_plugin.rs:10-75,107-157`); the SDK's own tests require exact coverage of declared tools and hooks (`crates/oc-plugin-sdk/tests/conformance.rs:1-48`).
- **No shipping model-id literal in `oc-agent`: satisfied.** The source-wide guard scans shipping `.rs` files, verifies every excluded `tests.rs` is actually `#[cfg(test)]`, has anti-vacuity floors, and includes planted positive controls (`crates/oc-agent/src/model_policy/tests.rs:903-1026`).
- **Goal behavior: partially satisfied, therefore not accepted.** Split status ownership, objective-edit/status-rejection projection behavior, guarded exactly-once idle continuation, and SQL regeneration after one compaction are covered. The two-compaction objective-and-counter requirement remains blocker 3.

## Closed Prior Findings and Scope-Creep Assessment

- The two SSE routes, twelve-entry divergence allow-list, and exact 36-crate roster are closed. Every nominated behavioral difference resolves to a declared entry and names a live, non-ignored assertion (`docs/divergences.toml`; `crates/oc-testkit/tests/compat_suite.rs:2620-2674`).
- The 36-crate roster is a deliberate, bidirectionally gated amendment. `oc-process` and `oc-reaping-fixture` support the required G6 process-containment evidence rather than adding a product surface.
- C8 maintenance, slim agents, durable goals, and cross-session memory are explicitly approved additive scope. The 14 API gaps are incomplete promised scope, not scope creep.
- No prohibited hosted service, billing/share control plane, bundled JavaScript runtime, OpenSSL default, or first-party unsafe implementation was found.

## Non-Blocking Observations

1. The 12 divergence decisions are now consistently declared and guarded, but the success-criterion summary at `.omo/plans/opencode-rust.md:1292-1293` still lists only the original seven categories. The executable allow-list is authoritative; the summary should be regenerated or expanded for reader accuracy.
2. Native Windows G6 execution remains unverified on this Linux host. The committed evidence reports that limitation rather than claiming a Windows pass.
3. Per instruction, this review did not rerun the approximately 100-minute memory gate or the two-hour soak. Their committed evidence was reviewed without changing the frozen performance surface.

## Review Basis

This was an independent scope-fidelity audit of the amended plan, source, tests, generated documentation, and committed remediation evidence. No source, test, documentation, plan, commit, branch, or remote state was modified; this report is the sole deliverable.

F4 VERDICT: REJECT
