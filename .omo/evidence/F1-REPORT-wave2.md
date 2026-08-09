# F1 Plan Compliance Audit — Second Wave

## Verdict

**REJECT**

This second-wave audit reviewed all **123 checked implementation todos**, all **18 success criteria**, the four blockers from the first F1 report, the final-wave remediation todos 115–123, the declared-divergence count, the closed workspace roster, and the corrected destructive-prune table count. The remediation work is substantial and the ordinary Rust quality gates are green. In particular, the original config parse failure, the two missing SSE routes, the broken G1/G2 evidence chain, and the undeclared-divergence structure were all repaired.

The project nevertheless does not meet the plan as written. Seven success criteria are satisfied and eleven are not: **SATISFIED 7 / NOT SATISFIED 11 / UNVERIFIABLE 0**. The remaining failures are executable contract failures or missing required proof, not style preferences. The most material are: the compatibility suite is pinned to the 1.18.12 executable rather than the criterion's 1.18.13 executable; the non-pure real-user config output is not byte-identical and does not contain the same plugin-expanded agent/command trees; 45 of 58 upstream API operations remain explicit `503 backend_unavailable` gaps; full CLI-output parity is not covered; bidirectional session continuation is not proved; the exact JS plugin/version contract is not implemented; the three-tier test uses a synthetic JS fixture; the goal is tested across one rather than two compactions; the criterion still says twelve prune-related tables while the audited schema has ten; native Windows G6 behavior was not executed; and F2–F4 still have only their earlier `REJECT` reports.

## Audit scope and constraints

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF1`
- Branch: `task-F1`
- Audited HEAD: `3d68d7a93b110f000a702537009c63f11c500122`
- The worktree was clean before this report was created.
- The only file created by this audit is `F1-REPORT.md`. No source, test, plan, evidence, generated documentation, commit, branch, or remote was modified.
- The approximately 100-minute G1/G2 measurement and two-hour G3/G4 soak were not rerun. Their committed records were audited as required.
- CodeGraph was unavailable for this worktree (`No indexed project found`), so targeted source, test, plan, evidence, and Git-history inspection was used.
- The compatibility target hard-codes `/config/.local/share/mise/installs/opencode/1.18.12/opencode` as its executable oracle and `.omo/fixtures/oracle-openapi-1.18.12.json` as its OpenAPI capture, while separately reporting plugin compatibility/source version 1.18.13.

## Executive findings

### A. The four original F1 blockers

| Original blocker | Second-wave result | Audit conclusion |
|---|---|---|
| B1 — real config exited 1 | **Parse failure fixed** by todo 117. Both binaries now exit 0. | **The blocker is only partially closed.** Todo 117's committed differential sets `OPENCODE_PURE=1`, explicitly disabling the real plugins that populate the oracle's full trees. The audited non-pure live outputs remain unequal: Rust normalized SHA-256 `719d557e34d63d353218e35966241d7651b6dd85d6d368e8c12bb57210f02e2e`; released output SHA-256 `1e527a3e849a30d90ef7576260a004f44d6a6659489a5d7dccb27a435958d274`. Rust has empty `agent`/`command` data and no `plugin_origins`; the released output has a 221,818-byte `agent` tree and a 17,970-byte `command` tree. Criterion 2 is still false. |
| B2 — missing `/api/event` and per-session SSE | **Closed as a routing defect** by todo 118. Both routes are served and tested. | **The original two-route blocker is fixed, but criterion 4 remains false.** The matrix itself reports only 13 operations with local backends and 45 operation-specific `503 backend_unavailable` gaps. Only five operations compare all three dimensions exactly; the other 53 rows carry exemptions. Accounting honestly for a gap is not implementing upstream behavior. |
| B3 — committed G1/G2 evidence ended in G2 FAIL | **Closed as an evidence-chain defect.** Todo 122 committed the honest failure; todo 123 diagnosed the aggregate-first startup-compaction allocation and committed a fresh frozen-gate PASS. | G1 and G2 now have auditable passing measurements. Criterion 15 still lacks native Windows G6 execution, and the README still publishes the older G1/G2 figures rather than todo 123's final values. |
| B4 — nominated divergences outside the allow-list | **Closed** by todo 119. | `docs/divergences.toml` has 12 entries, `DECLARED_COUNT` is 12, every recorded behavioral difference resolves to an entry, every entry has a reason, and named behavioral assertions must exist and not be ignored. Criterion 17 is satisfied. |

### B. Remaining blocking compliance findings

1. **The pinned executable is not the executable named by criterion 1.** Criterion 1 requires a journal round-trip through real `opencode 1.18.13`. `compat_suite.rs` hard-codes the installed 1.18.12 executable and a 1.18.12 OpenAPI capture. The suite is green for that pair, but a test against 1.18.12 cannot establish the explicitly named 1.18.13 contract.
2. **The full real-user config/tree comparison is still absent.** Todo 117 fixed the narrow legacy-TUI-key parse issue and retains strict rejection for other unknown keys. Its `real-user-global-config` differential is deliberately pure, however, so it excludes the plugin-generated agent, command, skill, and origin data the success criterion expressly includes. The live non-pure normalized outputs differ.
3. **The API matrix is an honest gap inventory, not full behavior parity.** All 58 upstream path+method pairs are present and invoked, and a `501` can no longer pass. Nevertheless, 45 return local `503 backend_unavailable`; eight more backed rows cannot be seeded for exact cross-process comparison; only five rows compare status, normalized body, and side effect exactly. Criterion 4 says every operation “exists here and behaves,” not merely that every operation is invoked and classified.
4. **Exact user-facing compatibility contracts remain narrower than their criteria.** The CLI registry compares 29 help surfaces and selected command outputs, not every implemented command's normalized behavior. Session tests cover important list/export/import directions but not one existing session being listed, opened, continued, and exported in both TS→Rust and Rust→TS directions. The JS host supports Kiro auth `0.20.1`, not the required `0.18.0`, and the real-plugin test neither asserts `client.middlewareStack.add` nor runs `models --format json`. The three-tier coexistence test uses `integration-js`, not the two required real auth plugins.
5. **Goal and prune acceptance text remain unsatisfied as written.** The goal continuation test proves regeneration after one compaction; no test drives two consecutive compactions while preserving objective and counters. The actual schema has ten session-attributable prune tables and the implementation correctly pins ten, but criterion 13 still says twelve. Correcting an inaccurate source count is defensible engineering; it is not a plan amendment, so F1 cannot silently rewrite the frozen criterion.
6. **G6's Windows half remains documented but unexecuted.** Linux clean shutdown, parent `SIGKILL`, interactive PTY reads, and terminal `Ctrl-C` behavior are tested. Todo 121 added a `cfg(windows)` natural-parent-exit/live-grandchild test and fixed explicit Job Object termination, but its evidence explicitly says that test was not run on Windows. Criterion 15's cross-platform containment statement therefore lacks an executed Windows result.
7. **Final verification cannot approve itself.** The only tracked F2, F3, and F4 reports are the earlier reports, each ending in `REJECT`. They predate todos 115–123 and were not rerun. Criterion 18 requires F1–F4 all to approve and the results to be surfaced for explicit user acceptance.

## Success criteria matrix

| # | Status | Evidence and decision |
|---:|---|---|
| 1 | **NOT SATISFIED** | Schema and journal round-trip tests pass against the real installed **1.18.12** binary; Rust-written `session.model` now uses `id`, and the release lists it. The criterion names real **1.18.13**, while `compat_suite.rs:63-74` hard-codes 1.18.12. The max-known-migration refusal behavior is tested, but the named version pair is not. |
| 2 | **NOT SATISFIED** | Todo 117 fixed the `theme`/`keybinds`/`tui` parse seam and preserved strict validation. Its byte-exact differential uses `OPENCODE_PURE=1`, excluding plugin expansion. The actual non-pure normalized outputs have different hashes and tree content; the checked-in fixture also differs from the live file (live SHA-256 `502ca4db55e63d958be28bb7ed9b2d687a9a6f2eca84442df37dd8e7245336c6`, fixture SHA-256 `33c8e02fff4549853e5354ebc8745eb548f9b5f5ff35352c9befa401ac7a4137`). |
| 3 | **NOT SATISFIED** | Every upstream command has one of 23 dispositions, and export now has a real handler with byte-identical seeded output. The registered CLI comparison still covers 29 help/long-option surfaces plus selected db/models/paths/config/session/export outputs. It does not execute and compare normalized behavior/output for every implemented command. |
| 4 | **NOT SATISFIED** | The generated surface is now a required superset: 58 upstream operations plus exactly two C8 operations. Both SSE routes work, and equal `501` observations are rejected. But only 13/58 operations have local backends; 45 are explicit 503 gaps. Five rows have exact three-dimensional live comparison, while 53 rows/159 dimensions are exempt. This is transparent partial compatibility, not “every path+method ... behaves.” |
| 5 | **NOT SATISFIED** | Rust→TS listing is covered by `rollback.rs`; Rust-written every-part export is decoded by the release; Rust export/import and release re-export are covered. Rust can read TS-created databases in manual QA. No end-to-end test proves the same existing session is listed, opened, **continued**, and exported in both directions. Internal `task_id` continuation is a different contract. |
| 6 | **NOT SATISFIED** | `SUPPORTED_JS_PLUGINS` pins antigravity `1.6.0` and Kiro `0.20.1`; the real-plugin test uses those versions and returns early if either cache directory is absent. The criterion requires Kiro `0.18.0`, successful `client.middlewareStack.add`, and provider visibility through `models --format json`. No source/test assertion for the latter two was found. |
| 7 | **NOT SATISFIED** | Rust, WASM, and JS fixture tiers coexist, preserve configuration order, and isolate crashes/timeouts. The JS tier is the synthetic `integration-js` plugin, not the two real JS auth plugins required by criterion 6, so the exact coexistence contract is unproved. |
| 8 | **SATISFIED** | Workspace lint policy forbids unsafe code; the source-policy test covers first-party `crates/*/src/**`; all-target Clippy passes with warnings denied. |
| 9 | **SATISFIED** | The example Rust JSON-RPC plugin registers tools and hooks and passes the reusable conformance suite without JavaScript. |
| 10 | **SATISFIED** | Built-in agent tests pin negative delegation boundaries, temperature, permissions, and output envelopes; no model literal is allowed in `oc-agent`; model inheritance/override behavior and all `task` selection/continuation fields are covered. |
| 11 | **NOT SATISFIED** | Objective/counter persistence, status ownership, idle-transition guards, and Markdown objective/status behavior have focused tests. `goal_is_regenerated_from_sql_after_compaction_discards_old_context` exercises one compaction. No two-compaction test or flow was found. |
| 12 | **SATISFIED** | `session list --all-projects` is compared against the real global/experimental endpoint on one database, including project summaries and the session set. |
| 13 | **NOT SATISFIED** | Preview, explicit confirmation, descendant closure, shared/mid-compaction exclusions, liveness handling, transactionality, and zero-orphan checks are implemented. The authoritative source defines `PRUNE_TABLES: [&str; 10]`; the criterion still requires “twelve related tables.” The corrected real count is ten, but the criterion was not amended. |
| 14 | **SATISFIED** | Artifact GC preserves referenced snapshots and removes unreferenced stores; vacuum is explicit, reports reclaimed bytes, performs integrity checks, and refuses when free space is insufficient. |
| 15 | **NOT SATISFIED** | Committed evidence now supports G1 PASS and G2 PASS under the unchanged revision-2 formulas; G3/G4 have a 500-turn, 7,200-second real-driver PASS; G5 production-channel behavior and Linux G6 clean/`SIGKILL` paths pass. Native Windows Job Object behavior was not executed, although the source and a `cfg(windows)` test exist. README's G1/G2 numbers are also stale after todo 123. “G1–G6 all pass” is therefore not fully established on the cross-platform mechanism the criterion names. |
| 16 | **SATISFIED** | MCP uses real server counterparts, LSP uses `rust-analyzer` and `typescript-language-server`, ACP uses the real `@agentclientprotocol/sdk`, and provider decoding is exercised with recorded real-traffic cassettes across wire families. |
| 17 | **SATISFIED** | There are exactly 12 intentional-divergence entries with reasons, generated documentation, count pinning, live behavioral assertion references, and an inverted gate that fails when a recorded difference is undeclared. The execute schema is checked against its declared contract. |
| 18 | **NOT SATISFIED** | F1 is this `REJECT`. F2, F3, and F4 remain tracked as earlier `REJECT` reports, and the final-wave checkboxes are unchecked. No explicit user okay exists. |

## Todo ledger audit — all 123 checked todos

### Mechanical completeness and traceability

- The plan contains **123 checked implementation todos**, with every numeric id 1–123 present exactly once despite wave-order numbering.
- **123/123** checked todos map to substantive implementation commits.
- **111/123** have an exact declared commit-subject match.
- The remaining **12/123** have semantic implementation matches but non-exact subject wording/scope: **61, 65, 66, 74, 76, 77, 80, 81, 83, 84, 87, 91**.
- Final remediation todos **115–123** all have tracked evidence artifacts. Todos 122 and 123 correctly preserve both the honest G2 regression and the later measured fix rather than replacing the failure record.
- The older per-task evidence gaps remain for **52, 60, 101, 109, 110, 111, 113, and 114**. Corresponding implementation commits/tests exist, so this is not an assertion that those capabilities are absent; it is a traceability defect. The critical G1/G2 result is no longer dependent on the lost 113/114 worktree artifacts because 122/123 now provide tracked measurements.

### Checked todos whose acceptance is not met as written

| Todo | Finding |
|---:|---|
| 1 | Its local acceptance sentence still says exactly 33 named crates. The deliberately amended current roster is 36. Todo 119 corrected the roster source of truth and added a bidirectional gate, but the old todo sentence remains stale. |
| 52 | It says all 58 `/api/*` endpoints are implemented. All 58 paths exist, but 45 operations intentionally answer `503 backend_unavailable`; the compatibility report correctly calls them gaps. |
| 56 | It says the headless command set has differential parity on every implemented command. The present matrix proves option/help surface and selected command output, not every implemented command behavior. |
| 117 | It says the real config's normalized merge is identical. The pure parse/merge case is identical, but the non-pure real output and full plugin-expanded trees are not. |

Other exact success-criterion failures are cross-todo composition failures rather than evidence that their owning implementation todo did nothing: criterion 5 requires a combined bidirectional session lifecycle not owned by one existing todo; criteria 6–7 add exact plugin-version and real-plugin coexistence requirements beyond the synthetic-tier test; criterion 11 strengthens todo 68's one-compaction acceptance to two compactions; criterion 13 retains a stale twelve-table count even though todo 82's own preview/confirmation acceptance passes with the real ten-table schema; and criterion 15 requires an executed cross-platform G6 conclusion beyond todo 121's explicitly permitted documented Windows gap.

### Final remediation todos 115–123

| Todo | Result |
|---:|---|
| 115 | **PASS.** Session `model` now writes `{id, providerID}` while message model keeps `{modelID, providerID}`; the real release lists a Rust-turn database. |
| 116 | **PASS.** `export` and `import` have real handlers; seeded export and sanitization match the oracle; disposition-to-handler consistency is mutation-tested. |
| 117 | **PARTIAL.** The narrow parse defect is fixed and invalid near-miss keys remain rejected. The pure fixture differential passes, but the full non-pure actual-config/tree criterion does not. |
| 118 | **PASS for its remediation contract; partial for criterion 4.** Both SSE operations are served, all 58 rows are invoked, and 501 cannot pass. The matrix visibly retains 45 backend gaps. |
| 119 | **PASS.** Twelve divergences, 36-crate roster, current README divergence count, and fail-closed gates are reconciled. |
| 120 | **PASS.** The turn-event gate now uses the production channel and fails under the dropped-send mutant; truncated error-body reads surface as transient typed failures. |
| 121 | **PASS for its written acceptance.** Interactive PTY reads and terminal signals work on Linux; Job Object cleanup is implemented and a Windows test exists, with non-execution honestly documented as the acceptance text permits. |
| 122 | **PASS.** Fresh tracked G1/G2 evidence records G1 PASS and the then-current G2 FAIL; lint suppressions are justified and policy-checked. |
| 123 | **PASS.** The startup-compaction aggregate-first allocation was diagnosed and removed; the frozen gate reports G1/G2 PASS, all five W-real runs pass, and median margin exceeds the five-run spread. |

## Corrected authoritative counts

| Surface | Audited count | Enforcement/evidence |
|---|---:|---|
| Checked implementation todos | 123 | Plan parse; ids 1–123 unique |
| Workspace crates | 36 | `crates.expected`; bidirectional comparison with locked/offline Cargo metadata |
| Intentional divergences | 12 | `docs/divergences.toml`; `oc_testkit::divergence::DECLARED_COUNT = 12` |
| Upstream `/api` operations | 58 | Committed 1.18.12 OpenAPI capture and behavior matrix |
| Added C8 API operations | 2 | Exact extra set: GET/POST `/api/session/prune` |
| Upstream API operations with local backends | 13 | Runtime behavior-matrix inventory |
| Explicit API backend gaps | 45 | Operation-specific `503 backend_unavailable` observations |
| Exact live API differential rows | 5 | health, session list, active sessions, global SSE, per-session SSE |
| Session-attributable prune tables | 10 | `PRUNE_TABLES` and `DELETE_ORDER`; source/schema tests pin the corrected count |
| G5 persistent bounded channels | 17 | Source-derived registry and per-channel behavior gates |
| G5 single-completion exclusions | 2 | Explicit bounded-growth exclusions |

The 10-table prune count is the correct count for the implemented schema: `session_context_epoch`, `session_input`, `session_message`, `todo`, `part`, `message`, `session_share`, `session`, `event_sequence`, and `event`. The compliance failure is the unreconciled criterion text, not a recommendation to invent two tables.

## Performance evidence audit

### G1/G2 — final frozen measurement, todo 123

- Methodology revision: 2, unchanged; methodology-hash tests pass.
- Pinned subject: `ses_2bcaee257ffeFZNJrmtpi3ZglR`, 931 messages, 3,620 parts, 105,118,812 part bytes.
- G1 Rust peaks: `[20,444, 20,504, 20,060, 20,356, 20,380]` KiB; median 20,380 KiB; ceiling 477,120 KiB; PASS.
- G2 Rust peaks: `[1,493,496, 1,493,948, 1,510,444, 1,494,024, 1,510,528]` KiB; median 1,494,024 KiB; ceiling 1,513,496 KiB; PASS.
- G2 median margin: 19,472 KiB; five-run spread: 17,032 KiB; margin exceeds spread by 2,440 KiB; every individual run is below the ceiling.
- Root cause: startup compaction retained complete provider-projected tool results for the whole owned transcript before truncation. The fixed path charges each complete message and immediately reduces retained tool output before projecting the next message.
- Documentation drift: README still reports G1 19,776 KiB, G2 1,494,236 KiB, and a 19,260 KiB margin, which are pre-todo-123 figures.

### G3/G4 — committed soak, todo 89

- 500 turns over 7,200 seconds.
- Final-half Theil–Sen RSS slope: 0.0001775568 MiB/turn, below 1.0 MiB/turn.
- Final/middle peak ratio: 0.9938255268, below 1.5.
- Two real language servers, a 50,000-file watcher, PTY output, tool output, and compaction were live.
- No 120-second meaningful-progress timeout or 1,800-second hard turn deadline fired.

### G5/G6 — committed records, todos 90, 120, and 121

- G5: 17 bounded persistent channels, two explicit single-completion exclusions, zero undeclared constructions; production turn-event backpressure is mutation-sensitive after todo 120.
- Linux G6: clean shutdown and parent `SIGKILL` each leave zero enumerated descendants across LSP, MCP, PTY, and plugin hosts.
- PTY containment now supports a real terminal read and terminal-generated `Ctrl-C` while retaining a separately killable process group.
- Windows Job Object cleanup is implemented and has a platform-gated test for a naturally exiting parent with a live grandchild, but the test was not run on native Windows. This is the remaining criterion-15 proof gap.

## Divergence and roster audit

### Divergences

The single allow-list contains exactly these 12 ids:

1. `session-list-default-sort`
2. `tool-output-filename-carries-session`
3. `no-eager-directory-creation`
4. `split-version-identity`
5. `execute-parameter-contract`
6. `c8-maintenance-endpoints`
7. `provider-coverage-by-wire-family`
8. `cross-session-resident-memory`
9. `session-subpath-is-applied`
10. `context-md-excluded`
11. `malformed-auth-json-is-an-error`
12. `failed-format-restores-pre-format-bytes`

The former `subpath-matches-literally` nomination is correctly merged into `session-subpath-is-applied`; `memory-subsystem` is correctly merged into `cross-session-resident-memory`. The compatibility gate fails if a recorded behavioral difference no longer resolves to a declared entry, if the count drifts, if a reason is empty, or if its named behavioral assertion disappears or becomes ignored.

### Workspace roster

`crates.expected` lists exactly 36 current workspace members, including the deliberately added `oc-process` and `oc-reaping-fixture`. `the_workspace_roster_matches_the_declared_crate_list` computes both set differences against locked/offline Cargo metadata, so silent additions and silent removals fail. The final roster correction is sound; only todo 1's old “33 named crates” sentence remains stale.

## Verification performed in this audit

One final chained engineering verification was run after the read-only audit and before writing this report:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test --workspace --offline
cargo build --workspace --offline
cargo metadata --locked --offline --format-version 1
```

Result: **PASS**. Formatting was clean; Clippy emitted no warning/error; all non-ignored workspace unit, integration, and doctest targets passed; the workspace build passed; locked/offline metadata resolution passed. The expected opt-in two-hour soak and one documentation example remained ignored. The expensive memory gate and soak were not rerun, by instruction.

Ordinary green gates do not override the explicit compatibility and evidence gaps above. In particular, several live-counterpart tests return early with an explicit skip when an external binary/cache is absent, and the compatibility registry itself labels the API surface `PartiallyCompared` and reports the 45 backend gaps.

## Required remediation before F1 can approve

1. Run criterion 1 against the exact real `opencode 1.18.13` executable named by the plan, or explicitly amend the pinned pair everywhere to 1.18.12.
2. Make the non-pure actual `/config/.config/opencode/opencode.json` output and full plugin-expanded skill/agent/command trees match after the declared normalization; do not substitute a pure-mode fixture for this criterion.
3. Execute every implemented CLI command in a deterministic matrix and compare normalized behavior/output, not only help flags and selected command families.
4. Implement the 45 upstream API operations that remain `503 backend_unavailable`, or amend the compatibility claim. Exemptions and gap classification should remain honest but cannot count as parity.
5. Add a bidirectional, same-session TS↔Rust list/open/continue/export end-to-end test.
6. Either support and test exactly `@sunerpy/opencode-kiro-auth@0.18.0` with `client.middlewareStack.add` and `models --format json`, or amend criterion 6 to the deliberately supported 0.20.1 contract. Then use both real auth plugins in the Rust+WASM+JS coexistence/fault-isolation test.
7. Add an end-to-end goal test spanning two consecutive compactions with objective and counters intact.
8. Amend criterion 13's stale twelve-table wording to the schema-derived ten-table set, or supply an approved schema argument for a different set. Do not add fictitious tables merely to meet a stale count.
9. Execute the Job Object containment tests on native Windows and retain the result as G6 evidence; update README to todo 123's final G1/G2 values.
10. Rerun F2, F3, and F4 against the post-remediation HEAD and obtain APPROVE results, then rerun F1 and surface all four results for explicit user acceptance.

## Final decision

Todos 115–123 repaired every concrete defect they directly targeted, and the codebase now has stronger mutation-sensitive gates, honest gap reporting, a closed roster, and a single enforceable divergence allow-list. Those improvements are real. They do not, however, make the broader frozen success criteria true: several exact end-to-end contracts were never implemented or never tested at the required version/platform, and the API remains a 13-backed/45-gap subset behind a complete path surface. F1 therefore cannot approve this artifact.

F1 VERDICT: REJECT
