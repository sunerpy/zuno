# F1 Plan Compliance Audit

## Verdict

**REJECT**

本次审计覆盖 `.omo/plans/opencode-rust.md` 中全部 **114 个实现 todo** 和 **18 条 Success criteria**。实现规模、常规测试和代码质量门禁总体健康，但计划要求的是 **18 条全部满足且 F1-F4 全部 APPROVE**；当前有 11 条不满足，其中实际用户配置、HTTP API、会话双向续写、指定 JS 插件版本、Goal 双压缩、prune 表数、G1-G6 证据链和 divergence 完整性均存在可复核缺口。

统计：**SATISFIED 7 / NOT SATISFIED 11 / UNVERIFIABLE 0**。

## Audit scope and method

- 审计工作树：`/config/workspace/ProdDir/AI/oc-wt/tF1`，分支 `task-F1`。
- 只写本报告；未修改被审计源码、测试、计划、证据或文档，未 commit/push/merge。
- 计划清单解析结果：114 个已勾选实现 todo，编号 1-114 各出现一次；18 条 Success criteria。
- Git 历史有 368 个 commit；102/114 个 todo 的声明 subject 可精确匹配，另外 12 个（61、65、66、74、76、77、80、81、83、84、87、91）存在语义对应但 wording/scope 不同的实现 commit。未发现完全没有实现 commit 的已勾选 todo。
- `.omo/evidence/` 有 106 个 tracked task evidence 文件；缺少 todo **52、60、101、109、110、111、113、114** 的约定证据文件。
- 仓库没有可用 `.codegraph/` 索引；`codegraph_explore` 明确返回 “No indexed project found”，因此按 CodeGraph fallback 规则读取源文件与测试。
- 未重跑约 100 分钟的 G1/G2 或 2 小时的 G3/G4；按任务要求审计 committed measurement records。

## Blocking findings

### B1 — 实际用户配置不兼容（criterion 2）

对 `/config/.config/opencode/opencode.json` 的实际检查结果不一致：

- Rust `debug config`：exit 1，`config file /config/.config/opencode/opencode.json failed validation (1 issue(s))`。
- TypeScript `debug config`：exit 0。

因此无法得到 normalized byte-identical merged config，也无法继续证明完整 skill/agent tree parity。这是直接运行失败，不是文档或测试覆盖不足。

### B2 — 上游 `/api` operation set 未完整提供（criterion 4）

`crates/oc-testkit/tests/compat_suite.rs:71-82,125-131,693-740` 明确把以下两个上游 operation 记录为 known gaps：

- `GET /api/event`
- `GET /api/session/{sessionID}/event`

同一测试确认上游 58 个 operation 中仅服务 56 个，并额外加入 2 个 C8 operation。criterion 4 要求 **every path+method** 都存在并验证行为，故不能以 equivalent `/event` 路径替代。

### B3 — G1-G6 的 committed evidence chain 不支持“全部通过”（criterion 15）

- `.omo/evidence/task-88-opencode-rust.txt` 是现存的 G1/G2 gate evidence：G1 PASS，G2 **FAIL**，G2 Rust median `3,249,508 KiB`，高于 frozen ceiling `1,513,496 KiB`。
- 计划 `opencode-rust.md:1003` 后来记录 G2 PASS（median `1,494,236 KiB`），但约定的 `.omo/evidence/task-113-opencode-rust.txt` 不存在。
- `.omo/evidence/task-114-opencode-rust.txt` 也不存在；todo 114 的计划文字说明没有重跑 100 分钟 gate。
- `.omo/evidence/task-89-opencode-rust.txt` 支持 G3/G4 PASS；`.omo/evidence/task-90-opencode-rust.txt` 支持 G5/G6 PASS。

在“现存正式 gate evidence 明示 G2 FAIL、后续 PASS 仅见于计划文字且约定 evidence 缺失”的状态下，不能判定 G1-G6 全部通过。

### B4 — intentional divergences 未全部进入 allow-list（criterion 17）

`docs/divergences.toml` 有 8 项，且 docs/compat tests 对这 8 项通过；但 `crates/oc-testkit/tests/compat_suite.rs:1055-1100` 又列出 **6 个明确的 nominated divergences**，并说明它们在 plan count 之外：

1. `subpath-is-implemented`
2. `subpath-matches-literally`
3. `context-md-excluded`
4. `malformed-auth-json-is-an-error`
5. `failed-format-restores-pre-format-bytes`
6. `memory-subsystem`

criterion 17 要求 **every intentional divergence** 都在 allow-list、divergence page 和 docs test 中。把它们放在 report-only nomination 列表不满足该要求；其中 `memory-subsystem` 还是 todo 103 明确要求加入 allow-list 的项目。

### B5 — 多项精确 acceptance contract 被较窄测试替代

- criterion 5 要求同一 existing session 在 TS→Rust 与 Rust→TS 两个方向均能 list/open/continue/export；现有 compat registry 只声明 `session-rows` 和 `message-export` 的 Rust-written→real-binary 方向，没有完整双向 continuation 测试。
- criterion 6 固定要求 `@sunerpy/opencode-kiro-auth@0.18.0`、`client.middlewareStack.add` 和 `models --format json`；`crates/oc-plugin/tests/js.rs:279-320` 实际加载 `0.20.1`，只检查 auth provider 与 SDK report，不检查 middlewareStack 调用或 models CLI 输出。
- criterion 7 要求 Rust/WASM plugins 与“those JS plugins”（criterion 6 的两个真实插件）同载；显式 WASM integration 测试使用自建 `integration-js` fixture，而非这两个插件。
- criterion 11 要求 goal 经历 **two compactions**；`oc-goal` 的可定位测试只证明一次 compaction 后从 SQL 重新注入，没有两次压缩的端到端测试。
- criterion 13 明定十二张 related tables；`crates/oc-db/tests/prune.rs:542-559` 反而断言 `PRUNE_TABLES.len() == 10` 并注明 “the plan's 12-table count is stale”。在计划未修订前，10 不能满足 12。

## Success criteria matrix

| # | Status | Evidence and exact check |
|---:|---|---|
| 1 | **SATISFIED** | `cargo test -p oc-testkit --test compat_suite --offline -- --nocapture`：8 passed。`journal_round_trip_through_the_real_binary_does_not_replay_migrations` 用 real binary 打开 Rust-created DB，前后保持 38 migration ids；workspace tests 同时覆盖 migration ceiling/open behavior。 |
| 2 | **NOT SATISFIED** | 实际 `/config/.config/opencode/opencode.json`：Rust exit 1 validation error，TypeScript exit 0；无法产生 byte-identical merged config。 |
| 3 | **NOT SATISFIED** | `crates/oc-cli/tests/differential.rs::every_headless_command_keeps_the_oracle_long_option_surface` 比较 29 个 help/flag surface，另只比较部分命令输出；没有“每个 implemented command 的 normalized behavior/output”全矩阵。`surface_every_upstream_command_has_exactly_one_disposition` 确认 23 个 disposition，但只满足后半句。 |
| 4 | **NOT SATISFIED** | compat suite 明确报告 58 个上游 operation 中缺少 2 个 SSE operation；criterion 要求全量 path+method 和行为矩阵。 |
| 5 | **NOT SATISFIED** | `compat_suite` registry 有 session rows 和 message export，但未提供同一 existing session 的 TS↔Rust 双向 list/open/continue/export 端到端测试。 |
| 6 | **NOT SATISFIED** | `oc-plugin/tests/js.rs::js_real_supported_plugins_load_with_their_own_sdk_clients` 使用 Kiro `0.20.1` 而非 `0.18.0`；未证明 `client.middlewareStack.add`；未通过 `models --format json` 证明 provider 出现。 |
| 7 | **NOT SATISFIED** | `cargo test -p oc-plugin --features wasm --test integration --offline -- --nocapture`：6 passed，证明三 tier 顺序和单 tier 故障隔离；但 JS tier 是自建 fixture，不是 criterion 6 的两个真实 JS plugins，因此精确 contract 未满足。 |
| 8 | **SATISFIED** | workspace lint `unsafe_code = "forbid"`；workspace safety tests 通过；`cargo clippy --workspace --all-targets --offline` 0 warning/0 error。 |
| 9 | **SATISFIED** | `crates/oc-plugin-sdk/tests/conformance.rs::reusable_conformance_suite_checks_declared_tools_and_hooks` 通过，example Rust plugin 注册 tool/hook；workspace suite 通过 example plugin host tests。 |
| 10 | **SATISFIED** | `oc-agent/src/builtin/tests.rs::every_agent_states_every_column`、`no_agent_names_a_model`、model-policy tests、`oc-tools/tests/task.rs::an_explicit_model_and_effort_reach_the_childs_outbound_request`、`oc-agent/src/continuation/tests.rs::a_task_id_continues_the_same_session_and_its_message_count_grows` 均在 workspace suite 中通过；schema 覆盖 agent/model/effort/category/background/task_id。 |
| 11 | **NOT SATISFIED** | objective/counters、idle guards、status ownership、Markdown objective/status edits 均有 passing tests；但只找到 `goal_is_regenerated_from_sql_after_compaction_discards_old_context` 的单次 compaction，未找到“survives two compactions”测试。 |
| 12 | **SATISFIED** | `crates/oc-cli/tests/differential.rs::session_list_all_projects_matches_the_experimental_endpoint_on_one_database` 在同一 DB 上比较 real `/experimental/session` 与 Rust CLI 的 roots/children/archived 集合及 JSON text；workspace suite 通过。 |
| 13 | **NOT SATISFIED** | preview/confirmation/subtree/liveness/shared safety 有 passing tests；但 `prune_delete_order_and_true_related_table_count_are_pinned` 明确断言 10 张表并称计划的 12 已过时，和 criterion 的十二张表要求冲突。 |
| 14 | **SATISFIED** | `session_prune_with_a_selection_reclaims_its_unreferenced_snapshot_store` 证明仅删除 unreferenced snapshot；`vacuum_a_prune_alone_reclaims_nothing_and_an_explicit_vacuum_reclaims_bytes` 证明 explicit vacuum 报告 reclaimed bytes；`vacuum_refuses_when_free_disk_is_under_the_database_size` 证明空间不足拒绝。 |
| 15 | **NOT SATISFIED** | G3/G4、G5/G6 committed evidence 为 PASS；但 tracked task-88 evidence 的 G2 是 FAIL，task-113/task-114 evidence 缺失。不能以计划内后写数字替代约定 evidence。 |
| 16 | **SATISFIED** | MCP 有 real codegraph/public remote counterpart tests；LSP 有 `typescript-language-server` 与 `rust-analyzer` live tests；ACP `live_sdk.rs` 使用 real `@agentclientprotocol/sdk` 0.21.0；provider tests 使用 recorded real traffic cassettes。workspace suite 通过，G3 evidence 还记录两个 real LSP processes。 |
| 17 | **NOT SATISFIED** | `cargo test -p oc-cli --test docs --offline -- --nocapture`：11 passed；`docs/divergences.toml` 的 8 项和 reason 被检查。但 compat suite 自己列出 6 个 allow-list 外的 intentional/nominated divergences，违反“every intentional divergence”。 |
| 18 | **NOT SATISFIED** | F1 本报告为 REJECT；计划 `opencode-rust.md:1108-1111` 中 F1-F4 仍均未勾选，也没有 F2-F4 APPROVE 结果。 |

## Todo ledger findings (1-114)

### Commit coverage

- **114/114** 个已勾选 todo 均能映射到实现 commit。
- **102/114** 的 commit subject 与 todo 声明精确匹配。
- **12/114** 为语义匹配但 subject wording/scope 不同：61、65、66、74、76、77、80、81、83、84、87、91。此项本身不是 blocker，因为对应实现存在；它降低了机械可追踪性。

### Evidence coverage

- Tracked task evidence：106 个。
- 缺失约定 evidence：52、60、101、109、110、111、113、114。
- 其中 **113/114 是 blocker**：它们承载从 task 88 的 G2 FAIL 到最终 G2 PASS、以及 W-real subject pinning 的关键性能论证。
- task 101 是 background reflection/negative-list 的关键新增能力，只有测试结果而无约定 task evidence；这不单独推翻 workspace test，但违反计划自己的 evidence contract。

### Checked todos whose written acceptance is not met as written

1. **Todo 88**：其 committed evidence 明示 G2 FAIL；不能作为完成的全部 G1/G2 gate。
2. **Todo 113**：计划中记录后续 G2 PASS，但约定 evidence 文件缺失。
3. **Todo 114**：acceptance criterion 写的是 methodology hash 在“new revision”通过、在 old revision 失败；完成说明却选择保持 revision 2。该设计选择可能合理，但 todo 的 acceptance text 没有同步修订，且 evidence 文件缺失。
4. **Todo 103 / criterion 17**：memory subsystem 被 compat report 列为 allow-list 外 nomination，而 todo 明确要求加入 todo 86 的 divergence allow-list。
5. **Todo 82/criterion 13**：实现把真实 related-table count 固定为 10，而 Success criterion 仍要求 12；计划与实现未收敛到同一可执行断言。

## Commands run and results

| Command | Result |
|---|---|
| `cargo test -p oc-testkit --test compat_suite --offline -- --nocapture` | PASS — 8 passed, 0 failed |
| `cargo test -p oc-cli --test docs --offline -- --nocapture` | PASS — 11 passed, 0 failed |
| targeted methodology tests | PASS — 4 passed, 0 failed; `PERF_METHODOLOGY_REVISION = 2` and formula hash intact |
| `cargo test --workspace --offline` | PASS — 201 result groups, 3,214 passed, 0 failed, 2 ignored, 0 measured, 0 filtered |
| `cargo clippy --workspace --all-targets --offline` | PASS — 0 warnings, 0 errors |
| `cargo fmt --all --check` | PASS |
| `cargo metadata --locked --offline --format-version 1` | PASS — 520 packages, 36 workspace members, 520 resolve nodes |
| `cargo test -p oc-plugin --features wasm --test integration --offline -- --nocapture` | PASS — 6 passed, 0 failed |
| Rust vs TypeScript actual `debug config` | FAIL parity — Rust exit 1; TypeScript exit 0 |

`cargo test --workspace` 的两个 ignored 项是 2 小时 real-driver soak `g3_and_g4_real_driver_soak_stays_bounded_and_live` 和一个 doc test。审计按要求没有重跑 soak，使用 `.omo/evidence/task-89-opencode-rust.txt` 的 committed result。

## Required remediation before F1 can approve

1. 让 Rust binary 成功读取实际 `/config/.config/opencode/opencode.json`，并新增 actual-config + full skill/agent tree differential。
2. 在 upstream-compatible 路径实现并行为验证两个缺失 SSE operations，或正式修订 Success criterion（当前文本不允许缺失）。
3. 增加完整 CLI behavior matrix，不能用 help option parity 代替全部 implemented command output parity。
4. 增加同一 session 的 TS↔Rust 双向 list/open/continue/export 测试。
5. 按 criterion 6 的固定版本与行为验证真实 auth plugins；明确解决 stale `middlewareStack.add` 要求，而不是静默换成 `0.20.1`。
6. 用两个真实 JS auth plugins 运行 Rust + WASM + JS 三 tier integration，保留每 tier 故障隔离断言。
7. 增加 goal 连续两次 compaction 的端到端测试。
8. 统一 prune 的 10/12 table contract：修订计划或实现，使唯一权威断言一致。
9. 补交 task-113/task-114 committed evidence；正式证据必须支持同一 pinned subject 下 G1/G2 PASS，并保留 frozen methodology/hash。
10. 将 6 个 nominated intentional divergences 纳入 allow-list/docs/asserted count，或逐项消除；特别是 todo 103 的 memory subsystem。
11. 完成 F2-F4 并取得 APPROVE，再重跑 F1。

## Final decision

常规工程门禁全绿并不能覆盖计划中明确写出的兼容性和非功能 gate。当前 artifact 对多个缺口有诚实披露，这是优点；但已披露 gap 仍然是 gap，不能计作满足。因此 F1 不批准。

**F1 VERDICT: REJECT**
