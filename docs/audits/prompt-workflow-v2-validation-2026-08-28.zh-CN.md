# Zuno Prompt/Workflow Generation 2 验收审计

日期：2026-08-28

实现 worktree：`/tmp/zuno-prompt-workflow-v2`

分支：`codex/prompt-workflow-v2`

基线：`b1429cd54f38475f6f1ff74f7adb2fe6dcb586ec`

## 1. 结论

Generation 2 已按四个逻辑阶段落地。Zuno 保留 typed `PromptEnvelope`、稳定 section
来源与 digest、持久化 Goal/Plan/Todo/Job/Inbox 基础，在其上完成了：

- 六个稳定 runtime policy section；
- 收敛后的内置 Agent Prompt 与 Skill；
- typed `DelegationContract` 和宿主生成的 `TaskReportMetadata`；
- 完整 provider-visible Prompt 的总上下文预算门禁；
- 父子模型、reasoning、MCP、Skill、Sandbox 和工具 authority 交集；
- durable Plan、前后台统一逻辑任务去重、报告原子 admission、父会话
  wake/restart recovery；
- Goal completion barrier 与 uncertain Job authoritative reconciliation；
- `zuno debug prompt`、`zuno debug agent`；
- OpenAI 与 Compatible Responses 的 typed `response.failed` 投影；
- TUI、ACP 和 CLI 对 durable typed child report 的一致历史回放。

真实授权的 GPT 5.6 Sol 与 Claude Opus 5 已分别完成原子任务、深度 Debug、并行委派和
Plan-only 四类 E2E。没有使用 mock 冒充账户或 Provider 验证。

## 2. 固定设计来源

| 项目 | 固定 SHA |
| --- | --- |
| Codex | `a73bf25d17805b4169ba2a2dc4329a010a3bb120` |
| OpenCode | `1be9fd55a9326d5e7b09786195e5669e311e61b4` |
| oh-my-opencode-slim | `9dbf2de015aec093e44273e6411c1392705b2f4d` |
| oh-my-openagent | `64d89819ef1fde81712630f8e5d798be9e4e8867` |

参考仓库只用于 adopt/adapt/reject 审计。没有复制品牌、隐藏身份、完整 Prompt、
OAuth client identity 或项目 ID。每个参考仓库在隔离目录执行过一次
`codegraph init`；之后的普通更新只使用 `codegraph sync`。

Zuno 最终索引状态：

```text
version: 0.48.1
extractionStatus: current
files: 676
nodes: 25,814
edges: 85,657
pending: 0
```

## 3. Prompt 与能力合同

最终 canonical section lane 为：

1. Agent role；
2. collaboration mode；
3. runtime policy；
4. 全局、profile、项目 instruction；
5. durable work state；
6. routing、extension 和 Skill policy；
7. selected Skill；
8. Skill index；
9. memory。

稳定 runtime section 为：

- `runtime.intent`
- `runtime.execution`
- `runtime.editing`
- `runtime.verification`
- `runtime.delegation`
- `runtime.persistence`

它们在 request hook 完成工具收缩之后，根据最终 provider-visible 工具生成。Prompt
receipt schema 3 同时持久化 system/developer projection、实际 post-hook projection、
section source/digest/token estimate 和 capability snapshot identity。

真实 GPT Plan-only 第一步的 receipt：

```text
session: ses_0f02ad700f774e3fb564238f23d970f7
receipt: evt_01a048fec0f97ec08383af715c493ec6
provider request: evt_01a048fec1017280a9f8004f428edd0e
assembly/actual sha256:
  43975a49e0b65f028a67f59c584e8854b281c87101fc1593d6d7fa473c1a4cdc
capability snapshot sha256:
  6069ae83b8b8690ca428bd9b5b120ccc6da97702ea8eb74a83e0319cf36a9df7
```

`zuno debug prompt --session ... --step 1` 能从 provider request 反查精确 receipt；
默认内容脱敏。`zuno debug agent plan` 能显示 effective model/reasoning、工具可见性、
Skill、MCP 未连接状态、Sandbox 与来源，并明确没有 step limit。

## 4. 委派与持久化工作流

模型可见的 `task` 参数已原生替换为：

- 必填：`objective`、`deliverable`、`instructions`、`success_evidence`；
- 可选：`scope.include/exclude`、`constraints.must/must_not`、`dependencies`；
- 调度：`agent`、`background`、`reportDelivery`、`task_id`。

旧 `description`、`prompt`、`subagent_type`、`category`、模型和 Skill 覆盖参数直接
拒绝，不提供迁移层。

完成的持久化合同包括：

- foreground 与 background native task 都先创建 durable internal Job；新 child session
  与 Job admission 位于同一个 SQLite 事务，重复 logical task 会整体回滚，不留下
  orphan child；
- 同一 parent 中 active、uncertain 或 report 尚未消费的相同 logical task
  不能重复派发，foreground 也不能绕过去重；同一 provider attempt 内即使第一个
  foreground 已经 quiet terminal，后续串行重复调用仍被拒绝；
- Job terminal settlement、`nextStep` report input 和 metadata 在一个 SQLite
  事务内提交；
- Job 持久化本次执行的 `evidence_start_rowid`；恢复或继续既有 child 时，
  `TaskReportMetadata` 只采集该边界之后的 typed 工具证据，不混入之前轮次；
- `nextStep` 进入 durable FIFO，只产生一份有效 admission/wake；
- `quiet` 不创建 parent input，也不唤醒；
- restart 后 queued child 取消、失联 running child 变为 uncertain、已提交 report
  继续使用同一 input row；
- parent 目标在活跃后代 Job、未消费报告、未完成 Plan/Todo 或 uncertain Job
  存在时不能完成；
- `job_reconcile` 只能依据明确 authority/evidence 处理 uncertain 结果，原副作用
  不机械重放。

Compaction 每轮从同一 SQLite snapshot 重建有界 `runtime.work_state`，保留 Goal、
Plan、Todo、Job、未消费 report 和上一份 prompt receipt 引用。

## 5. 真实双模型 E2E

### 5.1 环境

- Provider：`kiro-local`
- Endpoint：`http://127.0.0.1:8787/v1`
- Surface：OpenAI Responses
- `maxTokens: null`
- Kiro Provider：
  - `protocol_projection_mode: legacy-user-prefix`
  - `session_affinity_mode: explicit-only`
- 权限：`allow_all`
- Sandbox：`danger-full-access`
- 测试数据库格式：4

API key 只从本地 Provider 配置注入子进程，没有写入报告或命令输出。Provider
request 事件确认 `affinityAttached=true`、`affinitySource=durable-session`。

### 5.2 结果矩阵

| 模型/场景 | Session | Provider calls | 主要工具 | 结果 |
| --- | --- | ---: | --- | --- |
| GPT 原子 | `ses_38d14c5fcea84a2f98a120b8bcab0dfe` | 1 | 0 | 精确返回 `ATOMIC_GPT_OK`；无 Plan、无委派。 |
| Opus 原子 | `ses_123731a1c59b41f4bb6bb180d0b2f3e0` | 1 | 0 | 精确返回 `ATOMIC_OPUS_OK`；无 Plan、无委派。 |
| GPT Deep | `ses_cea8658e74cb499aaccc8aee06542462` | 12 | read 3、shell 7、apply_patch 1、plan 6、skill 2 | 复现整数除法错误，修复 owning function，原测试和边界测试通过。 |
| Opus Deep | `ses_90888b73c51c44569fbe69037d76b834` | 10 | read 3、shell 7、apply_patch 1、plan 3 | 同一缺陷完成复现、根因修复和恢复验证；一次无害的临时路径 read 失败。 |
| GPT 并行委派 | `ses_7bde1902ce3446d79604767f5e8660b8` | 9 | task 2、plan 5、todo 5 | 两个只读 child scope 不重叠；parent 不读取目标文件；两份报告各 admission 一次。 |
| Opus 并行委派 | `ses_8f157ba4c303410eb58c51e652100d9a` | 3 | task 2 | 两个 child 并行完成；parent 只消费报告并集成。 |
| GPT Plan-only | `ses_0f02ad700f774e3fb564238f23d970f7` | 6 | read 2、plan 2、todo 4、goal 1 | 只读取两个授权文件；7 步 Plan、7 个显式 ID Todo；无编辑、无委派、无越界检索。 |
| Opus Plan-only | `ses_0669d0712f9f4f819c5dd5ad0cc3db5b` | 8 | glob 2、read 2、plan 3、todo 2 | 只匹配并读取两个授权文件；9 步 Plan、10 个显式 ID Todo；无编辑、无委派。 |

Provider call 数按 durable assistant message 统计；工具数按 canonical `part.data.tool`
统计。

### 5.3 并行 Job 证据

GPT parent：

```text
job_4cdda8d6ef004ee88d7c17dec7ec0e30
  child=ses_2605de6f7fb64cb896c0ae45a7a87cac
  report=input_2f6531e6f5024c47b68881d77ca363b7
job_7703f6d06d8847548b513776acde9120
  child=ses_dd0c875c239f444cb07041cad240f43e
  report=input_51829443ecfc43a9b313e68a66f5d69e
```

Opus parent：

```text
job_bdf1052207b045e88a80a0f8621db153
  child=ses_d32b6bf123c14996872ba926d2937992
  report=input_4dce7622e23c4fdf8d79047f409a10bc
job_85b7b5bf38bc47aa8c81ed0db96463ed
  child=ses_577071e0e91f4d788f4b73fe26920dcf
  report=input_fe85bf7bc5d44f57adf2046ef77ac7da
```

四个 Job 均为 `completed`、`next-step`，每个 `report_input_id` 在 JobStore 中恰好
出现一次。

### 5.4 E2E 暴露并修复的问题

1. 首轮 GPT Plan-only 会越过用户指定目录，读取 sibling、Git、Skill 和 CodeGraph。
   `runtime.intent` 与 `plan` Prompt 已增加 closed-scope 合同；复测只读取两个授权文件。
2. 模型在同一 Todo batch 中引用尚未分配的伪位置 ID。
   `todo_update` 描述现在要求先给所有 dependent add 分配显式稳定 ID。
3. `plan_update` 未向模型声明 pending/in-progress 不变量，导致一次可避免的失败。
   描述现在明确：有 pending 时必须恰好一个 `in_progress`；全完成时没有 active step。
4. Opus Plan-only 的一次真实 `response.failed` 被旧 decoder 降级为普通
   `MessageEnd(Error)`，丢失 Provider error body。OpenAI 和 Compatible Responses
   decoder 现在返回 typed `ProviderError` 并保留 type/code/message。

独立代码审查又暴露并修复了六个边界问题：

1. foreground delegation 未进入 logical task JobStore 去重路径。现在 foreground
   使用 attached、quiet 的 internal Job，和 background 共用同一个原子 admission；
   terminal Job 在同一 provider attempt 内仍阻止串行重复工具调用。
2. Compatible Responses 对没有 HTTP status、只有 `type` 的 `server_error`、
   `rate_limit_error`、`authentication_error`/`permission_error` 曾统一落入 fatal。
   现在分别映射为 transient、rate-limited 和 auth，并保留原始 type/code/message。
3. background 重复派发曾可能先创建 child session 再被 JobStore 拒绝。child 与 Job
   现在同事务创建，拒绝时不留下 orphan session。
4. 继续既有 child 时，报告证据曾扫描整个 child 历史。每个 Job 现在持久化
   `evidence_start_rowid`，只报告本次委派产生的 typed evidence。
5. selected Skill 有独立预算，但完整 Prompt 尚缺总门禁。现在 system/developer
   projection、历史、动态上下文与 tool schema 在 hook 完成后统一估算；已知模型
   context 不足时在 provider I/O 前 typed fail，不截断约束或静默删 Skill。
6. ACP 已投影 typed child report，但 TUI 数据库回放曾只保留可见文本。隐藏
   `ReplayData` 现在同时传递 `message.data.taskReport` 与 foreground tool 的
   `state.metadata.subagent.report`，子 Agent 页面恢复 status、final text、
   changed paths、verification、uncertain evidence 和 evidence errors，不再解析
   英文字符串。

## 6. 自动化验证

定向测试覆盖：

- Prompt section 确定性顺序、去重、source、digest、预算和能力条件；
- Agent Prompt 长度与职责边界；
- Skill 目录和 selected body 聚合预算；
- typed delegation schema、旧字段拒绝、能力交集和 required Skill；
- Task report metadata 的本次执行证据边界、ACP/TUI/CLI typed replay；
- foreground/background Job logical key、child+Job 原子 admission、重复回滚、
  settlement/admission、wake/restart、uncertain reconciliation；
- Goal completion barrier、descendant barrier、compaction snapshot；
- 完整 provider-visible Prompt 的 context-limit fail-closed 门禁；
- debug prompt/agent；
- OpenAI/Compatible `response.failed`，包括无 HTTP status 的 type-only 分类；
- Plan/Todo 工具契约描述。

最终 gate 结果：

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --workspace --all-targets` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `git diff --check` | 通过 |
| `cargo build --release` | 通过，35.17s |
| `cargo test --workspace` | 通过。沙箱内曾因禁止 loopback 监听而在 `zuno-auth` fixture 返回 `Operation not permitted`；宿主环境完整重跑成功，确认不是端口占用或产品断言失败。 |

最终增量对应的定向测试均通过：

```text
cargo test -p zuno-provider-openai
cargo test -p zuno-provider-compatible
cargo test -p zuno-tools --lib
cargo test -p zuno-cli --test docs --test prompts
```

其中 Compatible 全套、`zuno-auth` loopback 用例和最终 workspace gate 均在允许
loopback 的宿主权限下执行。端口占用会返回 `Address already in use`，本次沙箱错误为
`Operation not permitted`；宿主完整重跑通过，因此没有终止任何进程。

## 7. 后续修复状态

同日后续增量已处理上述五项：

1. 宿主在首个 provider request 前创建或维持 durable Plan；仅用户输入和已解析
   command 可以创建，child report、steering、retry 不会误建 Plan。active Plan
   继续维护；completed Plan 遇到新目标会保留旧 step 并追加新 epoch。仅直接回答、
   一次有界读取和一次短小的既有变更 commit 走原子路径；typed 附件、selection、
   branch diff 和大型多块文本进入 planned path。Todo 明确为可选细化。
2. CLI 接受并在 provider I/O 前校验 `--variant` 与 `--thinking`，两者互斥。
3. `debug agent` 复用正式 `McpRuntime`，连接 enabled servers、读取精确当前
   schema、评估权限并等待清理；连接后的 discovery 失败、取消和超时同样有界
   close。根诊断不伪造 parent Attempt authority。
4. 新增 `zuno debug sandbox --mode workspace-write --network deny --check`。宿主实测
   `/usr/bin/bwrap` 为 `uid=0/gid=0`、不可被 group/world 写入，core/network
   namespace 与 seccomp 探测通过，并实际通过 bubblewrap、capability drop、
   `PR_SET_NO_NEW_PRIVS`、seccomp 执行 no-op。外层 Codex 沙箱中的 `uid=65534`
   是 user namespace 映射视图，不再被误报为宿主部署结论。
5. `debug agent` 输出 Skill source/description/unique/ambiguous 数量、metadata 与
   selected-body budget、rendered/omitted/truncated 覆盖率和最多 50 项预览；
   `debug skill` 输出显式标记为 `raw_discovery` 的对象，其 `skills` 数组保留同名
   不同 source 的条目；effective Agent view 由 `debug agent <name>` 提供。

历史 provider request 的最终 MCP schema 仍必须从当时的 receipt/request 取证；
当前实时诊断不能替代历史法证。模型可能把宿主 seed Plan 精炼得过细，成本和延迟
仍需以后续真实遥测观察。
