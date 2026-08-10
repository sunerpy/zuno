# F2 Fifth-Wave Code-Quality Audit

## Verdict

**REJECT.** The restored production tree passes the targeted suites and workspace build, and the mutations around credential redaction, endpoint/API-key precedence, CLI parity, ownership, and compaction accounting were caught. However, the audit proved four release-blocking weaknesses in the permanent verification surface:

1. the persist-before-live HTTP event guarantee can regress while all committed event and route tests stay green;
2. the question half of the request broker lacks the fail-closed coverage already present for permissions;
3. `oc-testkit` can silently run a stale `opencode-rust` binary and report a false green interop result; and
4. default CI does not enable `oc-plugin`'s `wasm` feature, so the advertised three-tier integration tests do not execute there.

These are test/CI blockers rather than evidence that the restored production implementation is currently wrong. They still prevent approval because each permits a material production regression to merge under a green required gate.

## Audit Baseline and Constraints

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Branch: `task-F2`
- Audited HEAD: `56c229c0abe070b72cd18a8279e3ba1ef9646446`
- Toolchain: `cargo 1.96.0`, `rustc 1.96.0`
- Lock-file preflight: `cargo metadata --locked --offline --no-deps --format-version 1` passed.
- CodeGraph was unavailable because this isolated worktree was not indexed; source was inspected directly.
- Frozen performance files were not modified.
- The long G1/G2 memory and G3/G4 soak gates were not run, as required. `OC_MEMORY_GATE_MODE=skip` was used for `oc-testkit` validation.
- No commit, push, merge, dependency change, or production fix was made.

## Blocking Findings

### B1 — Persist-before-live HTTP events are not protected by a committed route-level test

`EventService` promises that an event observed over session SSE is already replayable from durable history. The contract is documented at `crates/oc-server/src/events.rs:108-110` and depends on the append/fan-out order inside the event publisher.

Mutation evidence:

- Production was changed temporarily so a session event reached the live HTTP fan-out before the durable append, with persistence delayed long enough to make the race deterministic.
- The existing `oc-server` event suite, the `oc-cli` HTTP/SSE/history tests, and the global-event test all remained green.
- A temporary route-level probe subscribed to session SSE, consumed the event, and immediately requested history. It failed because the event just observed live was not replayable.
- The probe passed after restoration and was deleted.

Impact: a reconnect immediately after receiving an SSE event can lose that event even though the API contract says history can replay it. Existing tests separately prove live delivery and eventual replay, but not the ordering relation between them.

Required remediation: commit a deterministic route-level regression test that consumes one session SSE event and immediately proves the same sequence is present through the history/replay route. The test must fail when live fan-out occurs before durable append.

### B2 — Question requests lack the permission path's fail-closed regression coverage

The broker implements parallel permission and question paths in `crates/oc-server/src/request_broker.rs`: ownership claims at lines 230-265, watchdogs at lines 318-340, and observer-driven reclamation later in the file. Malformed reply cleanup is also parallel in `crates/oc-server/src/api/request.rs:110-163`.

Mutation evidence:

- Inverting either permission or question session ownership was caught by `api_reply_routes_validate_bodies_before_rejecting_cross_session_requests`.
- Removing malformed permission claim/drop was caught by `disconnected_permission_reply_fails_closed_without_running_the_tool`.
- Removing malformed **question** claim/drop left the committed `oc-server` API and `oc-cli session_mutation` suites green; a temporary owned-question probe failed, then passed after restoration.
- Disabling permission watchdog rejection was caught by `permission_without_an_observer_is_rejected_by_the_deadline`.
- Disabling the **question** watchdog left the committed suites green; a temporary question-deadline probe failed, then passed after restoration.
- Disabling permission observer-zero reclamation was caught by `disconnected_only_session_observer_rejects_permission_without_running_the_tool`.
- Disabling only **question** observer-zero reclamation left the committed suites green; a temporary question-observer probe failed, then passed after restoration.

Impact: malformed or disconnected question replies can leave a turn parked until an unrelated fallback occurs, and a question with no remaining observer can survive instead of failing closed. The production question implementation currently mirrors permissions, but the committed tests do not enforce that symmetry.

Required remediation: add permanent question equivalents for malformed owned reply cleanup, observer-zero rejection, and deadline rejection. Each test must isolate its trigger so channel closure or another fallback cannot satisfy it accidentally.

### B3 — Interop tests may execute a stale subject binary

`Subject::discover_or_build` in `crates/oc-testkit/src/subject.rs:61-77` calls `discover()` first and returns any existing candidate. It builds only when no binary exists. Candidate paths include shared worktree target directories (`subject.rs:164-181`), but no source freshness, artifact identity, or audited revision is checked.

Mutation evidence:

- `hydrate_retained_history` was temporarily changed to return no history, which should make the Rust half of lifecycle interop fail with `session ... has no user message to answer`.
- The targeted interop test initially passed because it reused an existing `target/debug/opencode-rust` compiled before the mutation.
- After explicitly running `cargo build -p oc-cli --bin opencode-rust`, the same test failed with the expected no-user-message error.
- After restoring production and rebuilding, all four `session_interop` tests passed with `OC_MEMORY_GATE_MODE=skip`.

Impact: source changes to the subject can be completely absent from the executable under test while interop reports success. This is a direct false-green path for compatibility work.

Required remediation: make the harness build the subject for source-coupled tests, or verify a revision/content identity and reject stale candidates. An explicit `OC_TESTKIT_SUBJECT` may remain caller-owned, but its provenance must be visible and intentional.

### B4 — Required CI does not execute the real WASM/plugin integration suite

The entire implementation of `crates/oc-plugin/tests/integration.rs` is under `#[cfg(all(feature = "wasm", unix))]` at lines 1-2. The ordinary gate is `cargo test --workspace` (`Makefile:67-68`), invoked by Linux CI at `.github/workflows/ci.yml:68-69` and again without features on Windows at lines 141-142.

Mutation evidence:

- Default execution emitted explicit feature/platform skip tests and completed almost instantly; it did not execute the real three-tier scenarios.
- `cargo test -p oc-plugin --features wasm --test integration -- --nocapture` executed all 11 real integration tests successfully on the restored tree.
- Temporarily changing the production WASM `TextComplete` export name left the default suite green/skipped.
- The same mutation made two runaway-tier tests fail when the `wasm` feature was enabled.

Impact: required CI can merge a production WASM host regression while displaying a green test job. The suite exists but is outside the gate that claims to run workspace tests.

Required remediation: add an explicit Unix CI step for `cargo test -p oc-plugin --features wasm --test integration`, or include an appropriate all-features/features matrix in the required gate. Skip-reporting tests are useful diagnostics but are not execution of the gated behavior.

## Mutations Caught by Permanent Tests

The following mutations produced the intended committed-test failure and were restored:

| Area | Temporary production mutation | Permanent guard that failed |
|---|---|---|
| Request ownership | Invert permission session predicate | `api_reply_routes_validate_bodies_before_rejecting_cross_session_requests` |
| Request ownership | Invert question session predicate | `api_reply_routes_validate_bodies_before_rejecting_cross_session_requests` |
| Permission cleanup | Remove malformed permission claim/drop | `disconnected_permission_reply_fails_closed_without_running_the_tool` |
| Observer cleanup | Disable all observer-zero reclamation | `disconnected_only_session_observer_rejects_permission_without_running_the_tool` |
| Permission watchdog | Disable permission deadline rejection | `permission_without_an_observer_is_rejected_by_the_deadline` |
| CLI parity | Reclassify production `ConsoleCommand` from `Rejected` to `Implemented` | `every_implemented_command_has_exactly_one_parity_row` |
| Interop shape | Serialize `session.model` with `modelID` instead of `id` | reverse oracle-shape guard in `session_interop` |
| Error redaction | Stop removing the presented credential from the rendered source chain | `cmd::turn::tests::a_rejected_credential_is_scrubbed_from_the_body_that_echoed_it` |
| API-key precedence | Prefer the stored credential over `options.apiKey` | `an_options_api_key_wins_over_the_stored_credential` |
| Endpoint precedence | Prefer `baseURL` over `endpoint` | `endpoint_wins_over_base_url_when_both_are_set` |
| Compaction accounting | Estimate tokens after truncating a large tool result | `owned_compaction_transcript_charges_full_tool_output_before_truncating_it` |

These guards were mutation-sensitive rather than merely green at baseline. The CLI credential test was also confirmed through the exact unit-test path after two earlier name filters matched zero tests; zero-test commands were discarded as evidence.

## Mutations Requiring Temporary Probes or Feature-Explicit Execution

| Area | Temporary production mutation | Committed suite result | Distinguishing evidence |
|---|---|---|---|
| HTTP event durability | Live fan-out before delayed append | Green | Temporary immediate SSE-to-history route probe failed |
| Malformed question reply | Remove owned question claim/drop | Green | Temporary malformed-question fail-closed probe failed |
| Question watchdog | Disable question deadline | Green | Temporary question-deadline probe failed |
| Question observer lifecycle | Disable question observer-zero reclamation | Green | Temporary question-observer probe failed |
| Rust lifecycle interop | Drop all retained history | Initially green | Rebuilding the subject changed the same test to the expected failure |
| WASM plugin host | Break `TextComplete` export name | Default suite green/skipped | Feature-enabled integration suite failed two tests |

Every temporary probe was deleted after it demonstrated sensitivity. Every production mutation was reversed before restored-state validation.

## Additional Review Results

- `describe_turn_failure` source-chain rendering and exact credential scrubbing are protected at unit and real-process layers. The redaction mutation failed precisely and the restored tests passed.
- `provider.<id>.options.apiKey` correctly wins over the auth-store fallback, including on the wire. The inverse mutation was caught.
- Endpoint order remains `options.endpoint` → `options.baseURL` → catalog `api.url`; the inverse mutation was caught by a dead-endpoint route fixture.
- Todo 123's compaction path estimates the complete provider-visible tool result before reducing it to the summary-safe 2,000-character representation. Reversing that order was caught with a large-result fixture.
- The retained-history optimized loader has committed byte-equivalence/fallback tests. The issue found in this audit was not its restored behavior, but the interop harness's stale executable selection.
- The CLI implemented-command parity roster rejected a newly implemented production member without a parity row.

## Restored-State Validation

After all mutations were reversed, one validation chain passed:

```text
cargo test -p oc-cli --lib                                      76 passed
cargo test -p oc-cli --test provider_endpoint                    8 passed
cargo test -p oc-cli --test provider_options                     7 passed
cargo test -p oc-cli --test cli_parity                           9 passed
cargo test -p oc-cli --test session_mutation                     8 passed
cargo test -p oc-engine --lib                                   38 passed
cargo test -p oc-server --test api                              36 passed
cargo test -p oc-server --test events                           12 passed
OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test session_interop
                                                                  4 passed
cargo test -p oc-plugin --features wasm --test integration      11 passed
cargo build --workspace                                           passed
```

The long memory and soak gates were intentionally not run. No result in this report claims they were re-measured.

## Restoration Record

- Restored: `crates/oc-server/src/events.rs`
- Restored: `crates/oc-server/src/request_broker.rs`
- Restored: `crates/oc-server/src/api/request.rs`
- Restored: `crates/oc-cli/src/disposition.rs`
- Restored: `crates/oc-cli/src/cmd/turn.rs`
- Restored: `crates/oc-db/src/session.rs`
- Restored: `crates/oc-engine/src/loop.rs`
- Restored: `crates/oc-engine/src/prelude.rs`
- Restored: `crates/oc-plugin/src/wasm.rs`
- Deleted: all temporary probe tests
- No frozen evidence, benchmark baseline, lock file, dependency, or production behavior remains changed by this audit.

F2 VERDICT: REJECT
