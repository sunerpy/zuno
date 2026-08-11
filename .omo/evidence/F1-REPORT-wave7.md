# F1 Plan Compliance Audit — Seventh Final Verification Wave

## Verdict

**REJECT**

Audited HEAD `0e1fe93b354e12705e6a83ba28f09259307e6053` satisfies success criteria 1–17. The two concrete sixth-wave contract defects are closed: the API inventory now derives one consistent 58/48/10 result, and success criterion 6 explicitly requires the real plain `models` surface rather than the nonexistent `models --format json` flag. Todos 143 and 144 also close the final plugin measurement and production-dispatch seams.

Approval is nevertheless impossible because success criterion 18 is not satisfied. No F2, F3, or F4 wave-7 report exists for this audited HEAD, so all four approvals and the user's explicit acceptance do not exist.

Result: **SATISFIED 17 / NOT SATISFIED 1 / UNVERIFIABLE 0**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `0e1fe93b354e12705e6a83ba28f09259307e6053`
- The worktree was clean before this audit. This report is the only intended retained modification; no product source, test, plan, documentation, other evidence, commit, branch, or remote was changed.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so the frozen plan, current source, tests, generated report, Git state, and tracked evidence were inspected directly.
- The approximately 100-minute G1/G2 measurement and two-hour G3/G4 soak were deliberately not rerun. Their committed measurements, frozen methodology, current executable guards, and mutation sensitivity were audited instead.

## Checked-todo ledger

The current plan contains **144 checked implementation lines**, representing **144 unique todo ids**, contiguous from 1 through 144. There are no duplicate ids and no gaps. Todos 1–142 retain substantive implementation ancestry; todos 143 and 144 add the final measured plugin proof and production Auth/Tool dispatch remediation. The unchecked F1–F4 rows are final-verification decisions, not implementation todos.

Mechanical ledger completeness does not satisfy criterion 18 by itself: all four final reviewers must approve the same HEAD and the results must then be surfaced for explicit user acceptance.

## Sixth-wave blocker disposition

| Sixth-wave blocker | Seventh-wave disposition |
|---|---|
| API inventory report | **Closed.** `FROZEN_API_GAPS` enumerates ten operations; the live gate and generated report agree on 58 upstream operations, 48 local backends, and ten named `503 backend_unavailable` gaps. The stale 14-gap surface prose is gone. |
| Success criterion 6 command | **Closed.** The authoritative criterion is amended to plain `models`, explicitly records that released 1.18.15 has no `--format`, and preserves the real-plugin proof for `google` and `kiro-auth`. |
| Final acceptance | **Still open and solely blocking.** No F2–F4 wave-7 report exists for HEAD `0e1fe93b`; therefore a common four-approval wave and explicit user acceptance do not exist. |

## Seventh-wave todo disposition

| Todo | Decision |
|---:|---|
| 143 | **Satisfied.** The retained measurement compares byte-identical 2,944-line `models --verbose` output with and without Antigravity. `plugin_models.rs` proves Antigravity through its unique `google` auth resource, with a negative control, rather than mistaking the pre-existing catalog provider id for plugin execution. |
| 144 | **Satisfied.** Production CLI paths dispatch both `HookInvocation::Auth` and `HookInvocation::Tool`; provider listing applies the auth loader, turn construction obtains plugin tools, and the governed registry inserts them through the same permission layer as built-ins. Current production-reachability and governance tests pass. |

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | The current compatibility suite uses released `opencode` 1.18.15, preserves the 38-row migration journal through the real-binary round trip, and enforces refusal above the max-known migration. |
| 2 | **SATISFIED** | The real user configuration is byte-identical after normalization in required pure mode. The measured non-pure third-party plugin trees remain a named divergence rather than being normalized away. |
| 3 | **SATISFIED** | Every upstream CLI command has one disposition, and implemented commands have production differential comparisons with bounded, named presentation/diagnostic exceptions. |
| 4 | **SATISFIED** | The executable behavior matrix freezes all 58 operations, compares all 48 locally backed operations, and freezes the remaining ten by exact name as explicit `503 backend_unavailable` gaps. The generated compatibility report is internally consistent. |
| 5 | **SATISFIED** | Five same-session interop tests cover list, open, continue, transcript growth, export, and opposite-implementation decoding in both directions. |
| 6 | **SATISFIED** | Real Kiro auth 0.20.6 and Antigravity plugins load through the JS host and contribute `kiro-auth` and `google` to plain `models`; the amended criterion matches the actual 1.18.15 command surface. |
| 7 | **SATISFIED** | Rust, WASM, Kiro, and Antigravity plugin tiers are covered, ordering and isolated degradation are tested, and the production Auth/Tool paths introduced by todo 144 are reachable. |
| 8 | **SATISFIED** | Workspace policy forbids first-party unsafe code and the release/source guard scans first-party crate sources. |
| 9 | **SATISFIED** | The Rust example plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent tests pin delegation boundaries, temperature, deny-by-default permissions, envelopes, model inheritance/overrides, absence of model-id literals, and the required task fields. |
| 11 | **SATISFIED** | Goal state survives two compactions; idle continuation, system-owned status, and Markdown objective/status round-trip rules are executable tests. |
| 12 | **SATISFIED** | `session list --all-projects` is differentially checked against the real global endpoint with project summaries and matching session sets. |
| 13 | **SATISFIED** | Preview is inert by default, destructive actions require confirmation, subtree deletion is transactional, and exact preview/delete accounting plus orphan checks cover all ten session-attributable tables. Shared, compacting, reported-active, and recently touched sessions receive the required protections. |
| 14 | **SATISFIED** | Snapshot GC retains referenced stores and removes only zero-reference stores. Explicit vacuum reports reclaimed bytes, remains separate from prune, and refuses when available disk is below the database size. |
| 15 | **SATISFIED** | Tracked measurements establish G1/G2 under the frozen formulas, the 500-turn/7,200-second G3/G4 soak, all 17 bounded-channel policies, and Linux G6 clean/`SIGKILL`/interactive-PTY/Ctrl-C containment. Windows Job Object support is implemented and explicitly **NOT EXECUTED**, as the narrowed criterion requires. |
| 16 | **SATISFIED** | Current tests exercise a real CodeGraph MCP server, `typescript-language-server` and `rust-analyzer`, and the real ACP SDK client. Provider-family coverage replays recorded real traffic through production decoders. |
| 17 | **SATISFIED** | `docs/divergences.toml` contains 17 intentional divergences with reasons; live behavior witnesses, declared-count checks, generated documentation, and current docs tests agree. Known missing behavior such as assistant step parts remains a gap rather than being laundered into the allow-list. |
| 18 | **NOT SATISFIED** | This F1 report is **REJECT** because F2–F4 wave-7 approvals for this exact HEAD are absent. Consequently all four approvals and explicit user acceptance are absent. |

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from tracked evidence:** Rust W-idle median 20,380 KiB versus the frozen 477,120 KiB ceiling, ratio 0.0214. |
| G2 | **PASS from tracked evidence:** Rust W-real median 1,494,024 KiB versus the 1,513,496 KiB ceiling. The 19,472 KiB margin exceeds the 17,032 KiB five-run spread, and every individual run is below the ceiling. |
| G3 | **PASS from tracked evidence:** 500 turns over 7,200 seconds; Theil–Sen slope 0.0001775568 MiB/turn and final/middle peak ratio 0.9938255268 are below 1.0 and 1.5. |
| G4 | **PASS from tracked evidence:** the soak completed without tripping the 120-second meaningful-state-progress watchdog or 1,800-second hard turn deadline. Deliberate stall and hard-deadline tests currently pass. |
| G5 | **PASS from current tests and tracked evidence:** 17 persistent bounded production channels have exact declarations and independent behavior gates; two single-completion exclusions are explicit; no undeclared construction exists. |
| G6 | **PASS under the narrowed criterion:** current Linux clean-shutdown and parent-`SIGKILL` process-tree tests pass with zero orphans. README and tests explicitly mark the Windows half **NOT EXECUTED**. |

## Observable mutation

The frozen G1 formula in `docs/perf-methodology.md` was temporarily changed from `0.50` to `0.51`. `methodology_formula_section_matches_its_revision_hash` failed with observed digest `ffb85ca1…` versus registered digest `db49ffeb…`. The exact original byte was restored, and `git diff --exit-code -- docs/perf-methodology.md` passed before the full workspace gate. No mutation remains.

## Validation performed

Current-HEAD focused validation passed:

- session maintenance: prune **9/9**, shared prune service **6/6**, vacuum **14/14**;
- G5/G6 backpressure and containment: **21/21**;
- frozen methodology digest: **1/1**;
- real CodeGraph MCP initialize/list/call: **1/1**;
- live LSP servers: **2/2**;
- real ACP SDK: **1/1**;
- provider cassette matrix: **6/6**;
- generated documentation and divergence guards: **13/13**.

The first full workspace attempt reached extensive green suites but the host returned `EAGAIN` (`Resource temporarily unavailable`) while listing `oc-tools` tests; this was not an assertion failure. Under the mandated two-attempt limit, the second attempt completed successfully with:

- `cargo test --workspace --offline -- --test-threads=1`: **3365 passed, 0 failed, 2 ignored**;
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: **PASS**, zero warnings/errors;
- `cargo fmt --all --check`: **PASS**.

The long G1/G2 measurement and G3/G4 soak were not rerun, per audit instructions.

## Blocking finding and closure condition

1. **Complete one common final-review wave.** F2, F3, and F4 must each finish against `0e1fe93b354e12705e6a83ba28f09259307e6053` (or all four reviewers must restart on one later common HEAD), all four reports must approve that exact HEAD, and those results must be surfaced to the user for explicit acceptance.

No implementation, compatibility, performance-methodology, or documentation blocker remains in F1's criteria 1–17 audit.

## Final decision

The seventh wave closes every substantive F1 finding from wave 6 and passes the complete current-HEAD workspace gate. The artifact is not yet finally accepted because the frozen plan makes four same-HEAD approvals plus explicit user acceptance a success criterion. Until F2–F4 complete, F1 cannot convert that missing external gate into an approval.

F1 VERDICT: REJECT
