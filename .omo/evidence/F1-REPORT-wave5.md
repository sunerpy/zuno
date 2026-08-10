# F1 Plan Compliance Audit — Fifth Final Verification Wave

## Verdict

**REJECT**

The fifth remediation wave closes the universal implemented-command differential, bidirectional same-session interoperability, real-auth-plugin three-tier coexistence, and disconnected-observer fail-closed behavior. The ordinary workspace gates also complete cleanly. Approval is nevertheless impossible because four final success criteria remain false at the audited HEAD: the compatibility report still publishes the obsolete 44-backed/14-gap API inventory instead of the plan's 48/10 inventory; the required `models --format json` user surface is not implemented and does not prove Kiro provider visibility; public divergence documentation still says 13 while the authoritative allow-list contains 17 entries; and F2–F4 have not approved this common final HEAD.

Result: **SATISFIED 14 / NOT SATISFIED 4 / UNVERIFIABLE 0**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `56c229c0abe070b72cd18a8279e3ba1ef9646446`
- The initial worktree was clean. This report is the only permitted file change; no source, test, plan, evidence, documentation, commit, branch, or remote was modified.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so the frozen plan, current source, tests, Git history, generated output, and tracked evidence were inspected directly.
- The approximately 100-minute G1/G2 gate and the two-hour G3/G4 soak were not rerun, as instructed. Their committed artifacts and the executable methodology guards were audited instead.

## Checked-todo ledger

The plan contains **140 checked implementation-todo lines** representing **137 unique todo ids**, contiguous from 1 through 137. Todos 124, 125, and 129 each appear twice. The plan contains **138 `Commit: Y` lines**; the duplicate todo lines intentionally share their implementation commit. Every unique todo id maps to substantive implementation ancestry at the audited HEAD. No empty or commit-less checked implementation claim was found.

This mechanical traceability is not itself final compliance. The generated API inventory, user-visible model command, public divergence documentation, and final-review state still contradict success criteria despite all implementation todos being checked.

## Previous F1 blocker disposition

| Fourth-wave blocker | Fifth-wave disposition |
|---|---|
| Universal implemented-command output parity | **Closed.** Todo 135 executes all 12 implemented commands against subject and oracle, compares normalized exit status/stdout/stderr, and includes `the_comparison_cannot_shrink_into_exemptions`. |
| API inventory contract disagreement | **Still open.** The plan now correctly says 48 local backends and ten frozen gaps, and the live named-set guard pins ten. However, `crates/oc-testkit/tests/compat_suite.rs` and the emitted `target/compat/compat-report.json` still report 44 backed and 14 gapped operations. |
| Bidirectional same-session interoperability | **Closed.** Todo 136 covers TS→Rust and Rust→TS list/open/continue/export lifecycles over the same persisted session, transcript growth, and opposite-implementation decoding. |
| Real Kiro request and model visibility | **Partially closed, still blocking.** The plan legitimately excludes the credential/network-only `effort` field and the real Kiro hook is tested in both directions. The required `models --format json` command is nevertheless rejected by the Rust CLI, so provider visibility is not proven through the specified user surface. |
| Real-plugin three-tier coexistence | **Closed.** Todo 137 loads the Rust and WASM examples with real Kiro auth 0.20.6 and Antigravity auth packages, preserves configuration order, and proves one-tier failure isolation under `--features wasm`. |
| Final acceptance | **Still open.** This F1 result is REJECT, and no F2/F3/F4 approval reports target this final HEAD. |

Todo 134 additionally closes the fail-open permission seam: `disconnected_only_session_observer_rejects_permission_without_running_the_tool` and `permission_without_an_observer_is_rejected_by_the_deadline` prove observer loss and the independent five-minute deadline reject rather than execute.

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | The compatibility oracle resolves installed `opencode` 1.18.15, the capture agrees, and the journal round-trip preserves all 38 migration rows while the max-known-migration refusal remains explicit. The workspace compatibility tests pass. |
| 2 | **SATISFIED** | The real-user `debug config` comparison runs in required pure mode. The excluded non-pure plugin-generated agent/command trees remain explicitly measured and declared rather than normalized away. |
| 3 | **SATISFIED** | All 23 upstream commands have exactly one disposition. Todo 135 compares every one of the 12 implemented commands through the production CLI against the oracle and prevents the compared set from shrinking into exemptions. |
| 4 | **NOT SATISFIED** | The executable gap-set guard correctly pins 58 operations, 48 local backends, and ten named `503 backend_unavailable` gaps. However, the machine-generated compatibility report still emits `14 of the 58 upstream /api operations` and `Forty-four operations have local backends`, sourced from stale text in `crates/oc-testkit/tests/compat_suite.rs`. A final artifact that publishes 44/14 contradicts the criterion's frozen 48/10 contract. |
| 5 | **SATISFIED** | Todo 136 proves one existing session survives list, open, continuation, transcript growth, opposite-side decoding, and export in both TS→Rust and Rust→TS directions. |
| 6 | **NOT SATISFIED** | Both real auth packages load, Kiro auth consistently resolves to 0.20.6, and the Kiro request-kind header hook is positively and negatively pinned. But `opencode-rust models --format json` exits 2 with `unexpected argument '--format'`; plain `models` exits 0 but the observed output contains Antigravity entries and no Kiro entry. The criterion explicitly requires both providers to appear through `models --format json`, so plugin-load tests alone are insufficient. |
| 7 | **SATISFIED** | The real-auth coexistence integration loads Rust, WASM, Kiro auth 0.20.6, and Antigravity auth in configuration order and proves killing one tier degrades only that tier. The test is explicit about requiring the `wasm` feature rather than silently passing a default-feature stub. |
| 8 | **SATISFIED** | Workspace lint policy forbids first-party unsafe code, source-policy tests scan first-party crate sources, all-target Clippy completed without diagnostics, and the workspace build passed. |
| 9 | **SATISFIED** | The example Rust JSON-RPC plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent tests pin negative delegation boundaries, temperature, deny-by-default permissions, output envelopes, model inheritance/overrides, absence of model-id literals, and all required task selection/continuation fields. |
| 11 | **SATISFIED** | The regression performs two consecutive compactions, proves each discards the goal injection, and preserves the objective, counters, active status, failure streak, and subsequent SQL-derived injection. Idle guards, status ownership, and Markdown projection are covered. |
| 12 | **SATISFIED** | `session list --all-projects` is compared with the real global endpoint on the same database, including project summaries and the session set. |
| 13 | **SATISFIED** | Preview is non-mutating without confirmation; subtree deletion, exclusions, liveness, transactionality, and orphan checks cover the amended complete ten-table schema-derived set. |
| 14 | **SATISFIED** | Artifact GC retains referenced snapshot stores and removes unreferenced stores. Explicit vacuum reports reclaimed bytes, checks integrity, and refuses insufficient free space. |
| 15 | **SATISFIED** | Tracked evidence establishes G1 and G2 under frozen formulas, the 500-turn/7,200-second G3/G4 soak, G5 channel policies, and Linux G6 clean/`SIGKILL`/interactive-PTY/Ctrl-C containment. The Windows half is implemented and explicitly disclosed as `NOT EXECUTED`, as the narrowed criterion requires. |
| 16 | **SATISFIED** | MCP is exercised against real servers, LSP against `rust-analyzer` and `typescript-language-server`, ACP against the real SDK client, and provider wire families against recorded real traffic rather than solely self-authored fixtures. |
| 17 | **NOT SATISFIED** | `docs/divergences.toml` and `DECLARED_COUNT` contain 17 authoritative entries, but `README.md` and `docs/divergences.md` still state 13 divergences. Therefore the public divergence page and summary do not accurately document the current allow-list, and the passing docs gate does not enforce this count consistency. |
| 18 | **NOT SATISFIED** | F1 is this **REJECT**. The checked-in F2, F3, and F4 base reports audit an older HEAD (`70114aa…`) and end in REJECT; no reports show all four reviews approving `56c229c…`, and no explicit user acceptance exists. |

## Source-derived inventory

- Workspace: **36 crates**, matching the amended closed roster and locked metadata.
- CLI: **23** upstream commands with dispositions; **12** implemented commands receive universal differential coverage.
- OpenAPI: **58** upstream operations; current executable contract is **48 local backends / 10 named gaps**.
- Server compatibility: **20** measured v1 routes.
- Plugin surface: **21** hooks.
- TUI: **184** keybindings.
- Tools: **17** built-ins.
- Session pruning: **10** session-attributable tables.
- Divergences: **17** authoritative TOML entries; public prose remains stale at 13.

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from tracked evidence:** Rust W-idle ratio 0.0214 under the frozen 0.50 ceiling. |
| G2 | **PASS from tracked evidence:** Rust W-real median 1,494,024 KiB versus the 1,513,496 KiB ceiling; ratio 0.4936; all five runs pass; 19,472 KiB margin exceeds the 17,032 KiB spread. |
| G3 | **PASS from tracked evidence:** 500 turns over 7,200 seconds with Theil–Sen slope and peak ratio below the frozen limits. |
| G4 | **PASS from tracked evidence:** no 120-second state-progress watchdog or 1,800-second hard-deadline trip. |
| G5 | **PASS from tracked evidence and current tests:** every bounded channel has a declared policy, and production turn-event backpressure is mutation-sensitive. |
| G6 | **PASS under the approved narrowed criterion:** Linux clean shutdown, parent `SIGKILL`, interactive PTY read, and Ctrl-C containment pass; Windows Job Object source/test exists and is explicitly `NOT EXECUTED` on this Linux host. |

## Blocking findings and closure conditions

1. **Regenerate the API compatibility inventory from the executable 48/10 set.** Replace the stale 44/14 source text, make the emitted compatibility report derive its counts from the same named set used by the live matrix, and add an assertion that the generated report cannot disagree with the plan contract.
2. **Implement and prove the required model user surface.** `models --format json` must parse, exit 0, and expose both real Antigravity and Kiro-auth providers after plugin loading. Add a user-surface integration test so a plugin that merely loads without contributing models cannot satisfy the criterion.
3. **Regenerate and pin divergence documentation.** Make `README.md` and `docs/divergences.md` derive the current count and entries from `docs/divergences.toml`; the docs test must fail on a 17-versus-13 mismatch.
4. **Run one common final verification wave.** After the three artifact defects above are fixed, rerun F1–F4 against the same HEAD, obtain four APPROVE verdicts, surface them, and wait for explicit user acceptance.

## Validation performed

- `cargo test --workspace --offline` — **PASS**: 208 result groups, **3,349 passed, 0 failed, 2 ignored**; no host-resource fault.
- `cargo clippy --workspace --all-targets --offline` — **PASS**, zero diagnostics.
- `cargo fmt --all --check` — **PASS**.
- `cargo metadata --locked --offline --format-version 1` — **PASS**: 520 packages, **36 workspace members**, 520 resolve nodes.
- `cargo build --workspace --offline` — **PASS**.
- Focused todo 134–137 checks, including CLI parity, bidirectional session interoperability, fail-closed observer behavior, and real-plugin coexistence with `--features wasm` — **PASS**.
- `opencode-rust models --format json` — **FAIL as a compliance probe**, exit 2 because `--format` is not accepted.
- The opt-in memory gate and ignored two-hour soak were deliberately not rerun; committed evidence was audited instead.

## Final decision

The implementation has closed every behavioral blocker added in todos 134–137, and the complete ordinary Rust verification surface is green. F1 still cannot approve a project whose generated API report contradicts its executable inventory, whose required model-list command does not exist, whose public divergence count is stale, and whose peer final reviews do not approve the same HEAD. These are finite artifact and acceptance defects, not requests for broader scope.

F1 VERDICT: REJECT
