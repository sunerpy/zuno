# F1 Plan Compliance Audit — Sixth Final Verification Wave

## Verdict

**REJECT**

The sixth remediation wave fixes stale-subject false greens, routes the real Kiro auth provider onto the plain `models` surface, records the assistant step-part omission as a named gap with a live witness, adds route-level persist-before-live and question fail-closed guards, runs the feature-enabled WASM suite in required CI, and strengthens the diagnostics divergence witness. Approval is still impossible at audited HEAD `b753fb950e5376f6f93e51024e3539900bc544ce`: the generated compatibility report contradicts itself by retaining the obsolete 14-gap API detail beside its derived 10-gap record; success criterion 6 still requires the nonexistent `models --format json` surface even though todo 139 correctly proves the real plain `models` surface; and F2–F4 have not all approved this common HEAD.

Result: **SATISFIED 15 / NOT SATISFIED 3 / UNVERIFIABLE 0**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `b753fb950e5376f6f93e51024e3539900bc544ce`
- The worktree was clean before this report. This report is the only file modified by F1; no product source, test, plan, evidence, documentation, commit, branch, or remote was changed.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so the frozen plan, current source, tests, Git history, generated report, and tracked evidence were inspected directly.
- The approximately 100-minute G1/G2 run and two-hour G3/G4 soak were not rerun. Their committed evidence and current methodology guards were audited as instructed.

## Checked-todo ledger

The plan contains **145 checked implementation-todo lines** representing **142 unique todo ids**, contiguous from 1 through 142. Todos 124, 125, and 129 each appear twice. All 142 ids map to substantive implementation ancestry at this HEAD: 130 use the plan's exact expected commit subject and 12 (61, 65, 66, 74, 76, 77, 80, 81, 83, 84, 87, and 91) use semantically matching substantive commits. No empty or commit-less checked implementation claim was found.

Mechanical traceability does not override final acceptance. Two current artifacts still contradict the executable or frozen contract, and the four-review acceptance gate is incomplete.

## Fifth-wave blocker disposition

| Fifth-wave blocker | Sixth-wave disposition |
|---|---|
| API inventory report | **Partially closed, still blocking.** `known_gaps()` and the generated compatibility matrix now derive 48 backed operations and ten named gaps from the live gate. However, `SURFACES[api-operations].detail` still says `14 missing local backends`, and the freshly generated `target/compat/compat-report.json` publishes both statements. |
| Real Kiro provider visibility | **Behavior closed; plan text still blocking.** Todo 139 proves the real Kiro and Antigravity plugins contribute `kiro-auth` and `google` through plain `models`; the port exposes nine providers versus the release's ten, with only the unrelated hosted `opencode` provider absent. Both release 1.18.15 and this port correctly lack `models --format`. The final success criterion was not amended and still requires that nonexistent flag. |
| Divergence documentation count | **Closed.** README derives “seventeen” from `DivergenceList`; `docs/divergences.toml`, `DECLARED_COUNT`, generated detail/index blocks, and the docs test agree on 17 entries. The historical words “thirteen” inside the narrative describe the late todo-119 batch and thirteenth entry; they are not a current total. |
| Final acceptance | **Still open.** F2 and F4 reports are absent; the F3 report on this HEAD still says testing is in progress and its verdict is pending. |

## Sixth-wave todo disposition

| Todo | Decision |
|---:|---|
| 138 | **Satisfied.** `Subject::discover_or_build` asks Cargo to refresh the workspace subject once per test process, explicit `OC_TESTKIT_SUBJECT` remains intentional and visible in provenance, and `subject_freshness` passes both cases. The retained-history mutation failed without a manual rebuild as required. |
| 139 | **Satisfied.** The production CLI loads configured real JS plugins, applies their provider model loaders, and exposes both `google` and `kiro-auth` through plain `models` without hard-coding either id. The remaining `opencode` delta is named as the release's compiled-in hosted provider. |
| 140 | **Satisfied.** `assistant-turn-step-parts` is a named compatibility gap, not a laundered divergence. A production-path SQLite witness proves this port persists only `text`, generated docs and the machine report share one source, and mutations prove behavior or classification drift fails. |
| 141 | **Satisfied.** Route-level SSE/history ordering, three isolated question fail-closed paths, and required feature-enabled WASM CI execution are committed and passing. |
| 142 | **Satisfied.** All three declared diagnostic surfaces carry two-sided text witnesses; a mutation removing the bound address fails, and the comparison cannot shrink into exemptions. |

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | The compatibility suite uses released `opencode` 1.18.15, preserves all 38 migration rows through the journal round-trip, and enforces the max-known-migration refusal. Same-session lifecycle tests additionally prove list/open/continue/growth/export/decoding in both directions. |
| 2 | **SATISFIED** | The real-user configuration comparison is byte-identical in required pure mode. The measured non-pure plugin-generated agent/command trees remain explicitly declared rather than normalized away. |
| 3 | **SATISFIED** | All 23 upstream commands have exactly one disposition. All 12 implemented commands have table-driven production CLI comparisons for normalized exit/stdout/stderr, explicit per-probe exemptions, and a guard against shrinking coverage. |
| 4 | **NOT SATISFIED** | The executable gate correctly freezes 58 upstream operations, 48 local backends, and ten named `503 backend_unavailable` gaps. But `crates/oc-testkit/tests/compat_suite.rs:143` still says `14 missing local backends`, while the same freshly generated JSON report later says `10 of the 58` and `48 operations have local backends`. A machine-readable compatibility artifact cannot simultaneously publish both inventories. |
| 5 | **SATISFIED** | Five current interop tests cover one existing session through list, open, continuation, transcript growth, export, and opposite-implementation decoding in both directions. |
| 6 | **NOT SATISFIED** | The required behavior now works through the real user surface: plain `models` shows `google` and `kiro-auth`, and the Kiro request-kind hook is pinned bidirectionally. Nevertheless, the authoritative success-criterion text still requires `models --format json`; released 1.18.15 and this port both reject that flag. Todo 139 explicitly says the flag does not exist and must not be used, but the final criterion was not amended to plain `models`. F1 cannot silently substitute a different command for the frozen command. |
| 7 | **SATISFIED** | Rust, WASM, Kiro auth 0.20.6, and Antigravity plugins load in configuration order; one-tier failure isolation is proved, and required Unix CI explicitly runs the feature-enabled 11-test WASM integration suite. |
| 8 | **SATISFIED** | Workspace policy forbids first-party unsafe code and a source-policy test scans `crates/*/src/**`. No first-party unsafe exception was found. |
| 9 | **SATISFIED** | The Rust example plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent tests pin negative delegation boundaries, temperature, deny-by-default permissions, output envelopes, model inheritance/overrides, absence of model-id literals, and all required task selection and continuation fields. |
| 11 | **SATISFIED** | The goal survives two compactions with objective and counters intact; idle continuation, system-owned status, and Markdown objective/status round-trip rules are tested. |
| 12 | **SATISFIED** | `session list --all-projects` is compared with the real global endpoint on the same database, including project summaries and the returned session set. |
| 13 | **SATISFIED** | Preview is non-mutating without confirmation; subtree deletion, exclusion/liveness rules, transactionality, and orphan checks cover the amended ten-table schema-derived set. |
| 14 | **SATISFIED** | Snapshot GC retains referenced stores and removes only unreferenced stores; explicit vacuum reports reclaimed bytes and refuses insufficient disk. |
| 15 | **SATISFIED** | Tracked evidence establishes G1/G2 under frozen formulas, the 500-turn/7,200-second G3/G4 soak, all G5 channel policies, and Linux G6 clean/`SIGKILL`/interactive-PTY/Ctrl-C containment. The Windows half remains implemented and explicitly `NOT EXECUTED`, as the narrowed criterion requires. |
| 16 | **SATISFIED** | MCP is exercised against real servers, LSP against two real language servers, ACP against the real SDK client, and provider families against recorded real traffic rather than only self-authored fixtures. |
| 17 | **SATISFIED** | All 17 intentional divergences are in `docs/divergences.toml` with reasons and executable/doc witnesses. README and generated divergence blocks derive from the authoritative list. The assistant step-part omission is correctly kept out of the allow-list and published as a named gap. |
| 18 | **NOT SATISFIED** | F1 is this **REJECT**. F2 and F4 have no final reports; F3's same-HEAD report remains in progress with a pending verdict. Therefore all four approvals and explicit user acceptance do not exist. |

## Source-derived inventory

- Checked plan lines: **145**, representing todo ids **1–142** with duplicates 124, 125, and 129.
- Workspace: **36 crates**.
- CLI: **23** upstream commands, **12** implemented-command differential rows, and **16** command probes.
- OpenAPI: **58** upstream operations; executable contract **48 local backends / 10 named gaps**.
- Compatibility report: four named gaps total, including the API aggregate and `assistant-turn-step-parts`.
- Plugin surface: real Kiro and Antigravity provider contributions plus Rust and WASM tiers.
- TUI: **184** keybindings.
- Tools: **17** built-ins.
- Session pruning: **10** session-attributable tables.
- Divergences: **17** authoritative entries.

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from tracked evidence:** Rust W-idle ratio 0.0214 under the frozen 0.50 ceiling. |
| G2 | **PASS from tracked evidence:** Rust W-real median 1,494,024 KiB versus the 1,513,496 KiB ceiling; 19,472 KiB margin exceeds the 17,032 KiB five-run spread. |
| G3 | **PASS from tracked evidence:** 500 turns over 7,200 seconds; slope and peak ratio remain below the frozen limits. |
| G4 | **PASS from tracked evidence:** neither the 120-second state-progress watchdog nor the 1,800-second hard deadline tripped. |
| G5 | **PASS from tracked evidence and current tests:** all 17 bounded channels have declared policies, two exclusions are named, and backpressure remains observable. |
| G6 | **PASS under the narrowed criterion:** Linux clean shutdown, parent `SIGKILL`, interactive PTY read, and Ctrl-C containment pass; the Windows Job Object path is implemented and explicitly `NOT EXECUTED`. |

## Blocking findings and closure conditions

1. **Make the generated API report internally consistent.** Replace the stale `SURFACES[api-operations].detail` 14-gap prose with data derived from the same frozen named set as `known_gaps()`, and add a report assertion that no surface detail can disagree with 48/10.
2. **Resolve success criterion 6 in the authoritative criterion itself.** Because released 1.18.15 has no `models --format json`, amend the final success criterion to the already-proved plain `models` surface, preserving the real-plugin test that requires `google` and `kiro-auth`. Alternatively, implementing a subject-only JSON flag would itself violate command parity and is not the correct fix.
3. **Complete one common final review wave.** F2, F3, and F4 must finish against `b753fb9` or a later common HEAD, all four reports must APPROVE that exact HEAD, and the results must be surfaced for explicit user acceptance.

## Validation performed

- Current-HEAD focused results observed during the two workspace attempts: `compat_suite` **16 passed**; `session_interop` **5 passed**; `subject_freshness` **2 passed**; server API **38 passed**; server events **14 passed**; backpressure **21 passed**. The separately audited real-plugin user-surface test passed **1/1** and showed `kiro-auth` and `google`.
- The freshly emitted `target/compat/compat-report.json` confirms the blocking contradiction: its API surface detail says 14 missing backends, while its known-gap entry says ten gaps and 48 backed operations.
- Full ordinary workspace validation was attempted twice without running the opt-in memory gate or ignored soak. Both attempts reached extensive passing suites but failed while the host was listing `oc-tools` tests with `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`. The second attempt used `CARGO_BUILD_JOBS=1`, `RUST_TEST_THREADS=1`, and `--test-threads=1`; the same host process-limit `EAGAIN` remained. This is not an assertion failure, but no complete current-HEAD workspace green result is claimed.
- Because each command chain stopped at the workspace-test `EAGAIN`, current-HEAD Clippy/fmt/metadata were not reached in this F1 run. Tracked todo 138–142 evidence records all three passing on their implementation branches before merge. No third status attempt was made under the two-check limit.
- The expensive G1/G2 measurement and G3/G4 soak were deliberately not rerun.

## Final decision

The implementation behavior is materially stronger than in the fifth wave: all five remediation todos have substantive, mutation-sensitive evidence, and the real Kiro provider now reaches the correct user surface. F1 still cannot approve an internally contradictory compatibility report, silently rewrite an unmet frozen criterion from `models --format json` to `models`, or declare completion before the other final reviewers approve the same HEAD. These are narrow artifact/contract and acceptance defects; they do not justify reopening already satisfied implementation scope.

F1 VERDICT: REJECT
