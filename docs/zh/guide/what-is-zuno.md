# Zuno 是什么？

Zuno 是一个以单个 Rust 可执行文件运行的编码 Agent。你交给它一个目标，它会读代码、改文件、执行命令，并汇报自己验证过什么。

它与大多数编码 Agent 的区别集中在三件事上：**目标是有预算、可恢复的持久对象**；**一个专职 Agent 团队而不是一个全能提示词**；以及**编排结构由配置拥有，模型不能在运行时改写它**。

## 一、目标模式：让长任务能被追踪和收敛

多数 Agent 的"任务"只存在于一次对话里。Zuno 的 Goal 是一个持久对象，带三样东西：

| 字段 | 作用 |
| --- | --- |
| `objective` | 具体目标，创建后模型不能悄悄缩小它 |
| `success_criteria` | 完成的判定条件，创建后模型无法改写 |
| `token_budget` | token 上限，超出即停止而不是无限烧下去 |

关键在于**终止条件是受约束的**。一个活跃 Goal 会持续推进，直到它真的完成、被显式暂停、达到预算，或遇到类型化的永久失败。模型想标记完成，必须拿出授权证据；想标记阻塞，必须给出具体的 `blocking_condition`，而且同一个真实僵局要连续三个回合都存在。

```sh
zuno run "把 /users 接口迁移到分页，并让集成测试通过"
```

这条命令背后不只是一次提问。目标、计划、待办项、后台作业都是 SQLite 里的持久记录，进程死掉后可以从中重建工作现场 —— 包括重试的截止时间。所以"下一步我会……"这类文字不构成进展，只有持久状态的变化才算。

进一步阅读：[Goal、Plan 与 Todo](/zh/guide/durable-state)。

## 二、一个专职 Agent 团队，而不是一个全能提示词

`zuno agent list` 列出 14 个内置 Agent。其中 4 个（`compaction`、`council-synth`、`summary`、`title`）服务于运行时内部任务；下面 10 个是你实际会选择的角色。它们不是同一个提示词的不同措辞，而是**能力边界不同的角色**：

| Agent | 定位 |
| --- | --- |
| `orchestrator` | 默认主 Agent，负责拆解与委派，保留架构决策与最终验收 |
| `build` | 端到端交付 |
| `plan` | 只读规划，写入类工具根本不会注册 |
| `deep` | 承担困难的跨领域实现，且不再递归委派 |
| `explorer`、`librarian`、`oracle`、`looker`、`fixer`、`general` | 专职子 Agent，各有明确的正负向职责 |

这套划分之所以有意义，是因为**Agent 契约只能收窄权限，永远不能放宽**。选一个只读 Agent 就是一项保证，而不是一个可以被配置反转的默认值：

```sh
# 无论 sandbox.mode 怎么配，这次都不可能写文件
zuno run --agent plan "审计重试预算的起算时机"
```

委派同样有真实边界：子 Agent 拿不到父级不具备的工具，`delegates` 精确限定它能调用谁，`subagent_depth` 限制层数。子 Agent 的报告是父级需要验证的**证据**，不是可以直接采信的结论。

进一步阅读：[Agent](/zh/guide/agents)、[编排与委派](/zh/guide/orchestration)。

## 三、编排由配置拥有

Council 让多个隔离的席位各自独立评估同一个问题，然后综合结论。它的席位、模型路由、法定人数（quorum）、并发上限、重试策略、端到端超时、预留的综合时间和输出上限**全部由配置决定** —— 模型在调用时只能提出问题，不能改写这些参数。

Workflow 同理：`maxAgents`（默认 12）、`maxParallel`（默认 4）和节点 DAG 是不可变模板。

这个取舍是刻意的。把编排参数交给模型，就等于让它在压力下自行放宽约束；固定在配置里，行为才可复现、可审计。

## 设计谱系

Zuno 把 DeepSeek Harness、Codex、oh-my-openagent、pi-agent、OpenCode 与 Claw Code 当作**设计来源，而不是兼容目标**。

其中影响最深的是 DeepSeek Harness 的"一切皆插件"。在 Zuno 里，这个 ABI 具体化为原生 Rust `Component`：它准备类型化服务与延迟副作用，为每个启动的副作用返回精确的异步 disposer，并参与事务化的 `HarnessProfile` 替换。一项能力只有在接口、提供方、消费方三者齐备时才算完整。

由此得到几条贯穿全项目的规则：

- **注册即副作用。** 挂载返回的 disposer 精确移除它注册过的东西，profile 替换失败时按相反顺序回滚。
- **模型可见即被记录。** 任何能改变一次模型请求的输入，都必须能从持久会话事件中重建。
- **组合优于分支。** 部署选择进入经过校验的 profile 字段，而不是主循环里不断增长的条件判断。

完整的借鉴与拒绝清单见 [Harness 对比](https://github.com/sunerpy/zuno/blob/main/docs/design/harness-comparison.md)。

## 单个二进制的实际含义

Linux 是静态 musl 产物，macOS 与 Windows 是原生构建。执行路径里没有 Node、Python 或包管理器，也没有需要与 Agent 版本对齐的运行时。

只有一个外部依赖：`rg`（ripgrep）主版本 14 或更新。因为 `glob` 与 `grep` 驱动的是真正的 ripgrep，而不是再实现一遍它的目录遍历器 —— 缺失或版本不符时工具运行时直接报启动错误，不静默降级。

扩展也是原生的：声明式包（Agent、workflow、Skill）、显式 WASI 授权下的 WebAssembly 组件，或受限子进程。Zuno 不加载 Rust 动态库 —— Rust 没有稳定的插件 ABI，而卸载一个库无法证明它的线程、回调和借用值都已消失。

## 与聊天形态工具的差别

一个能调工具的聊天界面是为对话优化的；Zuno 为一个能在中断后存活的工作单元优化。

| 关注点 | 聊天形态的工具 | Zuno |
| --- | --- | --- |
| 任务 | 一次对话的隐含意图 | 带成功条件与预算的持久 Goal |
| 分工 | 一个全能提示词 | 10 个可选的、能力边界不同的 Agent |
| 委派 | 同一上下文里的另一段提示词 | 拥有自身持久状态与能力上限的子会话 |
| 编排 | 模型自行决定 | 席位、quorum、并发由配置固定 |
| 历史 | 内存对话或托管线程 | 持久 SQLite 事件，可重放可续跑 |
| 重试 | 客户端循环，重启即丢 | 落盘的指数退避截止时间 |
| 工具重复执行 | 失败即重试 | 默认至多一次，只有只读或幂等工具可声明可重放 |
| 命令安全 | 请求模型不要造成破坏 | OS 约束加一道独立权限门 |

差别在副作用附近发生超时时最明显：Zuno 把该结果记录为**不确定**，要求检查权威状态，而不是机械重跑并指望第一次什么都没做。

## Zuno 不做什么

- **不做托管服务。** 没有控制台、随包 Web 应用或托管 GitHub Agent。那些命令名注册的唯一目的是说明用什么替代，并以失败退出。见[被排除的命令](/zh/cli/excluded)。
- **不追求兼容其他 Agent。** 与 OpenCode、Codex、Claude Code 之间没有配置、插件、hook 或工具参数兼容性。它们是设计参考。
- **不用增量迁移链。** 项目尚未发布，schema 变更提升格式版本，开发数据库直接重建。见[数据库生命周期](/zh/operate/migration)。
- **不做自卸载。** 用当初安装它的方式移除这个二进制。
- **macOS 与 Windows 尚无受约束沙箱。** 约束后端目前只有 Linux 实现，其他平台默认失败即拒绝；具备写能力的 Agent 可以通过受信的显式选择使用原生执行。见[权限与沙箱](/zh/guide/permissions)。

## 从哪里开始

想先跑起来：[安装](/zh/guide/installation) → [快速开始](/zh/guide/quick-start)。

想先理解执行模型：[Goal、Plan 与 Todo](/zh/guide/durable-state) 与 [Agent](/zh/guide/agents) 这两页信息密度最高。

## 参见

- [Goal、Plan 与 Todo](/zh/guide/durable-state)
- [Agent](/zh/guide/agents)
- [编排与委派](/zh/guide/orchestration)
- [权限与沙箱](/zh/guide/permissions)
