# Zuno 提示词与工作流第二代设计审计

状态：Generation 2 已实现并完成真实双模型 E2E，2026-08-28；精确 session、工具
计数、Job/report 和剩余风险在最终验收报告中单独记录。

本文既保留实施前基线，也记录最终合同和当前实现边界。历史缺口会明确标注为
“实施前”，不能当作当前运行时状态。

## 1. 审计范围

本轮审计回答五个问题：

1. Zuno 已有的 typed Prompt、能力快照、持久状态和 Job 基础是否需要重做；
2. Codex、OpenCode、oh-my-opencode-slim、oh-my-openagent 中哪些设计值得采用；
3. Prompt、Agent、Skill、委派、Job、Plan、压缩和诊断应如何形成一套一致的
   Generation 2 合同；
4. 哪些当前实现仍会产生错误提示、重复任务、错误完成或不可恢复状态；
5. 四个实施阶段分别需要哪些失败测试、代码边界和验收证据。

不在本设计范围内：

- OpenCode、OMO 或旧 Zuno 参数兼容；
- 复制第三方完整 Prompt、隐藏身份、品牌、OAuth client identity 或项目 ID；
- 将 `dual-review`、`auto-release` 变成产品内置命令；
- 新增强制内置 Designer Agent；
- 用 Markdown 文件替代 SQLite 中的 Goal、Plan、Todo、Job 或 Inbox 真相。

## 2. 审计基线

### 2.1 Zuno

- 隔离 worktree：`/tmp/zuno-prompt-workflow-v2`
- 分支：`codex/prompt-workflow-v2`
- 基线提交：`b1429cd54f38475f6f1ff74f7adb2fe6dcb586ec`
- CodeGraph：0.48.1，`current`
- 索引规模：676 files、25,726 nodes、85,213 edges
- pending changes：0（开始文档编辑前）

索引已存在，后续常规更新只能执行：

```sh
codegraph sync /tmp/zuno-prompt-workflow-v2
```

不得为普通文件变更重复执行 `codegraph init` 或强制重建。

### 2.2 固定的上游参考

| 项目 | 固定 SHA | 主要公开证据 |
| --- | --- | --- |
| Codex | [`a73bf25d17805b4169ba2a2dc4329a010a3bb120`](https://github.com/openai/codex/commit/a73bf25d17805b4169ba2a2dc4329a010a3bb120) | [`prompt_with_apply_patch_instructions.md`](https://github.com/openai/codex/blob/a73bf25d17805b4169ba2a2dc4329a010a3bb120/codex-rs/core/prompt_with_apply_patch_instructions.md)、[AGENTS.md 指南](https://developers.openai.com/codex/guides/agents-md)、[多 Agent](https://developers.openai.com/codex/multi-agent) |
| OpenCode | [`1be9fd55a9326d5e7b09786195e5669e311e61b4`](https://github.com/anomalyco/opencode/commit/1be9fd55a9326d5e7b09786195e5669e311e61b4) | [`session/prompt.ts`](https://github.com/anomalyco/opencode/blob/1be9fd55a9326d5e7b09786195e5669e311e61b4/packages/opencode/src/session/prompt.ts)、[`session/system.ts`](https://github.com/anomalyco/opencode/blob/1be9fd55a9326d5e7b09786195e5669e311e61b4/packages/opencode/src/session/system.ts) |
| oh-my-opencode-slim | [`9dbf2de015aec093e44273e6411c1392705b2f4d`](https://github.com/alvinunreal/oh-my-opencode-slim/commit/9dbf2de015aec093e44273e6411c1392705b2f4d) | [`orchestrator.ts`](https://github.com/alvinunreal/oh-my-opencode-slim/blob/9dbf2de015aec093e44273e6411c1392705b2f4d/src/agents/orchestrator.ts)、[`designer.ts`](https://github.com/alvinunreal/oh-my-opencode-slim/blob/9dbf2de015aec093e44273e6411c1392705b2f4d/src/agents/designer.ts)、[`task-session-manager`](https://github.com/alvinunreal/oh-my-opencode-slim/tree/9dbf2de015aec093e44273e6411c1392705b2f4d/src/hooks/task-session-manager) |
| oh-my-openagent | [`64d89819ef1fde81712630f8e5d798be9e4e8867`](https://github.com/code-yeongyu/oh-my-openagent/commit/64d89819ef1fde81712630f8e5d798be9e4e8867) | [`agent-model-matching.md`](https://github.com/code-yeongyu/oh-my-openagent/blob/64d89819ef1fde81712630f8e5d798be9e4e8867/docs/guide/agent-model-matching.md)、[`sisyphus-agent-factory.ts`](https://github.com/code-yeongyu/oh-my-openagent/blob/64d89819ef1fde81712630f8e5d798be9e4e8867/packages/omo-opencode/src/agents/sisyphus-agent-factory.ts) |

固定 SHA 的目的，是让设计结论可复核，而不是持续追随上游内部实现。上游发生变化
时，应重新审计并更新 SHA；不得用浮动分支链接替代验收证据。

### 2.3 Zuno 本地证据索引

| 主题 | 当前证据 |
| --- | --- |
| Prompt section、render、receipt、trace 去重 | [`crates/zuno-engine/src/prompt.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-engine/src/prompt.rs) |
| Provider request 与 prompt receipt 提交时机 | [`crates/zuno-engine/src/loop.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-engine/src/loop.rs) |
| Agent profile 与 runtime policy 中间实现 | [`crates/zuno-agent/src/profile.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-agent/src/profile.rs) |
| Orchestration capability catalogue | [`crates/zuno-orchestration/src/snapshot.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-orchestration/src/snapshot.rs) |
| 当前 task wire schema | [`crates/zuno-tools/src/task.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-tools/src/task.rs) |
| Job settlement 与 pending reports | [`crates/zuno-db/src/job.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-db/src/job.rs) |
| Inbox queued/promoted/consumed 状态 | [`crates/zuno-db/src/inbox.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-db/src/inbox.rs) |
| Goal completion blocker | [`crates/zuno-goal/src/store.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-goal/src/store.rs) |
| debug CLI 参数与查询 | [`crates/zuno-cli/src/command.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-cli/src/command.rs)、[`crates/zuno-cli/src/cmd/debug.rs`](https://github.com/sunerpy/zuno/blob/main/crates/zuno-cli/src/cmd/debug.rs) |

## 3. 当前实现的真实边界

### 3.1 已有 Prompt 基础不重做

Zuno 已经具备：

- provider-neutral `PromptEnvelope`；
- 稳定 section id；
- section source、semantic role、trust class 和 priority；
- SHA-256、字节数和本地 token 估算；
- selected Skill provenance；
- `session.prompt.assembled.1` 持久 receipt；
- provider request 对 prompt receipt 的引用；
- Goal、Plan、Todo、Job、Inbox 等 typed durable state。

这些基础优于重新回到匿名字符串拼接，因此 Generation 2 只调整 producer、时机、
角色内容和生命周期，不重建 Prompt 系统。

### 3.2 `PromptAssembly` 使用稳定的语义顺序

Generation 2 实现后的 `PromptAssembly::push`：

- 拒绝空或包含换行的 id；
- 拒绝重复 id；
- 为内容计算 digest；
- 插入后按 semantic lane 做稳定 canonical sort。

`PromptAssembly::render` 按已排序的存储向量迭代，并用两个换行连接内容。相同 lane
仍保留 producer 的相对顺序；即使 runtime section 在 provider step 才加入 receipt
assembly，也会落在 collaboration 之后、用户 instruction 之前。

因此，正确合同是：

> 跨 lane 顺序由 `PromptAssembly` 的稳定语义排序保证；同一 lane 的细粒度顺序由
> producer 保证。

对应测试必须验证确定性插入顺序、重复 id 拒绝、source 和 digest，而不能仅验证
最终字符串。

### 3.3 Generation 2 已把 runtime sections 后移到最终工具子集

隔离 worktree 已将统一 policy 拆成：

1. `runtime.intent`
2. `runtime.execution`
3. `runtime.sandbox`（可信 fallback 生效时）
4. `runtime.continuity`（最终存在 History 或 Notes 时）
5. `runtime.editing`
6. `runtime.verification`
7. `runtime.delegation`
8. `runtime.persistence`

`AgentProfile` 只保留角色、权限交集、delegate allowlist、原生边界和 Shell 可写
事实，不再提前渲染 model-visible capability summary。每个 provider step 依次：

1. 获取 permission/MCP/extension 已过滤的 registry tools；
2. 应用 tool-definition hook；
3. 锁定本 step tool snapshot；
4. 允许 `prepare_request` 只做工具子集收缩；
5. 根据最终 `CompletionRequest.tools` 生成 runtime sections；
6. 以独立 developer context 发送，并写入 receipt。

`runtime.sandbox`、`runtime.continuity`、`runtime.editing`、`runtime.delegation` 和
`runtime.persistence` 按最终能力条件省略，
不会向模型描述已被 hook 或 provider 移除的工具。现有集成测试覆盖
`apply_patch + task + plan_update` 经 hook 收缩为仅 `plan_update` 的场景。

### 3.4 当前 `CapabilitySnapshot` 不是最终工具快照

现有 orchestration `CapabilitySnapshot` 主要包含：

- profiles；
- presets；
- councils；
- workflows；
- skills；
- extension revision；
- permission policy digest。

它适合父子会话能力目录和漂移检查，但不等同于 provider request 已锁定的最终
ToolManifest。Generation 2 需要在保留该目录快照的同时，增加 attempt 级有效能力
投影，至少包含：

- 最终工具 wire id 与 schema digest；
- MCP server/tool 来源；
- selected/required Skill；
- Agent delegate allowlist；
- sandbox mode、filesystem roots 和 network policy；
- provider/model、reasoning、vision、structured-output 能力；
- 权限和 extension revision 来源；
- snapshot identity。

### 3.5 审计基线中的 task schema 是松散文本

实施前的 `TaskParams` 暴露：

- `description`；
- `prompt`；
- `subagent_type` / `category`；
- `model` / `effort`；
- `background`；
- `reportDelivery`；
- `task_id`；
- 一个仅用于拒绝兼容的隐藏 `load_skills` 字段。

这会把目标、交付物、范围、约束和证据混在自然语言中，也迫使 Council 和 Workflow
构造完整 `TaskParams` 只为复用模型解析。Generation 2 已删除旧参数形状和隐藏兼容
字段，并把 host-owned 模型路由请求与模型可见委派合同分离。

### 3.6 实施前 Job 基础强，但 completion barrier 不完整

当前生产路径已经做到：

- child settlement event、结果写入和 `nextStep` inbox admission 在同一事务；
- queued/running job 可在重启后恢复；
- pending report 可触发事件驱动 wake；
- `quiet` 可以保存结果而不注入父会话；
- 重复 settlement 被拒绝。

实施前缺口：

- Goal completion 只按 job status 检查，未统一检查 pending/unreconciled inbox；
- terminal `nextStep` 报告尚未消费时，Goal 仍可能完成；
- pending report 查询主要依据 `promoted_seq IS NULL`，promoted 但未 consumed 的
  崩溃窗口没有统一恢复语义；
- failed/uncertain job 可能因为 status 永久阻止完成，而不是依据是否已消费和是否
  已完成 authoritative reconciliation；
- `/goal complete` 与模型工具未完全复用同一 completion barrier；
- 内存 `JobBoard` 具备部分 active/unreconciled 语义，但不是生产 task 路径的唯一
  权威。

### 3.7 实施前 debug 命令存在但语义不完整

当前 `zuno debug prompt [session] [step]`：

- 能读取和脱敏 prompt receipt；
- 直接按 receipt 内的 `step` 查询；
- 不能可靠处理相同 Prompt 去重；
- A → B → A 时可能没有重新关联 A 的 receipt。

当前 `zuno debug agent <name>`：

- 只输出 catalog Agent；
- 未复用真实 TurnPlan 的 config、extension、model、reasoning、permission、Skill、
  sandbox 和 delegation 解析；
- 注册了必然报错的 `--tool`、`--params` 参数。

因此两个命令都不是新功能，而是需要修正为真实运行时诊断。

## 4. 上游设计取舍

| 来源 | Adopt：直接采用原则 | Adapt：按 Zuno 架构改造 | Reject：明确拒绝 |
| --- | --- | --- | --- |
| Codex | 小型角色 Prompt；分层用户/项目约束；Sandbox 与权限由 Runtime 强制；复杂任务才维护 Plan；子 Agent 可配置 | 保留 Zuno typed section、source、digest、receipt 和 provider-neutral projection | 复制完整 Prompt、隐藏身份、品牌或私有产品工作流 |
| OpenCode | 命令、Skill、MCP、模型环境、附件和压缩流程可发现 | 通过 Component、typed service、durable event 和 final capability snapshot 暴露 | 按模型名替换整套 Prompt；以字符串拼装作为运行时真相；追求配置/DB/Hook 兼容 |
| Slim | 明确的角色边界、task rejection、任务状态、结果协调和 wake gate | 用 SQLite JobStore、FIFO Inbox 和幂等 wake 取代插件 Hook board | 超长 Orchestrator、默认后台轮询、强制 Designer 路由 |
| OMO | 模型与职责匹配、目标持续、每次新输入重新判断当前意图 | 只注入经过验证的短 capability fragment；模型选择由配置和诊断显示 | 千行模型族 Prompt、Hook 重复续跑、`load_skills` 兼容、Markdown 状态成为权威 |

### 4.1 为什么不复制第三方 Prompt

完整复制会同时带来：

- 身份和品牌混淆；
- 隐含工具、权限、命令和目录假设；
- 上游模型、Hook 和 Provider 绑定；
- 难以审计的重复规则；
- 版权、维护和安全边界问题。

Zuno 只采用公开可描述的设计原则，以原生英文角色 Prompt、typed runtime sections、
测试和中文设计文档重新表达。任何上游原文引用都应保持短小并链接固定 SHA。

## 5. Generation 2 Prompt 合同

### 5.1 Canonical section order

建议的语义顺序：

1. `kernel.*`
2. `agent.base`
3. `collaboration.mode`
4. `runtime.intent`
5. `runtime.execution`
6. `runtime.sandbox`（可信 fallback 生效时）
7. `runtime.continuity`（最终存在 `history` 或 `notes` 时）
8. `runtime.editing`（最终可写时）
9. `runtime.verification`
10. `runtime.delegation`（最终存在 `task` 和有效 delegate 时）
11. `runtime.persistence`（存在 durable tool 或活跃 durable state 时）
12. `instructions.global.*`
13. `instructions.profile.*`
14. `instructions.project.*`
15. `goal.*` / `plan.*` / `todo.*` / `work_state.*`
16. `skills.policy` / `extensions` / `routing.*`
17. `skills.selected.*`
18. `skills.index`
19. `memory.*`

该顺序是 receipt 和 provider projection 的 canonical order。实现可以区分静态 cache
prefix 与动态 developer context，但两者必须在 receipt 中恢复为同一确定性语义
顺序。

### 5.2 Stable runtime sections

| section | 内容责任 | 生成条件 |
| --- | --- | --- |
| `runtime.intent` | 当前用户请求或 delegated objective 是本轮授权；新输入到达时重新分类意图 | 用户可见和 child turn 均存在 |
| `runtime.execution` | 最小工作流、批量独立读取、避免重复读取和重复 gates、复杂任务维护 Plan | 始终；Plan 文字仅在 `plan_update` 真正可用时出现 |
| `runtime.sandbox` | 请求/生效 sandbox 权限与无隔离 fallback 原因 | 可信的 unavailable fallback 生效时 |
| `runtime.continuity` | History/Notes 是不可信会话数据；说明会话、Agent 与 revision 边界 | 最终工具快照存在 `history` 或 `notes` 时 |
| `runtime.editing` | 当前是否只读、可编辑范围、Shell、网络、sandbox 和 uncertain side effect 规则 | 来自最终 sandbox 与工具快照 |
| `runtime.verification` | 完成证据和失败/恢复路径，不接受“看起来正确” | 用户可见实施任务；只读任务改为证据完整性 |
| `runtime.delegation` | 可用 delegate、合同、去重、无轮询、结果 reconciliation | task 工具和 delegate allowlist 的交集非空；否则只说明不可委派 |
| `runtime.persistence` | Goal/Plan/Todo/Inbox/Job 是权威；文字“下一步”不改变状态 | durable continuation 启用时 |

每个 section 必须有：

- 固定 id；
- 精确 source；
- canonical order；
- content digest；
- byte/token estimate；
- 生成它的 capability snapshot identity；
- provider projection。

### 5.3 Final capability snapshot

runtime policy 只能从最终有效能力生成：

```text
configured capabilities
∩ parent authority
∩ agent role allowlist
∩ current permission overlay
∩ sandbox availability and mode
∩ extension/MCP registration
∩ tool-definition hooks
∩ provider/model verified capabilities
= final effective capability snapshot
```

规则：

- Prompt 不得描述 snapshot 中不存在的能力；
- snapshot 锁定后才生成 capability-dependent sections；
- late MCP 只允许一次受控重建，并产生新的 snapshot identity；
- provider request、prompt receipt、child request 和 debug 输出引用同一 identity；
- 模型切换只改变经过验证的视觉、工具调用、structured output 或 reasoning 短片段，
  不替换整套角色 Prompt。

### 5.4 Prompt 预算

预算至少分别统计：

- kernel + role；
- runtime sections；
- AGENTS/instructions；
- selected Skills；
- Skill index；
- work state；
- memory。

预算测试应验证：

- section 顺序不随 HashMap、文件系统枚举或 provider 改变；
- 同 id 不能重复；
- Skill body 和 index 不重复；
- disabled capability 不产生描述；
- 模型变化不替换角色主体；
- 超预算时按明确优先级缩减 index 或 memory，不删除约束来源和 selected Skill。

除 lane/Skill 的局部预算外，hook、动态上下文、历史与最终 tool schema 应形成一次
完整 provider-visible input 估算。已知模型 context limit 时，聚合估算超限必须在
provider I/O 前返回 typed `PromptAssemblyError`；不能通过截断 instructions、
selected Skill、历史或工具合同使请求“勉强可发”。模型 context 未知时由 provider
执行最终限制。

## 6. Agent 与 Skill 合同

### 6.1 角色 Prompt

Agent 自身只保留：

- 正职责；
- 禁止边界；
- 少量角色方法；
- 输出契约。

工具用法、Git 安全、编辑规则、验证原则、权限、sandbox、Plan、Goal 和通用沟通规则
不得在每个 Agent Prompt 中重复。

主要角色：

- `orchestrator`：简单任务直接完成；复杂任务建立依赖图；只委派独立且边界清楚的
  工作；禁止重复委派、轮询和重读不变状态。
- `build`：端到端交付；复杂任务维护简洁 durable plan；不委派。
- `deep`：复现、假设排序、因果链、根因修复、回归和恢复验证；不递归委派。
- `plan`：严格只读，输出“事实探索 → 必要决策 → 实现设计 → 验收方案”的
  decision-complete plan。
- Specialist：统一返回 Outcome、Evidence、Inspected/Changed、Risks/Blocker，
  不要求模型自造 XML 或 JSON 机器协议。

### 6.2 Skill

Skill 只描述领域工作流、脚本、参考资料和高价值决策，不重复 runtime policy。

需要同步收敛：

- `deepwork`；
- `git-workflow`；
- `verification-planning`；
- 其他重复权限、编辑、验证 footer 的内置 Skill。

`deepwork` 若要求读取 Goal，`requiredTools` 必须真实包含对应只读 Goal 工具，不能只在
文字中提出不可执行要求。

`ui-design` 保持 Skill。用户可配置 Designer Agent，但产品不强制内置。

## 7. Typed delegation

### 7.1 `DelegationContract`

旧 `description + prompt` 直接删除，不提供兼容层。最终 wire shape：

```jsonc
{
  "agent": "explorer",
  "objective": "定位 prompt receipt 与 provider request 的关联缺口",
  "deliverable": "给出调用链、缺口和最小修改文件集合",
  "instructions": "只读审计；使用 CodeGraph 跟踪调用链。",
  "success_evidence": "列出精确符号和文件，并区分已实现与推断。",
  "scope": {
    "include": ["crates/zuno-engine", "crates/zuno-cli"],
    "exclude": ["provider credential code"]
  },
  "constraints": {
    "must": ["保留未提交修改"],
    "must_not": ["编辑文件", "启动外部服务"]
  },
  "dependencies": ["capability snapshot audit"],
  "background": true,
  "reportDelivery": "nextStep",
  "task_id": "optional-child-session-id"
}
```

必填：

- `agent`
- `objective`
- `deliverable`
- `instructions`
- `success_evidence`

可选：

- `scope.include`
- `scope.exclude`
- `constraints.must`
- `constraints.must_not`
- `dependencies`

保留：

- `agent`
- `background`
- `reportDelivery`
- `task_id`

模型、reasoning 和 category 路由应由独立的 host routing 类型处理。若将来允许用户
显式 override，也必须与模型可见合同分离，不能让 Council/Workflow 构造伪造的
DelegationContract 只为复用模型解析。

删除：

- `description`
- `prompt`
- `subagent_type`
- `category`
- `model`
- `effort`
- 隐藏 `load_skills`
- 其他只为旧参数报错而存在的兼容字段。

### 7.2 子 Agent 能力继承

子 Agent 默认继承父会话的：

- provider/model 与 reasoning 配置；
- MCP 配置；
- Skill discovery 配置；
- sandbox 配置；
- permission ceiling；
- orchestration capability snapshot identity。

最终能力仍取交集：

```text
parent authority
∩ child role permissions
∩ child tool allowlist
∩ task scope
∩ current runtime availability
```

`requiredSkills` 自动按源重新加载。父 Agent 已展开的 Skill 正文不无条件复制，避免
过期、越权和 Prompt 膨胀。

目录元数据和完整 Skill 正文必须分别预算。目录继续使用
`skills.maxContextTokens`；所有已选正文共享由模型窗口推导的聚合预算，并可由
`skills.maxSelectedContextTokens` 显式收紧或放宽到运行时上限。任何新加载或历史
恢复超限都应在 provider request 前失败关闭，不能截断正文或悄悄删除已选 Skill。

### 7.3 `TaskReportMetadata`

报告机器协议由宿主生成，不要求模型自序列化。当前结构：

```jsonc
{
  "schemaVersion": 2,
  "jobId": "job_*",
  "workContext": {
    "schemaVersion": 1,
    "goalId": null,
    "planId": "plan_*",
    "planRevision": 3,
    "planStepId": "verify-release"
  },
  "sessionId": "ses_child",
  "parentSessionId": "ses_parent",
  "agent": "explorer",
  "status": "completed",
  "finalText": "...",
  "usage": {},
  "changedPaths": [],
  "verificationRecords": [],
  "uncertainSideEffects": [],
  "evidenceErrors": []
}
```

同一 metadata 必须进入：

- `agent_job.result`；
- `nextStep` 的 `subagentReport.metadata`；
- foreground `ChildTurn`；
- task ToolOutput metadata；
- parent 规范 user message 的 `message.data.taskReport`；
- TUI、ACP、CLI replay projection，其中 ACP 使用
  `_meta.zuno.taskReport`。

`changed_paths` 和 `verification_records` 只有在存在宿主级 typed durable event 时才能
填充。不得解析模型自然语言或任意 Shell 输出伪造这些字段。每个 internal Job 在
准入时还会由宿主捕获当前 Plan 位置并写入 `workContext`；该关联不是模型参数。只要
对应步骤尚未完成，终态 Job 证据会继续进入恢复上下文。
admission 时持久化 `evidence_start_rowid`；继续已有 child session 时，只查询该游标
之后的 typed tool parts，防止先前轮次证据污染当前报告。foreground task 同样拥有
internal Job id，只是执行仍附着在当前 parent tool call。

## 8. Durable Job 与 completion barrier

### 8.1 唯一权威

生产权威是 SQLite 中的：

- `agent_job`
- `session_input`
- Goal/Plan/Todo 表
- typed reconciliation event

内存 `JobBoard` 不得继续作为第二套状态机。它应删除，或变成对生产 Store 的纯
projection。

### 8.2 状态语义

| 状态 | 定义 | 是否阻止父目标完成 |
| --- | --- | --- |
| active | job 为 queued/running | 是 |
| pending | terminal `nextStep` report 已 admitted，input 为 queued/steering | 是 |
| unreconciled | report input 已 promoted，但未 consumed | 是 |
| reconciled | report input 已 consumed | 否；仍受其他工作和 uncertain 状态约束 |
| quiet terminal | terminal result 持久化，不产生 parent input | 否，不 wake |
| ordinary failed | 失败报告未消费时阻止；消费后父 Agent 可选择修复、替代或结束 | 取决于 report 是否已消费 |
| uncertain | side effect 结果未知 | 始终阻止，直到 typed authoritative reconciliation |
| cancelled | 有 `nextStep` report 时按 pending/unreconciled 处理；quiet cancel 直接 terminal | 取决于 delivery |

### 8.3 原子和幂等要求

- 同一 parent + host-derived logical key 若存在 active、uncertain 或未消费报告，
  拒绝重复派发；foreground 与 background 使用同一 JobStore admission；
- 同一 provider Attempt 内的 terminal Job 仍阻止相同 logical key，避免
  `tool_calls=1` 时两个相同 foreground 调用在第一个完成后串行重跑；
- 新 child session 与对应 Job 在一个 SQLite 事务中创建；重复或失败 admission
  回滚两者，不产生 orphan session；
- `nextStep` 必须在一个事务中 settle child、admit parent input、记录事件；
- wake 在事务提交后至多一次；
- `quiet` 不 wake；
- 重启恢复不得重复报告、重复 wake 或重复执行 child；
- promote 后、模型消息写入前崩溃，重启仍能看到并消费同一 input；
- `/goal complete` 与模型 Goal 工具调用同一 completion barrier；
- uncertain outcome 只能经 authoritative inspection 和 typed resolution 解除。

`job_reconcile` 是唯一 model-facing typed resolution：必须记录 authority 与
evidence，只能把 uncertain 解析为 completed、failed 或 cancelled，永不重放原始
side effect，并在同一事务内替换未消费的 uncertain report。

## 9. Plan、执行波次与压缩

### 9.1 何时维护 Plan

默认 profile 通过类型化 `HostPlanningCapability` 声明宿主规划能力；自定义 profile
需要显式选择。能力存在时，宿主在第一次 provider request 之前执行统一分类：

- 用户输入或已解析 command 可以被分类为需要创建 Plan；child report、steering、
  retry 只能继续既有状态，不能自行触发新根 Plan；
- 已有 active durable Plan 且输入是明确继续：继续维护；
- 新的实质性多阶段目标：要求模型以 `action=create` 和当前 revision 替换根 Plan；
  宿主归档旧根，但不生成通用步骤；
- 直接回答、一次有界读取、一次短小的既有变更 commit：允许原子执行；
- 图片、resource、selection、branch diff 等 typed context，以及足够大的多文本块
  输入：进入 planned path；
- 其他工程请求：分类为 `Required`，由模型创建用户可见的战略步骤。

这使调研、修改、验证组成的普通多阶段工作默认可见，并确保跨组件、委派、多阶段
验收及可能压缩/中断恢复的工作维护 durable Plan。宿主决定是否需要 Plan，模型负责
战略步骤。隐藏 `plan_update` 会使分类结果成为 `Unavailable`，阻止新的模型修改；
已有 Plan 仍会持久化、投影并恢复。`create`、`append`、`push` 的 id 由宿主生成，
`patch` 只提交变化的 id；`completed` 与 `superseded` 都是终态。Todo 是 step 下的
可选细化，不要求机械一一对应。
若一个活跃步骤需要聚焦的临时工作流，模型使用 `plan_update action=push` 在同一事务
中暂停父 Plan 并安装持久子 Plan；子步骤全部终态后使用只带 revision 的
`action=pop`，归档子 Plan 并精确恢复父 Plan 一次。

机器执行波次持久化为独立 `DriverPhase`，不写入用户 Plan。最终回复前的 driver 只
检查 Plan、Todo、Job、Goal、工具结果与验证记录；普通会话最多触发两次 durable
reconciliation continuation，仍无法对齐则进入 typed `PlanUnreconciled` 人工等待。
进程重启继续原 cycle，模型自然语言不作为完成证据。

### 9.2 执行波次

默认执行策略：

1. 一次探索波次；
2. 按组件集中编辑；
3. 定向测试；
4. 一次共享 gates；
5. 对照验收项收尾。

禁止：

- 无状态轮询；
- 未发生输入变化时重复读取同一文件；
- 未发生相关变化时重复运行相同 Git 检查；
- 重复委派同一 scope；
- 为了展示进度制造无意义 Plan。

默认不设置 step 上限；用户可以显式配置。文本“下一步我会……”不构成运行状态，
也不能触发自动续跑。

### 9.3 Compaction

压缩后必须恢复并再次投影：

- active Goal；
- durable Plan revision 与未完成 steps；
- Todo / work items；
- active jobs；
- pending 和 unreconciled reports；
- uncertain effects；
- selected/required Skills；
- capability snapshot identity；
- latest prompt receipt identity。

当前实现每轮从 SQL 重建 Goal，并生成有界的 `runtime.work_state`，投影 Plan、
Todo/WorkItem、active/uncertain Job、未消费报告和上一份 prompt receipt identity。
所有查询来自同一个 deferred SQLite snapshot；各集合最多 64 项，完整 section
最多 16 KiB，并记录 omitted count。描述性文本先被 UTF-8 安全截断，再按尾部条目
缩减；稳定 identity 和 reconciliation 字段不能容纳时失败关闭。完整细节和修改仍
通过 typed tools 完成。

## 10. 诊断命令

### 10.1 Prompt

目标接口：

```sh
zuno debug prompt --session <id> [--step <non-zero>] [--show-sensitive]
```

查询链：

```text
session + step
→ 对应 session.provider.request.1 started event
→ promptReceiptID
→ 精确 session.prompt.assembled.1 event
```

输出：

- section id、order、source、role、trust、priority；
- bytes、estimated tokens、SHA-256；
- agent、step、turn id；
- assembly digest、actual digest、hook transformed；
- capability snapshot identity；
- provider projection；
- 默认脱敏内容。

Prompt trace 去重应为 `digest → receipt event id`，而不是只保存 digest set，确保
A → B → A 时第三次请求重新引用 A receipt。

### 10.2 Agent

目标接口：

```sh
zuno debug agent <name>
```

必须复用真实但无副作用的解析路径，输出：

- Agent 名称、mode、source、hidden、steps；
- 配置模型和最终 provider/model；
- preset、reasoning effort、reasoning support、context window；
- 有效 permission rules；
- declared tool allowlist 与 parent authority；
- delegates 和 `requiredSkills`；
- sandbox mode 和 policy source；
- capability snapshot identity 和 extension revision；
- Prompt policy digest；
- 所有 resolution diagnostics。

该命令复用正式 `McpRuntime` 主动连接 enabled server，输出 lifecycle state、
discovery status、connected servers、精确当前 tool schema、连接 warning 和
cleanup warning。连接成功后的 discovery 失败、取消或超时也必须执行有界 close。
当前根 Agent 诊断逐个应用角色权限和 Agent allowlist；根诊断没有 parent Attempt，
不得构造虚假的 parent schema authority。委派历史的最终 authority 必须从对应
Attempt 的持久化 orchestration snapshot 读取。未配置 MCP 时才输出
`not-connected`，不得构造虚假 tool id。

不得：

- 创建 session；
- 启动 provider 或发出模型请求；
- 把 MCP transport 留在后台；所有诊断连接必须在返回前清理；
- 改变 sandbox 状态；允许无副作用的本地 readiness 检查；
- 输出 credential、环境变量值或敏感 header；
- 保留必然报错的 `--tool` / `--params`。

配置态输出必须标明“declared/pre-runtime”；历史最终工具从 provider request 的
orchestration snapshot 读取，不能伪装成当前已锁定状态。

## 11. 四阶段实施与提交边界

### Phase 1：审计基线与 Prompt 分层

实现：

- 固定本文的四个上游 SHA；
- runtime section stable ids；
- canonical order、source、digest 和预算；
- final capability snapshot 生成时机设计；
- 修正文档中 render 排序的错误。

先写失败测试：

- 确定性 section 顺序；
- 重复 id 拒绝；
- 每个 section 的 source/digest；
- 无不可用工具描述；
- capability 条件；
- Prompt 预算；
- 模型切换不替换角色主体。

提交只包含 Phase 1 文件。

### Phase 2：角色、Skill 与委派合同

实现：

- 收敛全部内置 Agent Prompt；
- 收敛重复 Skill policy；
- `DelegationContract`；
- host-generated `TaskReportMetadata`；
- 父子能力继承和 `requiredSkills`。

先写失败测试：

- 所有旧 `description + prompt` 调用者编译失败并迁移；
- unknown/legacy task 参数被拒绝；
- specialist report 结构一致；
- 子 Agent 能力严格取交集；
- Skill body 不被无条件复制；
- foreground/background/replay metadata 一致。

提交只包含 Phase 2 文件。

### Phase 3：运行时工作流

实现：

- JobStore 与 continuation board 统一；
- active/pending/unreconciled/uncertain completion barrier；
- report admission、wake 和 restart idempotency；
- Plan 触发阈值；
- 执行波次；
- compaction work-state producer；
- 默认无 step 上限、用户可配置。

先写失败测试：

- 原子 commit 不建 Plan、不委派、不重复 Git 检查；
- 跨 crate 任务维护 Plan，只运行一次共享 gates；
- 并行只读任务 scope 不重叠，结果只投递一次；
- settlement 后崩溃重启仍能继续；
- active、pending、unreconciled、cancelled、failed、uncertain 状态矩阵；
- `/goal complete` 不绕过 barrier；
- compaction 后状态完整进入新 receipt。

提交只包含 Phase 3 文件。

### Phase 4：诊断、文档与验收

实现：

- 修正两个 debug 命令；
- 更新 runtime、orchestration、configuration 和用户文档；
- TUI、ACP、CLI 对同一 durable event 使用一致 projection；
- 真实双模型 E2E。

先写失败测试：

- Prompt step 通过 provider request receipt 查询；
- A → B → A 正确关联；
- `--step 0` 被 parser 拒绝；
- debug Agent 与真实 TurnPlan 解析一致；
- debug 无 session/provider/MCP 副作用；
- 敏感字段永不输出；
- TUI、ACP、CLI replay 一致。

提交只包含 Phase 4 文件。

## 12. 验收矩阵

### 12.1 Prompt

- section 顺序确定；
- section id 唯一；
- source、digest、token estimate 可复核；
- AGENTS 分层正确；
- Skill index/body 去重；
- final snapshot 与提示内容一致；
- 模型切换只改变短 capability fragment。

### 12.2 工作流

- 简单任务无无意义 Plan；
- 复杂任务有 durable plan；
- 委派无重复；
- report 只投递和 wake 一次；
- restart 和 compaction 幂等；
- completion barrier 状态矩阵正确；
- uncertain side effect 不被机械重放；
- 所有客户端投影一致。

### 12.3 真实 E2E

分别使用 GPT 5.6 Sol 与 Claude Opus 5 验证：

1. 原子任务；
2. 深度 Debug；
3. 并行委派；
4. Plan-only。

每次记录：

- provider calls；
- tool call 次数；
- 重复读取；
- Plan 更新；
- child wake；
- report delivery；
- prompt receipt 和 capability snapshot；
- 最终验证证据。

E2E 不能用模拟结果代替账户和 Provider 的真实响应；若授权、模型或服务不可用，
应明确记录阻塞，不绕过服务授权。

### 12.4 最终 gates

```sh
cargo fmt --all --check
cargo test -p <changed-crate>
cargo test --workspace
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

只有命令完整成功返回时，才能声称对应 gate 通过。

## 13. 当前实现结论

1. `PromptEnvelope`、section provenance、durable receipt 和 typed work state
   是正确基础，不重做。
2. 最多八个 runtime section 从最终 provider-visible capability snapshot 生成，禁用
   能力不进入 Prompt；`runtime.continuity` 只在 `history` 或 `notes` 最终可见时出现。
3. `PromptAssembly::push` 执行稳定 canonical semantic sort；同 lane 保留 producer
   顺序，重复 id 被拒绝。
4. 角色 Prompt 已收敛；公共执行、编辑、验证、委派和持久化规则进入 runtime
   sections。
5. typed `DelegationContract`、host-generated `TaskReportMetadata`、父子能力交集、
   前后台统一 logical task 去重、child+Job 原子 admission、每次委派 evidence
   游标、evidenced `job_reconcile`、Job/Goal barrier、同一 SQL snapshot 生成的
   16 KiB `runtime.work_state`、无默认 step 上限、宿主 Plan 分类器和两个 debug
   命令已落地。
6. GPT 5.6 Sol / Claude Opus 5 已分别完成原子任务、深度 Debug、并行委派和
   Plan-only E2E。
7. 完整 provider-visible Prompt 在已知 context limit 下执行总预算门禁；typed
   child report 可从数据库一致回放到 TUI、ACP 与 CLI。
8. 上游只作为 adopt/adapt/reject 的设计来源，不复制完整 Prompt 或专属工作流。
9. `ui-design` 保持 Skill；`dual-review`、`auto-release` 保持用户自定义。
10. `zuno run --variant`/`--thinking` 在 provider I/O 前解析并校验；
    named-only variant catalog 不会获得推断出的 canonical effort；
    `debug agent` 连接实时 MCP 并有界清理；
    `debug sandbox --check` 通过真实 helper 执行验证宿主部署；大量全局 Skill
    以 raw discovery、effective Agent view、预算、覆盖率、歧义统计和有界预览呈现。
