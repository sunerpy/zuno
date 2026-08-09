# F1 Plan Compliance Audit — Final Round

## Verdict

**REJECT**

The remediation work materially improved the artifact: the compatibility oracle is now pinned and verified against installed `opencode` 1.18.15, 44 of 58 upstream API operations now have local backends, the production CLI dispatcher is mutation-covered, the ten-table prune contract was formally amended, and the ordinary Rust gates are green. The frozen plan still does not succeed as written. Eight success criteria remain false, and the Windows half of G6 remains unverified.

Result: **SATISFIED 9 / NOT SATISFIED 8 / UNVERIFIABLE 1**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `8628937ab3ee79b8208a6b5610837cc26ac93ce2`
- This audit is read-only except for this report. No source, test, plan, evidence, documentation, commit, branch, or remote was modified.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so the frozen plan, tracked evidence, current source, tests, and generated compatibility records were inspected directly.
- The approximately 100-minute G1/G2 gate and two-hour G3/G4 soak were not rerun. Their committed records were audited, as required.

## Checked-todo ledger

The plan contains **133 checked implementation-todo lines** but **130 unique todo ids**. Todos **124, 125, and 129** each appear twice. All 130 unique ids have substantive implementation commits; duplicate lines point to the same implementation. This establishes mechanical traceability, not success-criterion compliance: several checked todos deliver narrower contracts than the final criteria require.

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | Todo 130 moved the executable oracle to discovered `opencode` **1.18.15**. `crates/oc-testkit/src/oracle.rs:79,122-136,310-318,424-429` binds `PINNED_RELEASE` to the binary's actual `--version`; the committed OpenAPI capture is byte-checked against that release; the live journal round-trip preserves all 38 migration rows. The plan expressly amended this criterion to the latest installed release. |
| 2 | **NOT SATISFIED** | The real-user config differential explicitly sets `OPENCODE_PURE=1` at `crates/oc-config/tests/differential.rs:295-303`. That excludes the real JS plugins and therefore cannot prove byte-identical full plugin-expanded skill/agent trees for `/config/.config/opencode/opencode.json`. The parse seam is fixed, but the required non-pure comparison is absent. |
| 3 | **NOT SATISFIED** | Every upstream command has a disposition, and todo 124 now proves every implemented production dispatcher arm reaches a non-pending handler (`crates/oc-cli/tests/surface.rs:152,422-432`). The probes assert command-specific handler fragments, not normalized output equality with the real binary for every implemented command. Selected differential command families do not satisfy the universal criterion. |
| 4 | **NOT SATISFIED** | The matrix invokes all 58 upstream operations and rejects both `501` and `503` as parity, but only **44** have local backends. The remaining **14** are deliberately routed to `BackendUnavailable` at `crates/oc-server/src/api/mod.rs:144-193`; `crates/oc-testkit/tests/compat_suite.rs:2677-2683` records them as a compatibility gap. A complete path inventory with honest gap reporting is not “every path+method ... behaves.” |
| 5 | **NOT SATISFIED** | Current tests cover Rust-written session decoding/listing by TypeScript, import/export, and internal child-task continuation. No current end-to-end test takes the same existing session through **list, open, continue, and export in both TS→Rust and Rust→TS directions**, as the criterion requires. |
| 6 | **NOT SATISFIED** | The exact plugin contract is not implemented: `SUPPORTED_JS_PLUGINS` pins Kiro auth **0.20.1**, not required **0.18.0** (`crates/oc-plugin/src/js/loader.rs:23-31`), and the real-plugin test uses 0.20.1. No assertion proves `client.middlewareStack.add` followed by provider visibility in `models --format json`. |
| 7 | **NOT SATISFIED** | Rust, WASM, and JS tiers coexist and are fault-isolated, but the three-tier test uses the synthetic `integration-js` fixture (`crates/oc-plugin/tests/integration.rs:23-38`), not the two real auth plugins named by criterion 6. It therefore does not prove the required real-plugin coexistence contract. |
| 8 | **SATISFIED** | Workspace policy forbids unsafe first-party code, a source-policy test scans `crates/*/src/**`, and all-target Clippy passed. |
| 9 | **SATISFIED** | The example Rust JSON-RPC plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Built-in-agent tests pin negative delegation boundaries, temperature, deny-by-default permissions, output envelopes, model inheritance/overrides, and all required `task` fields. Source policy rejects model-id literals in `oc-agent`. |
| 11 | **NOT SATISFIED** | Goal status ownership, counters, idle continuation, and Markdown projection have focused tests. The compaction regression at `crates/oc-goal/src/continuation_tests.rs:42-78` demonstrates regeneration after **one** compaction. It does not drive two consecutive compactions while proving objective and counters remain intact. |
| 12 | **SATISFIED** | `session list --all-projects` is tested against the real global/experimental endpoint on the same database, including project summaries and the session set (`crates/oc-cli/tests/differential.rs:804-863`). |
| 13 | **SATISFIED** | The plan owner amended the stale twelve-table wording to the schema-derived **ten** tables. `PRUNE_TABLES` and `DELETE_ORDER` both pin ten (`crates/oc-db/src/prune.rs:15-32`), and preview, confirmation, descendant closure, exclusions, liveness, transactionality, and orphan checks are covered. |
| 14 | **SATISFIED** | Artifact GC retains referenced snapshot stores and removes unreferenced ones. Explicit vacuum reports reclaimed bytes, checks integrity, and refuses insufficient free space. |
| 15 | **UNVERIFIABLE** | Committed evidence establishes G1 PASS (`20,380 KiB / 477,120 KiB`), G2 PASS (`1,494,024 KiB / 1,513,496 KiB`, all five runs below the ceiling, 19,472 KiB margin versus 17,032 KiB spread), the 500-turn/7,200-second G3/G4 PASS, G5 bounded-channel behavior, and Linux G6 containment. The Windows Job Object test exists only behind `#![cfg(windows)]` (`crates/oc-process/tests/windows_containment.rs:1-31`) and was explicitly not executed on native Windows. This Linux audit cannot establish that all cross-platform G1–G6 requirements pass. |
| 16 | **SATISFIED** | MCP is exercised against real servers, LSP against `rust-analyzer` and `typescript-language-server`, ACP against the real SDK client, and providers against recorded real-traffic cassettes across wire families. |
| 17 | **SATISFIED** | `docs/divergences.toml` contains exactly 12 non-empty declarations with reasons; `crates/oc-testkit/src/divergence.rs:50` pins `DECLARED_COUNT = 12`; the compatibility gate resolves recorded behavioral differences to declarations and checks the live `execute` schema contract. |
| 18 | **NOT SATISFIED** | F1 is this **REJECT**. The tracked F2, F3, and F4 reports still end in **REJECT**, the final-wave F1–F4 checkboxes remain unchecked, and no explicit user acceptance is recorded. |

## Blocking findings and closure conditions

### B1 — The actual non-pure user configuration is not compared

- **Location:** `crates/oc-config/tests/differential.rs:295-303`
- **Failure:** the case named `real-user-global-config` forces `OPENCODE_PURE=1`, excluding exactly the plugins and generated trees criterion 2 requires.
- **Close when:** a deterministic test runs both binaries on the actual configuration without pure mode and proves the normalized merged config plus complete skill/agent trees are byte-identical, with any intentional normalization explicitly declared.

### B2 — Fourteen upstream API operations still have no backend

- **Location:** `crates/oc-server/src/api/mod.rs:144-193`; inventory at `crates/oc-testkit/tests/compat_suite.rs:2677-2683`.
- **Failure:** 14 of 58 operations return operation-specific `503 backend_unavailable`. The matrix correctly refuses to call this parity.
- **Close when:** implement those 14 operations and compare status, normalized body, and observable side effects against the pinned oracle, or formally amend criterion 4's universal compatibility claim.

### B3 — Universal CLI output parity is not proved

- **Location:** `crates/oc-cli/tests/surface.rs:152,422-432` and the selected cases in `crates/oc-cli/tests/differential.rs`.
- **Failure:** production routing is now strongly mutation-covered, but routing to a real handler does not establish normalized output equality for every implemented command.
- **Close when:** execute every implemented command in a deterministic oracle/subject matrix and compare exit status and normalized stdout/stderr, retaining the production-dispatch mutation guard.

### B4 — Bidirectional same-session continuation is absent

- **Location:** current CLI/session compatibility tests; no single lifecycle test covers the required sequence.
- **Failure:** partial interoperability tests do not prove one existing session can be listed, opened, continued, and exported in each direction.
- **Close when:** add TS→Rust and Rust→TS end-to-end tests over the same persisted session and assert both transcript growth and export readability.

### B5 — The exact real JS plugin contract is not met

- **Location:** `crates/oc-plugin/src/js/loader.rs:23-31`, `crates/oc-plugin/tests/js.rs:286-300`, and `crates/oc-plugin/tests/integration.rs:23-38`.
- **Failure:** Kiro is pinned/tested at 0.20.1 instead of 0.18.0; required middleware-stack/provider-list behavior is not asserted; tier integration substitutes a synthetic JS fixture.
- **Close when:** support the plan's exact versions (or amend the criterion), prove `client.middlewareStack.add`, prove provider visibility through `models --format json`, and run both real auth plugins alongside Rust and WASM tiers with independent-failure tests.

### B6 — Goal survival is tested across one compaction, not two

- **Location:** `crates/oc-goal/src/continuation_tests.rs:42-78`.
- **Failure:** the test selects one boundary, discards one old injection, and regenerates once; it never performs a second compaction or checks counters across both.
- **Close when:** an end-to-end test forces two consecutive compactions and asserts the SQL objective, counters, and next-turn injection after each.

### B7 — Native Windows G6 evidence is missing

- **Location:** `crates/oc-process/tests/windows_containment.rs:1-31`; tracked todo 121/126 evidence.
- **Failure:** implementation and a platform-gated test exist, but no native-Windows execution result is retained.
- **Close when:** run the containment suite on native Windows, retain the output, and show zero live descendants after the required clean and abnormal parent-termination cases.

### B8 — Final approvals and user acceptance do not exist

- **Location:** `.omo/evidence/F2-REPORT-wave2.md`, `F3-REPORT-wave2.md`, `F4-REPORT-wave2.md`; `.omo/plans/opencode-rust.md:1249-1254`.
- **Failure:** all tracked peer reports reject, final verification is unchecked, and the user has not accepted four approving reports.
- **Close when:** remediate the substantive blockers, rerun F1–F4 against one final HEAD, obtain four approvals, surface them, and receive explicit user acceptance.

## Verification performed

The following required chain was run once on the audited tree and exited 0:

```text
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline
cargo fmt --all --check
cargo metadata --locked --offline --format-version 1
```

Results: all non-ignored unit, integration, and doctest targets passed; Clippy completed without diagnostics; formatting was clean; locked offline metadata resolved. These ordinary gates do **not** rerun the opt-in G1/G2 measurement or ignored two-hour soak, and they do not convert explicit compatibility gaps into parity.

## Final decision

The codebase is build-clean and substantially closer to the target than in the prior rounds, but the frozen success contract remains unmet. In particular, non-pure user-config parity, complete API behavior, universal CLI output parity, bidirectional same-session continuation, exact real-plugin compatibility, and two-compaction goal survival are absent; native Windows containment remains unverified; final peer approvals and user acceptance do not exist. F1 must reject.

F1 VERDICT: REJECT
