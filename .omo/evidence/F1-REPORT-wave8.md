# F1 Plan Compliance Audit — Eighth Final Verification Wave

## Verdict

**REJECT**

Audited HEAD `2e57e490c84224f44ff3ba8469cf9dd8dfa1b9e8` passes the complete Rust workspace gate and closes the wave-7 implementation findings through todos 145–149. It cannot be approved because success criteria 2, 3, and 18 are not satisfied:

1. criterion 2's test no longer exercises the user's **actual** `/config/.config/opencode/opencode.json`; the committed copy has drifted, and a direct current pure-mode comparison is not byte-identical after structural normalization;
2. checked todo 13 still requires nonexistent `opencode agent list --format json`; that command exits 1 with help text rather than JSON, while the real plain `agent list` output also differs materially from this port on the current setup;
3. F2, F3, and F4 have not produced wave-8 reports for this HEAD, and explicit user acceptance therefore does not exist.

Result: **SATISFIED 15 / NOT SATISFIED 3 / UNVERIFIABLE 0**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `2e57e490c84224f44ff3ba8469cf9dd8dfa1b9e8`
- The worktree was clean before this audit. This report is the only intended retained modification; no product source, test, plan, documentation, commit, branch, or remote was changed.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so the plan, current source, tests, executable surfaces, Git state, and tracked evidence were inspected directly.
- The approximately 100-minute G1/G2 measurement and two-hour G3/G4 soak were not rerun. Their retained measurements, frozen methodology, executable guards, and mutation sensitivity were audited instead.
- No config values or other potentially sensitive live data are reproduced in this report; only byte counts, hashes, key names, and equality results are recorded.

## Checked-todo ledger

The current plan contains **149 checked implementation rows**, representing **149 unique ids**, contiguous from 1 through 149. There are no duplicate ids or gaps. F1–F4 remain unchecked final-verification rows.

Mechanical continuity is not sufficient. A checked row whose acceptance command does not exist is not executable evidence, and criterion 18 separately requires all four reviewers to approve the same HEAD plus explicit user acceptance.

## Wave-8 remediation disposition

| Todo | Decision |
|---:|---|
| 145 | **Satisfied.** Todos 26 and 60 now use the real `models` / `models --verbose` surfaces and explicitly preserve the history that `models --format json` was invalid. |
| 146 | **Satisfied.** Differential suites route through `Oracle::discover_pinned`; `no_pinned_oracle_paths.rs` guards against hard-coded bypasses. |
| 147 | **Satisfied.** A provider payload truncated by the bounded JS encoder is rejected with `BridgeError::TruncatedProvider` and a JSON Pointer path rather than silently written back. |
| 148 | **Satisfied.** Production registers all eight declared wire-family factories and tests representative selection/decoding; the divergence now covers unknown transports rather than implemented families. |
| 149 | **Satisfied.** `HookName::ALL` maps exhaustively to production triggers, all 21 advertised hooks have dispatch paths, and generated plugin documentation derives from that support matrix. |

## Newly confirmed blocking findings

### 1. Criterion 2 does not currently test or match the actual user config

`crates/oc-config/tests/differential.rs:298-313` calls its fixture “the live file, byte-for-byte”, but `real_user_config()` reads the committed `tests/fixtures/user-config.json`, not `/config/.config/opencode/opencode.json` and not a drift-checked capture.

Current hashes prove the copy is stale:

- live config: 24,417 bytes, SHA-256 `502ca4db55e63d958be28bb7ed9b2d687a9a6f2eca84442df37dd8e7245336c6`;
- committed fixture: 25,361 bytes, SHA-256 `33c8e02fff4549853e5354ebc8745eb548f9b5f5ff35352c9befa401ac7a4137`.

The fixture-based suite remains green: `cargo test -p oc-config --test differential --offline` passed 4/4 and reported all 15 fixture trees identical. That proves the loader against the committed capture, but not criterion 2's explicitly named current path.

A direct current comparison was captured to files to avoid the tool's 64 KiB inline-output limit:

```text
OPENCODE_PURE=1 opencode debug config
  266,233 bytes; valid JSON
OPENCODE_PURE=1 target/debug/opencode-rust debug config
   25,581 bytes; valid JSON
```

After parsing and deterministic JSON key sorting, the outputs are unequal. Removing the oracle's diagnostic `mode` field does not close the difference: `agent` and `command` differ, and the oracle additionally reports `plugin_origins`. This directly contradicts the required byte-identical actual-config result and the narrowing's premise that pure mode removes the excluded generated trees.

### 2. Todo 13 and criterion 3 rely on an invalid surface

The checked acceptance criterion at `.omo/plans/opencode-rust.md:270` says the differential asserts equality with `opencode agent list --format json`. Released 1.18.15 exposes no `--format` option for this command:

```text
opencode agent list --format json
exit: 1
stdout: 0 bytes
stderr: 561 bytes of command help
JSON parse: failed
```

This is the same defect class corrected for `models` by todo 145, and todo 145's own evidence explicitly records it as adjacent unfinished work.

The valid plain surface does not provide an alternative passing proof on the current setup:

| Surface | Bytes | Lines | SHA-256 |
|---|---:|---:|---|
| released 1.18.15 `agent list` | 714,302 | 26,058 | `c210eb08c8c4b0597528991b8c1c1fa176a679c27ce82559824d41ce4facb7d9` |
| `target/debug/opencode-rust agent list` | 438,099 | 15,813 | `a647d9bc0bb84d097fd7d01d13e288dc9cc1f8c28661cfefc64322e3dc76240c` |

The first record already differs semantically (`Sisyphus - ultraworker (primary)` versus `build (primary)`), followed by different permission entries and agent trees; this is not a whitespace-only or key-order-only discrepancy. Therefore criterion 3's “every implemented CLI command” claim is not currently true, independently of the stale acceptance command.

### 3. Criterion 18 has no common approval wave

The `task-F2`, `task-F3`, and `task-F4` worktrees all point at the audited HEAD, but no `F2-REPORT-wave8.md`, `F3-REPORT-wave8.md`, or `F4-REPORT-wave8.md` exists. F1 itself is a rejection, and the user has not been presented with four approvals for an explicit okay.

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | Compatibility tests use installed release 1.18.15, preserve the migration journal across the real-binary round trip, and refuse a database above the max-known migration. |
| 2 | **NOT SATISFIED** | The green differential uses a stale committed fixture rather than the current named path. Direct current pure-mode JSON is structurally unequal, principally at `agent` and `command`; details and hashes are above. |
| 3 | **NOT SATISFIED** | Todo 13's required JSON surface does not exist and exits 1. The valid plain `agent list` outputs differ materially on the current setup. |
| 4 | **SATISFIED** | The behavior inventory freezes all 58 operations, compares the 48 backed operations, and freezes ten explicit `503 backend_unavailable` gaps. Generated report and executable inventory agree. |
| 5 | **SATISFIED** | Same-session interop covers list, open, continue, transcript growth, export, and opposite-implementation decoding in both directions. |
| 6 | **SATISFIED** | Kiro auth 0.20.6 and Antigravity load through the JS host; real plugin tests prove their provider contributions on the valid plain models surface. |
| 7 | **SATISFIED** | Rust, WASM, Kiro, and Antigravity tiers have ordered mutation and isolated-degradation coverage; todos 144 and 149 close production Auth/Tool and all-hook reachability. |
| 8 | **SATISFIED** | Workspace policy forbids first-party unsafe code and the source guard scans first-party crate sources. |
| 9 | **SATISFIED** | The Rust example plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent/task tests cover delegation boundaries, temperatures, deny-by-default permissions, envelopes, model inheritance and overrides, absence of model-id literals, category/background/continuation, and effort selection. |
| 11 | **SATISFIED** | Goal state survives two compactions; idle continuation, status ownership, and Markdown objective/status round trips are executable tests. |
| 12 | **SATISFIED** | `session list --all-projects` is compared against the real global endpoint with project summaries and matching session sets. |
| 13 | **SATISFIED** | Prune is preview-only by default, confirmation gates mutation, subtree deletion is transactional, and all ten attributable tables plus shared/compacting/active/recent liveness protections are covered. |
| 14 | **SATISFIED** | Snapshot GC deletes only unreferenced stores; explicit vacuum reports reclaimed bytes, remains separate from prune, and checks available disk. |
| 15 | **SATISFIED** | Retained measurements pass G1–G4, all bounded channels have behavior guards, Linux containment is executed, and Windows Job Object support remains explicitly implemented but NOT EXECUTED as narrowed. |
| 16 | **SATISFIED** | Tests use a real CodeGraph MCP server, two real language servers, the real ACP SDK client, and recorded real provider traffic through production decoders. |
| 17 | **SATISFIED** | Declared divergences have reasons and live witnesses; generated docs, counts, and current documentation tests agree. Unsupported provider transports remain named rather than silently routed. |
| 18 | **NOT SATISFIED** | F1 rejects; F2–F4 wave-8 reports and explicit user acceptance are absent. |

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from retained evidence:** Rust W-idle median 20,380 KiB versus the frozen 477,120 KiB ceiling. |
| G2 | **PASS from retained evidence:** Rust W-real median 1,494,024 KiB versus the 1,513,496 KiB ceiling; every measured run is below the ceiling. |
| G3 | **PASS from retained evidence:** 500 turns / 7,200 seconds, slope 0.0001775568 MiB/turn, final/middle peak ratio 0.9938255268. |
| G4 | **PASS from retained evidence:** no 120-second meaningful-progress or 1,800-second hard-deadline violation. |
| G5 | **PASS from current tests and retained evidence:** 17 persistent bounded production channels have exact declarations and behavior gates; exclusions are explicit. |
| G6 | **PASS under the narrowed criterion:** Linux clean, parent-`SIGKILL`, PTY, and Ctrl-C containment are tested; Windows remains honestly NOT EXECUTED. |

## Observable mutation

The frozen G1 formula in `docs/perf-methodology.md` was temporarily changed from `0.50` to `0.51`. The exact unit test `perf::methodology::tests::methodology_formula_section_matches_its_revision_hash` failed with observed digest `ffb85ca1affb6132486f78844a5be95333ff2d4f0a15ecd06a481e93491ac128` versus registered digest `db49ffeb3a19a265a948e5545afe14e245f8ac7c8201ae1b1e1748e87f6922ad`. The original byte was restored and `git diff --exit-code -- docs/perf-methodology.md` passed. No mutation remains.

## Validation performed

- focused config differential: **4 passed, 0 failed** across 15 trees against the committed fixture;
- complete workspace: `cargo test --workspace --offline` — **3390 passed, 0 failed, 2 ignored**;
- lint: `cargo clippy --workspace --all-targets --offline -- -D warnings` — **PASS**, zero warnings/errors;
- formatting: `cargo fmt --all --check` — **PASS**, no output.

The full workspace gate succeeded on its first attempt. The long G1/G2 measurement and G3/G4 soak were deliberately not rerun.

## Closure conditions

1. Amend todo 13 honestly, as todo 145 amended the equivalent `models` defect: preserve the invalid-command history, use the real plain `agent list` surface, and add executable fixture and current-setup evidence that detects semantic differences rather than normalizing them away.
2. Restore criterion 2's falsifiability. Either make the test consume or drift-check the named current config and full agent/skill trees, then fix the observed pure-mode `agent`/`command` difference, or obtain an explicit owner amendment to a reproducible committed capture. A stale fixture labelled “live file, byte-for-byte” is not closure.
3. After those changes, run F1–F4 against one common HEAD, obtain four approvals, surface them to the user, and receive the explicit okay required by criterion 18.

## Final decision

The implementation gate is green and todos 145–149 close their intended wave-7 findings. That does not override two directly falsified compatibility requirements or the missing final-acceptance gate. This HEAD is therefore not plan-compliant.

**F1 VERDICT: REJECT**
