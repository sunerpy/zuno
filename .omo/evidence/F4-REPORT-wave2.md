# F4 Scope Fidelity Review — Final Verification Wave 2

## Verdict: REJECT

The remediation wave closed the declared-divergence and workspace-roster defects and implemented the two missing SSE routes. It did not, however, close the frozen behavioral-compatibility contract for the `/api` surface or reconcile the C8 deletion contract. The blockers below are direct mismatches with `.omo/plans/opencode-rust.md`, not requests for additional scope.

## Blockers

### 1. The `/api` behavior matrix exempts 53 of 58 upstream operations instead of comparing them

Success criterion 4 requires every upstream path+method to exist **and behave**, verified by a per-operation matrix comparing status, normalized body, and observable side effects against the real binary (`.omo/plans/opencode-rust.md:1204`). Todo 118 makes the remediation requirement even more explicit: the matrix must assert status and normalized body for **every** upstream operation (`:1136-1141`).

The new inventory and SSE work are real: all 58 operations are registered, `GET /api/event` and `GET /api/session/{sessionID}/event` are served, and a `501` is rejected. But the matrix defines only five operations as `Compared` (`crates/oc-testkit/tests/compat_suite.rs:852-899`). All three dimensions of the other 53 operations are `Exempt`; the test pins that result as 15 compared dimensions and 159 exempt dimensions (`:1312-1370`). The live oracle test therefore compares only health, session list, active sessions, and the two SSE routes (`:1404-1429`).

This is not parity accounting. Forty-five subject operations return `503 backend_unavailable`, and eight backed operations are also exempt because the harness lacks deterministic fixtures. The repository itself correctly calls the 45 operations “compatibility gaps” (`README.md:94-97`; `.omo/evidence/task-118-opencode-rust.txt:79-93`). Invoking both processes without comparing their observations does not satisfy the frozen differential criterion, and replacing `501` stubs with operation-specific `503` gaps does not make those operations behave like upstream.

**Required resolution:** implement the missing local backends or explicitly obtain approval to narrow the frozen compatibility scope. Build deterministic shared fixtures for every backed operation and make every non-divergent row compare oracle and subject status, normalized body, and observable side effect. An intentional behavioral difference must be declared in `docs/divergences.toml`; a missing backend or missing fixture must not be converted into a blanket matrix exemption.

### 2. The C8 deletion implementation still covers 10 related tables while the frozen contract requires 12

Success criterion 13 requires a confirmed delete to remove the selected subtree with zero orphaned rows in all twelve related tables (`.omo/plans/opencode-rust.md:1217-1220`). The implementation still defines `PRUNE_TABLES` and `DELETE_ORDER` as ten tables (`crates/oc-db/src/prune.rs:14-32`). Its regression test explicitly asserts that “the plan's 12-table count is stale” (`crates/oc-db/tests/prune.rs:542-568`), but no approved contract amendment or remediation todo reconciles that assertion with the frozen criterion.

This is the same unresolved blocker reported in the first F4 wave. An implementation-side test cannot unilaterally rewrite an accepted scope contract, even if ten is ultimately the correct schema-derived number.

**Required resolution:** derive and document the authoritative related-table set from the pinned schema, then either cover the agreed twelve tables or explicitly amend the plan and acceptance criterion to the proven set. The preview, delete order, and orphan checks must all be closed over that same authoritative set.

## Closed Findings from F4 Wave 1

1. **Missing SSE operations — closed.** Both upstream SSE routes are implemented and behavior-tested. The remaining API blocker is the breadth of the differential, not route reachability.
2. **Undeclared behavioral differences — closed.** `docs/divergences.toml` now contains twelve entries, the six nominations are declared or merged, `DECLARED_COUNT` is twelve, and mutation evidence shows an undeclared nomination fails the compatibility suite (`.omo/evidence/task-119-opencode-rust.txt:65-183`).
3. **Workspace roster drift — closed.** The plan and `crates.expected` now name all 36 members, and the bidirectional `cargo metadata` gate rejects undeclared additions (`.omo/evidence/task-119-opencode-rust.txt:185-277`).

## Explicit Scope Checks

- **No first-party `unsafe`: satisfied.** Workspace lints forbid unsafe code, all 36 crates inherit workspace lints, and the release-surface test scans first-party source.
- **Rust-authorable plugin path: satisfied.** `examples/rust_plugin.rs` registers one tool and three hooks, can run its reusable `ConformanceSuite` directly, and the JSON-RPC integration covers execution, timeout isolation, startup failure isolation, and configured dispatch order.
- **No shipping model-id literal in `oc-agent`: satisfied.** The crate-wide source scanner covers shipping Rust source, verifies every excluded `tests.rs` is test-gated, has anti-vacuity floors, and includes planted positive controls (`crates/oc-agent/src/model_policy/tests.rs:903-1026`).
- **Goal behavior: satisfied for the reviewed contract.** Tests cover one authoritative three-tool set, system-owned status rejection, three matching blocker turns, progress/reset behavior, exactly-once guarded idle continuation, terminal-error blocking, SQL regeneration after compaction, and Markdown objective adoption/status-edit rejection.
- **G1/G2 remediation: satisfied by committed evidence.** The unchanged frozen gate reports G1 and G2 PASS; all five W-real repetitions are below the ceiling, and the median margin exceeds the observed spread (`.omo/evidence/task-123-opencode-rust.txt:67-108`). Per instruction, this review did not rerun the approximately 100-minute memory gate or the two-hour soak.
- **Process containment remediation: accepted with a platform limitation.** Linux PTY foreground behavior and clean/`SIGKILL` multi-host reaping pass. The native Windows descendant test is present but was not executed on this Linux host; the evidence states that limitation rather than claiming a pass (`.omo/evidence/task-121-opencode-rust.txt:78-88`).

## Scope-Creep Assessment

- The 36-crate roster is now an explicit, guarded amendment rather than silent expansion. `oc-process` and `oc-reaping-fixture` implement required G6 containment evidence.
- The twelve declared divergences, C8 maintenance endpoints, slim agents, durable goals, and cross-session resident memory are approved additive scope, not accidental product expansion.
- No prohibited hosted service, billing/share control plane, bundled Node/Bun runtime, OpenSSL default, or first-party unsafe implementation was found.
- The 45 explicit API backend gaps are not scope creep; they are incomplete promised scope and are therefore blocker 1 rather than divergence entries.

## Non-Blocking Observations

1. `README.md:128-140` still reports the pre-remediation G2 measurement (`1,494,236 KiB`, margin `19,260 KiB`) rather than todo 123's final measurement (`1,494,024 KiB`, margin `19,472 KiB`). Both pass with ratio `0.4936`, but the public evidence summary should be regenerated from the latest artifact.
2. Archive restoration exists as `PruneRequest::restore_archive`, but `docs/session-retention.md:14-23` states that neither CLI nor HTTP can clear the archive marker. This makes the documented “reversible” operator story library-only; expose a user-facing restore operation or narrow that wording.
3. Success criterion 6 still mentions `client.middlewareStack.add` (`.omo/plans/opencode-rust.md:1208`), while the plan's corrected C5 contract explains that `middlewareStack` belongs to the Kiro plugin's AWS client, not `PluginInput.client` (`:58`, `:680-685`). The behavioral middleware assertion is the technically valid requirement; the stale success-criterion sentence should be corrected to avoid an impossible literal reading.

## Review Basis

This was a scope-fidelity audit of the frozen plan, source, tests, generated documentation, and committed remediation evidence. The expensive G1/G2 and soak measurements were assessed from their committed artifacts as instructed. No source, test, plan, commit, branch, or remote state was modified; this report is the sole deliverable.

F4 VERDICT: REJECT
