# F1 Plan Compliance Audit — Tenth Final Verification Wave

## Verdict

**APPROVE**

Audited HEAD `2e742986206d5a8707508b4008d2b56d651f0864` satisfies all seventeen implementation and artifact success criteria in F1's review scope. The two blockers from F1 wave 9 are closed: databases above the compiled migration ceiling are refused before use, and the real pure-mode `debug config` projection is exactly equal to released `opencode` 1.18.15 after removing only that release's empty deprecated `mode: {}` diagnostic object. The wave-9 defects assigned to todos 154–161 are also closed through production-path tests and independent observable mutation probes.

Result: **SATISFIED 17 / NOT SATISFIED 0 / UNVERIFIABLE 0** for criteria 1–17.

This F1 approval is not a declaration that the whole project is done. The separate final gate still requires current-HEAD F2, F3, and F4 approvals, surfacing all four results to the user, and the user's explicit okay. No F2–F4 wave-10 reports exist at the time of this report, so that gate remains pending rather than inferred.

## Scope and method

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `2e742986206d5a8707508b4008d2b56d651f0864`
- CodeGraph was attempted earlier for this sibling worktree and reported `No indexed project found`; source, tests, executable behavior, the frozen plan, and committed evidence were therefore inspected directly.
- The worktree was clean before the audit. Every temporary product mutation was restored immediately after its named test failed, and `git diff --exit-code` plus `git status --porcelain` were clean before this report was created.
- The approximately 100-minute G1/G2 measurement and two-hour G3/G4 soak were not rerun. Their retained raw results and methodology guards were audited as required. Current functional, backpressure, and process-containment tests were exercised by the complete workspace gate.
- No product source, test, plan, documentation, commit, branch, remote, or user configuration was changed. This report is the only intended retained modification.

## Checked-todo ledger

The frozen plan contains **161 checked implementation rows**, representing **161 unique numeric ids**, contiguous from 1 through 161. There are no duplicate ids, gaps, or unchecked numeric implementation rows. F1–F4 remain separate unchecked review-gate rows; their checkbox state does not negate the mechanical completion of the implementation ledger and does not substitute for reviewer approval.

## Wave-9 finding disposition

| Finding / todo | Decision |
|---|---|
| Compatible stream can remain idle forever / 154 | **Closed.** Reads have a 120-second per-chunk idle default and a 180-second override cap. Already received chunks survive the timeout. The provider suite and an independent 86,400-second default mutation prove the production policy is guarded. |
| Future migration journal accepted / 155 | **Closed.** The ceiling is derived from `MIGRATION_IDS`; a future id is refused before the `db` command serves a query, while an unknown below-ceiling id remains accepted. Removing the production guard makes the exact entry test serve the forbidden query and fail. |
| Compatible `/responses` uses Chat bytes / 156 | **Closed.** Responses surfaces send `input` and `max_output_tokens`, omit `messages`, and use a typed Responses event decoder and Responses cassettes. Independently forcing either request construction or decoding through Chat makes its named test fail. |
| Edit permission hides path/diff / 157 | **Closed.** Production arguments reach the view; collapsed and fullscreen layouts show the target and replacement diff; the footer names the actual Up/Down bindings. Removing the argument producer makes the production-render test fail. |
| Canonical `parts` mutation discarded / 158 | **Closed.** Provider history is rebuilt from transformed canonical parts. Restoring the old `message.info` projection makes the real provider-request assertion fail. |
| `chat.message` lacks upstream identity fields / 159 | **Closed.** The hook receives the real `MessageRecord`; `id`, `sessionID`, `agent`, and `model` are checked against live hook input. Removing `id` from `MessageRecord::to_json` makes the production lifecycle test fail. |
| `PluginKind::Tui` has no production path / 160 | **Closed.** Interactive startup loads a TUI-target runtime while the turn runtime remains server-targeted. Routing the TUI target to `Server` leaves the server marker present but removes the TUI marker and fails the PTY test. |
| Pure-mode runtime trees absent from `debug config` / 161 | **Closed.** Runtime Markdown agents, commands, plugin ordering, and `plugin_origins` are merged. The released and Rust canonical documents are both 252,891 bytes with 9 agents, 2 commands, and 3 plugin origins. Renaming the emitted `agent` key makes the real-oracle differential fail. |

## Success-criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **SATISFIED** | The full compatibility gate is green. The production `db` entry rejects `99999999999999_future_migration`, names it and ceiling `20260622202450_simplify_session_input`, accepts `20260622202449_unknown_gap`, and preserves compatible journals. `session_interop` also proves the released binary can consume this port's persisted data. |
| 2 | **SATISFIED** | `criterion_2_pure_debug_config_matches_the_released_binary` passes against released 1.18.15 in the real config directory. After removing only released's empty `mode` object, both canonical documents are exactly 252,891 bytes and contain 9 agents, 2 commands, and 3 plugin origins. The comparison is exact JSON equality, not an allow-list for the former omission. |
| 3 | **SATISFIED** | The complete command/disposition matrix remains green in the workspace gate. Every implemented command reaches a non-pending production handler, normalized surfaces use valid released flags, and every upstream command has one recorded disposition. |
| 4 | **SATISFIED** | The frozen 58-operation behavior inventory remains green: all operations are invoked, 48 local backends are compared, exactly ten frozen unavailable backends answer explicit `503 backend_unavailable`, no operation answers `501`, and the two declared fixture exemptions remain bounded. |
| 5 | **SATISFIED** | `session_interop` passes all five cases. The released and Rust binaries alternately list, open, continue, export, and replay one persisted session in both directions, including strict message/part growth and provider-wire history. |
| 6 | **SATISFIED** | The real Antigravity 1.6.0 and Kiro auth 0.20.6 packages remain covered through the JS host and production plain `models` surface; their `google` and `kiro-auth` contributions are guarded without inventing the invalid `--format` flag. |
| 7 | **SATISFIED** | JS, Rust, and WASM tiers retain ordered, isolated lifecycle behavior. All advertised hooks have consumed production effects; the repaired message transform, full `chat.message` record, and real TUI factory are independently mutation-sensitive. |
| 8 | **SATISFIED** | Workspace policy forbids first-party unsafe code, the source guard scans the closed crate roster, and the complete all-target Clippy gate passes with warnings denied. |
| 9 | **SATISFIED** | The Rust example plugin still registers its tool and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Agent/task gates cover negative delegation boundaries, temperatures, deny-by-default permissions, output envelopes, model inheritance and overrides, category/background/continuation, and reasoning effort; `oc-agent` contains no model-id literals. |
| 11 | **SATISFIED** | Goal tests retain objective/counters across two compactions, fire guarded idle continuation exactly once, prevent model writes to system-owned status, and round-trip objective edits through the Markdown projection while rejecting status edits. |
| 12 | **SATISFIED** | Global `session list --all-projects` remains compared with the released global endpoint on one database, including project summaries and the matching session set. |
| 13 | **SATISFIED** | Prune is inert preview by default, requires explicit confirmation, deletes the full parent subtree transactionally from all ten session-attributable tables, and protects shared, compacting, active, and recently touched sessions. |
| 14 | **SATISFIED** | Snapshot GC removes only unreferenced stores; prune and reclamation remain separate; explicit vacuum reports positive reclaimed bytes and refuses when free disk is insufficient. |
| 15 | **SATISFIED** | Retained frozen evidence passes G1–G4: G1 20,380 KiB, G2 1,494,024 KiB, G3 500 turns/7,200 seconds with slope 0.0001775568 MiB/turn and peak ratio 0.9938255268, and G4 no watchdog violation. Current G5/G6 gates remain green. Linux clean, parent-`SIGKILL`, PTY-read, and Ctrl-C containment are executed; Windows Job Object support is implemented and honestly **NOT EXECUTED** on this Linux host. |
| 16 | **SATISFIED** | Committed gates validate MCP against a real CodeGraph server, LSP against two real language servers, ACP against the real client SDK, and provider behavior against recorded real traffic through production decoders. Responses tests now use Responses traffic rather than a protocol-matched self-authored false positive. |
| 17 | **SATISFIED** | `docs/divergences.toml` remains the machine authority; reasons, live witnesses, declared count, generated index/detail sections, compatibility matrix, and unknown-provider refusal are executable guards. None of todos 154–161 was laundered into a new divergence. |

## G1–G6 evidence decision

| Gate | Decision |
|---|---|
| G1 | **PASS from retained frozen evidence:** Rust W-idle median 20,380 KiB versus the 477,120 KiB ceiling. |
| G2 | **PASS from retained frozen evidence:** Rust W-real median 1,494,024 KiB versus the 1,513,496 KiB ceiling; all five retained runs pass. |
| G3 | **PASS from retained frozen evidence:** 500 turns over 7,200 seconds; final-half Theil–Sen slope 0.0001775568 MiB/turn and final/middle peak ratio 0.9938255268. |
| G4 | **PASS from retained frozen evidence:** all 500 turns completed without a 120-second state-progress or 1,800-second hard-deadline violation. |
| G5 | **PASS from current tests and retained evidence:** the exact bounded-channel inventory and per-policy progress/overflow guards remain green. |
| G6 | **PASS under the approved narrowed criterion:** Linux clean and abnormal containment, interactive PTY reads, and terminal Ctrl-C are executed; Windows remains explicitly NOT EXECUTED here. |

## Independent observable mutation results

All nine mutations changed a production producer, selector, or consumer; each compiled far enough to run the intended named test, failed for the intended externally observable reason, and was restored immediately. A clean `git diff --exit-code` afterwards proves no mutation remained.

1. **Migration ceiling:** removed `refuse_future_migrations(&completed)?`. The production `db` entry served `[{"should_not_be_served":1}]`, and `future_migration_in_the_journal_is_refused_before_the_db_command_serves_a_query` failed.
2. **Runtime config projection:** renamed output key `agent` to `agent_mutant`. The real released-binary criterion-2 differential failed.
3. **Responses request producer:** forced Responses through `build_chat`. `responses_surface_uses_input_and_max_output_tokens_not_chat_fields` failed because no `input` array existed.
4. **Responses stream consumer:** forced Responses through `ChunkTranslator`. `responses_events_decode_text_reasoning_tools_and_usage` produced only an empty terminal event instead of reasoning, text, tool, and usage events.
5. **Canonical message transform:** restored the old `message.info` projection. The real lifecycle provider request lacked `:messages`, and `ordinary_plugin_lifecycle_hooks_run_through_the_real_binary` failed.
6. **Full `chat.message` shape:** removed the `id` insertion from `MessageRecord::to_json`. The same production lifecycle test observed `Null` instead of the live message id and failed.
7. **TUI plugin factory:** routed `PluginRuntimeTarget::tui` to `JsPluginKind::Server`. The PTY test retained the server marker but lost the TUI marker and failed.
8. **Streaming idle policy:** changed the production default from 120 to 86,400 seconds. `production_transport_installs_a_sane_default_idle_timeout` failed because the liveness bound was no longer met.
9. **Edit permission producer:** removed `metadata.arguments`. The production edit dialog retained its path fallback but lost the visible diff, and `production_edit_dispatch_renders_path_and_diff_in_collapsed_and_fullscreen` failed.

## Validation performed

- Targeted production-path tests all passed on the restored tree:
  - `cargo test -p oc-cli --test db_migration_ceiling --offline` — 3 passed.
  - criterion-2 released-binary differential — 1 passed.
  - `cargo test -p oc-provider-compatible --offline` — 65 unit tests plus all integration and doc targets passed.
  - permission view/bridge suites — 23 + 7 passed.
  - real plugin lifecycle — 1 passed.
  - production PTY TUI-plugin selection — 1 passed.
  - `cargo test -p oc-testkit --test session_interop --offline` — 5 passed.
- Complete gate, run once after every mutation had been restored:
  - `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace --offline -- --test-threads=1` — **3421 passed, 0 failed, 2 ignored** across 212 successful result groups.
  - `CARGO_BUILD_JOBS=1 cargo clippy --workspace --all-targets --offline -- -D warnings` — **PASS**, zero warnings/errors.
  - `cargo fmt --all --check` — **PASS**, no formatting difference.
- `lsp_diagnostics` was attempted on this changed report. The tool first rejected the sibling-worktree path as outside its fixed request root; an exact temporary copy under that root then reported that no LSP server is configured for `.md`. The temporary copy was deleted. No source file is changed, and the compiler-backed workspace and all-target Clippy gates completed cleanly.
- The long G1/G2 measurement and G3/G4 soak were deliberately not rerun.

## Concurrent final-review gate status

- Branches `main`, `task-F1`, `task-F2`, `task-F3`, and `task-F4` currently point to the same audited commit `2e742986`.
- This report supplies **F1 APPROVE** for that commit.
- No `F2-REPORT-wave10.md`, `F3-REPORT-wave10.md`, or `F4-REPORT-wave10.md` exists at report time. Their latest reports are wave-9 rejections of older commit `c251665a` and cannot be reused as judgments on this HEAD.
- The user's explicit okay after seeing all four current reports has not occurred.
- Therefore the implementation is F1-compliant, but the final four-review-and-user-acceptance gate is **PENDING**. This report does not declare the project complete.

## Final decision

The implementation ledger is complete, criteria 1–17 are satisfied, both prior F1 blockers are closed by exact production-path tests, todos 154–161 close the remaining wave-9 defects, all independent mutations are killed, and the complete offline test/Clippy/format gate is green. F1 therefore approves audited HEAD `2e742986206d5a8707508b4008d2b56d651f0864`.

**F1 VERDICT: APPROVE**
