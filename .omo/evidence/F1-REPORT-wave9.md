# F1 Plan Compliance Audit — Ninth Final Verification Wave

## Verdict

**REJECT**

Audited HEAD `c251665ac3b6fda21c276fe6814cf2ab17006a27` passes the complete Rust workspace gate and closes the implementation seams assigned to todos 150–153. It is not plan-compliant because success criteria 1, 2, and 18 are not satisfied:

1. the binary does not enforce the required max-known-migration ceiling and accepts a database containing a migration id above its newest known id;
2. the real current pure-mode configuration remains structurally unequal to released `opencode` 1.18.15, as the corrected todo-153 evidence now discloses;
3. F1 rejects, no F2–F4 wave-9 reports exist for this HEAD, and the explicit user acceptance required after four approvals therefore cannot exist.

Result: **SATISFIED 15 / NOT SATISFIED 3 / UNVERIFIABLE 0**.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `c251665ac3b6fda21c276fe6814cf2ab17006a27`
- The worktree was clean before this audit. This report is the only intended retained modification; no product source, test, plan, documentation, commit, branch, or remote was changed.
- CodeGraph was attempted first but this sibling worktree is not indexed (`No indexed project found`), so the frozen plan, source, tests, executable behavior, Git state, and committed evidence were inspected directly.
- Every success criterion was re-evaluated. Checked todo rows were mechanically parsed independently, high-risk production seams from todos 150–152 were mutation-tested, the current installed oracle and live config were measured, and the complete Rust gate was run.
- The approximately 100-minute G1/G2 measurement and two-hour G3/G4 soak were not rerun. Their retained raw results, frozen methodology guards, and mutation evidence were audited as directed.
- No live configuration values are reproduced here. Only byte counts, hashes, key names, and equality results are recorded.

## Checked-todo ledger

The current plan contains **153 checked implementation rows**, representing **153 unique numeric ids**, contiguous from 1 through 153. There are no duplicate ids, gaps, or unchecked implementation rows. F1–F4 remain the four unchecked final-verification rows at `.omo/plans/opencode-rust.md:1387-1390`.

Mechanical completion does not override an executable counterexample. Todo 153 is legitimately checked because it makes the live-config test honest and discloses the resulting defect; it does not claim to fix criterion 2. Criterion 18 separately requires all four reviewers to approve one common HEAD and the user to explicitly accept the surfaced results.

## Wave-8 finding disposition

| Finding / todo | Decision |
|---|---|
| Criterion 2 stale “live” fixture / todo 153 | **Seam closed; criterion still unmet.** The fixture now matches `/config/.config/opencode/opencode.json` byte-for-byte and `real_user_config_capture_matches_live_file_byte_for_byte` fails on absence or one-byte drift. The honest same-cwd pure-mode comparison still exposes missing adjacent `agent` and `command` trees and missing `plugin_origins`. |
| Criterion 3 invalid `agent list --format json` / todo 153 | **Closed.** The plan preserves the invalid former requirement and now names the released plain `agent list` surface. Under `OPENCODE_PURE=1`, both binaries emit the same 16 resolved `name (mode)` headers; the fixture-owned differential uses that valid surface. Criterion 3 is now satisfied. |
| F3-W8-D2 permission deadlock / todo 150 | **Closed.** Production publishes `metadata.arguments`, the TUI consumes it, focused dialog scopes receive action keys, and direct-PTY plus tmux evidence proves the request can be answered. This audit independently killed and restored the producer seam. |
| F2-B1 ordinary-hook truncation / todo 151 | **Closed.** Every returned argument is checked before any ordinary-hook output is committed. The production-shape no-op `tool.definition` test rejects a real truncated built-in schema before provider dispatch. This audit independently killed and restored the shared preflight. |
| F4-W8-1 provider identity collapse / todo 152 | **Closed.** Factory selection is separate from provider identity, all fifteen frozen compatible identities are admitted by a closed table, and unknown transports remain refused. This audit independently reintroduced and killed the exact identity-collapse defect. |
| Criterion 18 common approval wave | **Still open.** No `F2-REPORT-wave9.md`, `F3-REPORT-wave9.md`, or `F4-REPORT-wave9.md` exists for this HEAD, F1 itself rejects, and explicit user acceptance has not occurred. |

## Blocking findings

### 1. Criterion 1's max-known-migration ceiling is absent

Criterion 1 requires this binary to refuse any database whose migration journal contains an id above its max-known migration, naming both the ceiling and observed id. The current source does not do so:

- `crates/oc-db/src/migration/mod.rs:37-45` derives the 38 known ids;
- the current highest id is `20260622202450_simplify_session_input` (`:312-313`);
- `apply_only` reads all completed ids and merely skips known ids (`:116-141`);
- there is no comparison against a maximum id and no error carrying an observed future id.

The production entry point confirms the defect. A fresh Rust database was created, then its journal was given `99999999999999_future_migration`. Reopening it through:

```text
target/debug/opencode-rust db --format json "SELECT 1 AS ok"
```

exited **0** and returned `[{"ok":1}]`. It should have refused before serving the query and named both `20260622202450_simplify_session_input` and `99999999999999_future_migration`.

`compat_suite.rs::journal_round_trip_through_the_real_binary_does_not_replay_migrations` remains valuable and passes, but it tests a compatible pinned journal. No test in `compat_suite.rs` exercises an above-ceiling id. The Wave-8 report's criterion-1 approval was therefore too broad.

### 2. Criterion 2 is now honestly, explicitly unmet

Todo 153 corrected the evidence seam. The committed fixture and the live named file are both 24,417 bytes with SHA-256 `502ca4db55e63d958be28bb7ed9b2d687a9a6f2eca84442df37dd8e7245336c6`, and the named drift guard fails visibly if either side is absent or differs.

That guard proves input identity, not output parity. Independent same-cwd measurements under `OPENCODE_PURE=1` and released 1.18.15 produced:

```text
released opencode debug config:     266,233 bytes
target/debug/opencode-rust output:   25,581 bytes
```

After recursive JSON key sorting and removal of only the released binary's empty diagnostic `mode` object, differences remain:

| Key | Released 1.18.15 | Rust |
|---|---:|---:|
| `agent` | 9 entries | 0 entries |
| `command` | 2 entries | 0 entries |
| `plugin_origins` | 3 entries | field absent |

The nine agent files exist under `/config/.config/opencode/agent/powerapps/`, and the two command files exist under `/config/.config/opencode/command/`. These are adjacent real configuration trees discovered even in pure mode, not third-party plugin output and not a permissible normalization. The plan itself now records criterion 2 as `UNMET` at `.omo/plans/opencode-rust.md:1405`.

### 3. Criterion 18 cannot pass in this wave

F1 is a rejection. The evidence directory contains no F2, F3, or F4 wave-9 report for the audited SHA, and four approvals have not been surfaced for explicit user acceptance. This procedural criterion cannot be inferred from green implementation tests.

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **NOT SATISFIED** | The pinned journal round-trip passes, but the required future-migration ceiling is absent. A database carrying `99999999999999_future_migration` was accepted by the production `db` command with exit 0 instead of naming it and the current ceiling. |
| 2 | **NOT SATISFIED** | The live-input drift guard is now honest, but current pure-mode output is 266,233 bytes upstream versus 25,581 bytes here. Real adjacent `agent` and `command` trees plus `plugin_origins` remain missing. |
| 3 | **SATISFIED** | The universal command matrix executes all twelve implemented commands through controlled probes with named, witnessed exemptions. Todo 13 now uses the valid plain agent surface; its pure-mode fixture comparison finds the same 16 resolved headers. All upstream registered commands retain exactly one disposition. |
| 4 | **SATISFIED** | The behavior inventory freezes all 58 upstream operations, invokes both processes, covers 48 local backends, and freezes exactly ten explicit `503 backend_unavailable` gaps. No operation answers `501`; generated compatibility reporting derives its gap count from the frozen set. |
| 5 | **SATISFIED** | `session_interop.rs` alternates the real release and Rust binaries over one absolute database in both directions, proving list, open/continue, strict message and part growth, export, and opposite-implementation history replay onto provider wire requests. |
| 6 | **SATISFIED** | Real Antigravity 1.6.0 and Kiro auth 0.20.6 packages load through the JS host. Production plain `models` output contains their user-visible `google` and `kiro-auth` contributions; dedicated Antigravity auth-loader evidence proves its contribution rather than relying on a catalog fixture. |
| 7 | **SATISFIED** | Real JS auth plugins coexist with Rust and WASM tiers in configuration order; kill, timeout, and missing-runtime cases degrade only one tier. The 21-hook production matrix and lifecycle tests prove every advertised hook reaches a consumed production effect. |
| 8 | **SATISFIED** | Workspace policy forbids first-party unsafe code and the source guard scans `crates/*/src/**`; the complete workspace gate passes. |
| 9 | **SATISFIED** | The Rust example plugin registers a tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent/task tests cover negative delegation boundaries, temperatures, deny-by-default permissions, output envelopes, model inheritance and overrides, absence of model-id literals in `oc-agent`, category/background/continuation, and reasoning effort. |
| 11 | **SATISFIED** | Goal tests cover survival through two compactions, exactly-once guarded idle continuation, model refusal to set system-owned status, and Markdown objective/status round trips. |
| 12 | **SATISFIED** | `session list --all-projects` is compared against the real experimental global endpoint on one database, including project summaries and the matching session set. |
| 13 | **SATISFIED** | Prune defaults to inert preview, requires explicit confirmation, transactionally deletes the parent subtree from all ten attributable tables, and protects shared, compacting, active, and recently touched sessions. |
| 14 | **SATISFIED** | Snapshot GC removes only unreferenced stores; prune remains separate from reclamation; explicit vacuum reports positive reclaimed bytes and checks free disk before rewriting. |
| 15 | **SATISFIED** | Retained frozen results pass G1–G4; G5 freezes 17 persistent bounded channels with behavior gates; G6 executes clean, parent-`SIGKILL`, PTY-read, and Ctrl-C containment on Linux. Windows Job Object support remains explicitly implemented but **NOT EXECUTED**, as the narrowed criterion requires. |
| 16 | **SATISFIED** | Committed tests validate MCP against a real CodeGraph server, LSP against two real language servers, ACP against the real client SDK, and provider behavior against recorded real traffic through production decoders. Cassettes are not represented as live-provider equality. |
| 17 | **SATISFIED** | `docs/divergences.toml` is machine-loaded; reasons, live witnesses, declared count, generated index/detail sections, compatibility matrix, and provider unknown-transport refusal are guarded by tests. |
| 18 | **NOT SATISFIED** | F1 rejects; F2–F4 wave-9 reports for this SHA do not exist; explicit user acceptance after four approvals has not occurred. |

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from retained frozen evidence:** Rust W-idle median 20,380 KiB versus the 477,120 KiB ceiling. |
| G2 | **PASS from retained frozen evidence:** Rust W-real median 1,494,024 KiB versus the 1,513,496 KiB ceiling; all five runs pass and the median margin exceeds the measured spread. |
| G3 | **PASS from retained frozen evidence:** 500 turns over 7,200 seconds; final-half Theil–Sen slope 0.0001775568 MiB/turn and final/middle peak ratio 0.9938255268. |
| G4 | **PASS from retained frozen evidence:** all 500 turns completed without a 120-second meaningful-progress or 1,800-second hard-deadline violation. |
| G5 | **PASS from current tests and retained evidence:** 17 persistent bounded production channels have an exact source-derived inventory and independent progress/overflow behavior gates; two single-completion exclusions are explicit. |
| G6 | **PASS under the narrowed criterion:** Linux clean shutdown, parent `SIGKILL`, interactive PTY reads, and terminal Ctrl-C are executed; Windows is implemented and honestly **NOT EXECUTED**. |

## Observable mutation results

All three mutations changed one production seam, compiled, failed the intended named test for the intended observable, were restored immediately, and passed after restoration. `git diff --exit-code` then confirmed that no mutation remained in the three product files.

1. **Permission metadata producer:** removed `metadata.insert("arguments", args.clone())` from `crates/oc-engine/src/dispatch.rs`. `production_dispatch_arguments_reach_the_rendered_permission_dialog` failed because the rendered shell-command row was blank. Restored result: pass.
2. **Ordinary JS hook truncation preflight:** removed the complete argument scan from `invocation_output`. `noop_tool_definition_hook_rejects_real_truncated_schema_before_provider_dispatch` failed because the no-op hook silently dispatched a damaged deep built-in schema. Restored result: pass.
3. **Provider identity:** changed `Spec::new(&model.provider_id)` to `Spec::new(factory_key)`. `every_todo_94_identity_reaches_its_profile_from_resolved_config` failed with `identity collapsed for openrouter`, observed `openai-compatible` versus expected `openrouter`. Restored result: pass.

## Validation performed

- `cargo test --workspace --offline` attempt 1 was interrupted while the Rust harness listed `oc-testkit` tests with host error `EAGAIN` / `Resource temporarily unavailable`; there was no assertion failure.
- The single permitted retry used `RUST_TEST_THREADS=1 cargo test --workspace --offline` and passed. Summing all 211 successful result groups from the complete captured log gives **3404 passed, 0 failed, 2 ignored**.
- `cargo clippy --workspace --all-targets --offline -- -D warnings` — **PASS**, zero warnings/errors.
- `cargo fmt --all --check` — **PASS**, no output.
- `lsp_diagnostics` was attempted on this report, but the MCP is rooted at the main checkout and rejected the sibling-worktree path before analysis as outside its request cwd. The only changed file is this Markdown report; no Rust source remains modified, and the compiler-backed workspace test and Clippy gates cover the restored source tree.
- The long G1/G2 measurement and G3/G4 soak were deliberately not rerun.

## Closure conditions

1. Enforce the max-known-migration ceiling before any existing database is used. Add a production-path test whose journal contains an id greater than `20260622202450_simplify_session_input`, require refusal, and assert that the error names both the ceiling and observed id.
2. Implement pure-mode discovery and merged debug projection for the live adjacent `agent` and `command` trees and account for `plugin_origins` according to the released pure-mode contract. The real same-cwd comparison must become byte-identical after only the already justified normalization; do not allow-list the current defect.
3. After criteria 1 and 2 pass, run F1–F4 against one common HEAD, obtain four approvals, surface all four results to the user, and receive the explicit okay required by criterion 18.

## Final decision

The implementation and static gates are green, todos 150–153 close their assigned evidence and seam defects, and criterion 3 is repaired. Those facts do not override two direct production counterexamples or the unfulfilled final-approval gate. This HEAD is therefore not plan-compliant.

**F1 VERDICT: REJECT**
