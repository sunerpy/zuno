# F1 Plan Compliance Audit — Fourth Final Verification Wave

## Verdict

**REJECT**

The fourth remediation wave closes the pure-mode config scope, the two-compaction goal regression, the Linux/Windows G6 disclosure contract, and the HTTP permission/question broker. It does not make the frozen plan internally consistent or satisfy every remaining universal claim. Success criterion 4 still says fourteen frozen API gaps while the implementation and todo 132 now freeze ten; criterion 6 still requires a real request proving both injected header and effort fields, while the executable test explicitly does not assert effort. Universal CLI output parity, bidirectional same-session interoperability, real-plugin three-tier coexistence, and final approvals also remain absent.

Result: **SATISFIED 12 / NOT SATISFIED 6 / UNVERIFIABLE 0**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `55612823e3da7f025e174ccb28fa3c8a86c17fb1`
- The audit was read-only except for this report. No source, test, plan, evidence, documentation, commit, branch, or remote was modified.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so the frozen plan, current source, tests, Git history, and tracked evidence were inspected directly.
- The approximately 100-minute G1/G2 gate and two-hour G3/G4 soak were not rerun. Their committed artifacts were audited as instructed.
- The owner-approved corrections and narrowings were accepted only where the final plan text, executable assertion, and measured evidence agree.

## Checked-todo ledger

The plan contains **136 checked implementation-todo lines** representing **133 unique todo ids**, contiguous from 1 through 133. Todos **124, 125, and 129** each appear twice. Every unique id maps to a substantive implementation commit in the audited ancestry; the duplicate lines map to the same implementation as their first occurrence.

The planned evidence paths for todos **52, 60, 101, 109, 110, 111, 113, and 114** are absent, but each has a substantive implementation commit, executable tests, and later cross-evidence. This is evidence-path drift rather than an empty checkmark. Mechanical todo traceability does not establish success-criterion compliance: six final claims remain false or internally inconsistent.

## Corrections and narrowing audit

| Contract change | Decision |
|---|---|
| Criterion 1: use the latest installed released `opencode` | **Accepted.** The oracle resolves installed 1.18.15, requires its reported version to equal `PINNED_RELEASE`, byte-checks the capture, and runs the journal round-trip against that binary. |
| Criterion 2: pure-mode config parity only | **Accepted.** The real-user matrix forces `OPENCODE_PURE=1`; the excluded non-pure `agent` and `command` trees are declared with measured sizes; mutations removing the mode or a size fail. |
| Criterion 4: freeze explicit `503 backend_unavailable` operations by name | **Not converged.** The executable set correctly contains ten operations after todo 132, but the final criterion and todo 133 prose still call fourteen current gaps frozen, and the compatibility report detail still describes 44 backed/14 gapped operations. |
| Criterion 6: converge on Kiro auth 0.20.6 and remove `client.middlewareStack.add` | **Partially accepted, criterion still unmet.** The version/mechanism correction is legitimate and pinned. The replacement contract still says a real request proves injected header **and effort fields**; the test explicitly says effort is not asserted. |
| Criterion 13: ten session-attributable tables | **Accepted.** The schema has ten such tables, and both `PRUNE_TABLES` and `DELETE_ORDER` pin the corrected complete set. |
| Criterion 15: execute Linux G6 and disclose native Windows as `NOT EXECUTED` | **Accepted.** Linux containment runs; the Windows implementation and `cfg(windows)` test exist; README and tracked evidence are test-pinned to state `NOT EXECUTED`. |

## Todo 133 falsifiability review

- **Criterion 2:** the named test fails if pure mode is removed and if either measured non-pure tree size is removed. It also forces the declaration to be revisited if local non-pure tree generation appears.
- **Criterion 4:** the named live-server test compares the observed `503` operation set to ten named operations. It fails for a newly appearing gap, for a departed member not removed from the set, and for a closed gap that has not gained compared status/body dimensions. The pin is sound, but it pins a different set from the fourteen-operation wording still present in the plan and generated report detail.
- **Criterion 6:** the convergence test derives the version from `SUPPORTED_JS_PLUGINS`, checks the plan, capture, user config, and installed manifest, and rejects a second named version. The real Kiro hook test positively and negatively pins `x-opencode-kiro-request-kind`. It does not prove the criterion's effort-field clause and therefore does not fully pin the narrowed replacement contract.
- **Criterion 15:** mutations removing the README disclosure or changing the source platform gate fail the named docs test; both README and evidence must name `windows_containment.rs` and `NOT EXECUTED`.
- **Criterion 11 and PTY expiry:** the new goal test selects two real compaction boundaries and preserves objective/counters from SQL; the repaired route test fails when expiry pruning is removed after proving the ticket scope matches. These close the two non-narrowing defects assigned to todo 133.

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | The executable oracle is pinned to installed `opencode` 1.18.15, the capture agrees, and the journal round-trip preserves all 38 migration rows while the max-known-migration refusal remains explicit. |
| 2 | **SATISFIED** | The real-user `debug config` comparison runs in required pure mode. The excluded non-pure plugin trees are declared as `non-pure-plugin-generated-trees` with 221,818-byte `agent` and 17,970-byte `command` measurements, and the declaration/count gate now pins 13 entries. |
| 3 | **NOT SATISFIED** | All 23 upstream commands have one disposition and implemented dispatch arms are mutation-covered, but only selected command families receive oracle output comparison. No matrix executes every implemented command and compares normalized exit status/stdout/stderr. |
| 4 | **NOT SATISFIED** | The live matrix accounts for all 58 operations and the current implementation has 48 backed operations plus ten named `503` gaps. The final criterion still freezes “the fourteen current gaps,” and the emitted compatibility-report detail still states 44 backed/14 gapped. A test pinning ten cannot satisfy a contract that still names fourteen. |
| 5 | **NOT SATISFIED** | Partial interoperability exists, including Rust-written sessions readable by TypeScript and HTTP session continuation. No end-to-end pair takes the same existing session through list, open, continue, and export in both TS→Rust and Rust→TS directions. |
| 6 | **NOT SATISFIED** | Both real auth packages load on this host and expose provider records; Kiro auth is consistently pinned to 0.20.6; the real `chat.headers` hook injects the compaction request-kind header. However, the criterion's replacement requires header and effort fields, while `js_real_kiro_plugin_injects_its_request_kind_header_for_a_compaction_turn` explicitly states effort is not asserted. Provider visibility is also checked at plugin-load level rather than through the required `models --format json` user surface. |
| 7 | **NOT SATISFIED** | Rust, WASM, and JS tiers preserve order and isolate failures, but `three_tiers_follow_configuration_order` still uses the synthetic `integration-js` fixture rather than the real Antigravity and Kiro auth plugins required by criterion 6. |
| 8 | **SATISFIED** | Workspace policy forbids first-party unsafe code, source-policy tests scan first-party crate sources, and all-target Clippy completed without diagnostics. |
| 9 | **SATISFIED** | The example Rust JSON-RPC plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent tests pin negative delegation boundaries, temperature, deny-by-default permissions, output envelopes, model inheritance/overrides, absence of model-id literals, and the required `task` selection/continuation fields. |
| 11 | **SATISFIED** | The new regression performs two consecutive compactions, proves each discards the goal injection, and then verifies the objective, 2,200-token/50-second counters, active status, persistent failure streak, and a third SQL-derived injection. Existing tests cover idle guards, status ownership, and Markdown projection. |
| 12 | **SATISFIED** | `session list --all-projects` is compared with the real global endpoint on the same database, including project summaries and the session set. |
| 13 | **SATISFIED** | Preview is non-mutating without confirmation; subtree deletion, exclusions, liveness, transactionality, and orphan checks cover the corrected ten-table schema-derived set. |
| 14 | **SATISFIED** | Artifact GC retains referenced snapshot stores and removes unreferenced stores. Explicit vacuum reports reclaimed bytes, checks integrity, and refuses insufficient free space. |
| 15 | **SATISFIED** | Tracked evidence establishes G1 and G2 under frozen formulas, the 500-turn/7,200-second G3/G4 soak, G5 channel policies, and Linux G6 clean/`SIGKILL`/interactive PTY/Ctrl-C containment. Under the approved narrowing, the native Windows half is implemented and explicitly test-pinned as `NOT EXECUTED` in README and evidence. |
| 16 | **SATISFIED** | MCP is exercised against real servers, LSP against `rust-analyzer` and `typescript-language-server`, ACP against the real SDK client, and provider wire families against recorded real traffic. |
| 17 | **SATISFIED** | `docs/divergences.toml` contains 13 counted declarations with reasons; generated docs and behavioral-difference records resolve through the single allow-list; the non-pure plugin-tree exclusion is included rather than silently absorbed. |
| 18 | **NOT SATISFIED** | F1 is this **REJECT**. The latest tracked F2, F3, and F4 reports also end in **REJECT**, final-wave checkboxes remain unchecked, and no explicit user acceptance exists. |

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from tracked evidence:** Rust W-idle median 20,380 KiB versus frozen ceiling 477,120 KiB. |
| G2 | **PASS from tracked evidence:** Rust W-real median 1,494,024 KiB versus ceiling 1,513,496 KiB; all five runs pass; 19,472 KiB margin exceeds the 17,032 KiB spread. |
| G3 | **PASS from tracked evidence:** 500 turns over 7,200 seconds; Theil–Sen slope 0.0001775568 MiB/turn and peak ratio 0.9938255268 remain below frozen limits. |
| G4 | **PASS from tracked evidence:** no 120-second progress-watchdog or 1,800-second hard-deadline trip. |
| G5 | **PASS from tracked evidence and ordinary tests:** bounded channels have declared policies; the production turn-event mutation is caught after todo 120. |
| G6 | **PASS under the approved narrowed criterion:** Linux clean shutdown, parent `SIGKILL`, PTY read, and Ctrl-C tests pass; Windows Job Object source/test exists and is explicitly `NOT EXECUTED` on this Linux host. |

## Blocking findings and closure conditions

1. **Universal CLI parity is still not proved.** Execute every implemented command against subject and oracle, then compare normalized exit status, stdout, and stderr while retaining the production-dispatch mutation guard.
2. **Criterion 4 has two incompatible current inventories.** Amend the final criterion and todo 133 prose from fourteen to ten, regenerate the compatibility report's stale 44/14 detail to 48/10, and keep the live named-set test as the authority.
3. **Bidirectional same-session interoperability is incomplete.** Add TS→Rust and Rust→TS lifecycle tests over the same persisted session covering list, open, continue, transcript growth, export, and decoding by the opposite implementation.
4. **Criterion 6's replacement contract is not fully demonstrated.** Either execute a credential-safe real request that proves effort reaches Kiro's internal AWS request and verify provider visibility through `models --format json`, or obtain an explicit owner amendment removing those clauses. Do not leave the test saying it deliberately omits a field the criterion says it proves.
5. **Three-tier coexistence does not use the required real JS plugins.** Run the Rust and WASM examples alongside both real auth plugins in configuration order and prove killing any tier degrades only that tier.
6. **Final acceptance is absent.** Re-run F1–F4 against one common final HEAD, obtain four approvals, surface them, and wait for explicit user acceptance.

## Ordinary validation performed

- `cargo test --workspace --offline` was attempted twice. The first run stopped while listing `oc-provider-compatible --test rules`; the serialized retry (`CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 ... --test-threads=1`) stopped while listing `oc-provider-compatible --lib`. Both failures were the known host-level `EAGAIN: Resource temporarily unavailable`; every test binary that started before each interruption passed. Per the two-attempt limit, the suite was not retried again. The full workspace test command therefore did **not** complete and is not reported as passing.
- `cargo clippy --workspace --all-targets --offline` — **PASS**, zero diagnostics.
- `cargo fmt --all --check` — **PASS**.
- `cargo metadata --locked --offline --format-version 1` — **PASS**.
- The opt-in memory gate and ignored two-hour soak were not rerun.

## Final decision

The implementation is substantially improved and most owner-approved corrections are now honest, executable contracts. Approval is still impossible because criteria 4 and 6 disagree with their own executable evidence, and criteria 3, 5, 7, and 18 remain unmet. The ordinary full workspace test command also could not complete after two host-resource failures, although no started test failed and the other required gates passed.

F1 VERDICT: REJECT
