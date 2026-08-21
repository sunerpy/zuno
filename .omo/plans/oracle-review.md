# oracle 复核 —— `7d0b05f` 距离「完美替代 opencode」还差什么

在 `main` 的 `7d0b05f` 上复核。只读：没有改动任何源文件，没有跑构建或测试。所有结论均来自源码
与本项目自己的文档，`file:line` 引用均可直接核对。

本页是一份**决策记录**，不是任务清单：每一项都给出「用户会遇到什么」「证据在哪」「代价多大」，
排序依据是用户后果而不是修起来顺不顺手。

## 结论

**`7d0b05f` 还不是 opencode 的可直接替换品。** 本地前台写代码这条主路径是通的；卡住替换的
是下面这些具体缺口，它们不是「大体能用只差打磨」，而是每一项都能在正常使用中被撞到。

## 按用户后果排序的七项缺口

### 1. 插件兼容层有实测出来的洞 —— 代价：大

实测 20 条 v1 插件路由中 **6 条仍返回 `501 not_implemented`**：`app.log`、`config.get`、
`session.status`、`session.update`、`session.children`、`session.todo`
（`docs/compatibility-matrix.md:279-306`）。58 个 `/api` 操作中 10 个返回 503（`:187-190`）。
3 条 CLI 命令未注册、8 条被主动拒绝（`:142-176`）。

排第一的理由不是数量，而是性质：这些是**可复现的协议失败**，而且恰好落在文档唯一声称保留的
兼容面上（`:3-5`）。其余各项是功能不完整，这一项是承诺不成立。

### 2. `-s` 恢复能带回模型上下文和标题，但带不回屏幕上的对话 —— 代价：中，1–2 天

TUI 永远用一个空的 `TranscriptView` 构造 `SessionScreen`
（`crates/zuno-tui/src/views/session.rs:727-783`），启动阶段只注入输入历史
（`crates/zuno-cli/src/cmd/tui.rs:357-373`）；而下一次请求会从数据库完整回灌历史
（`crates/zuno-engine/src/loop.rs:733-809`）。

结果是最难自辩的一种表现：**用户看着欢迎页，模型却在引用一段屏幕上不存在的对话。** PR #23 已经
把标题种进去了（`tui.rs:318-329`），但没有种 transcript —— 差的正是这一半。

### 3. provider 线上行为还没有拿到「可替换」强度的证据 —— 代价：短，1–4 小时

兼容矩阵自己承认：**从未对真实 provider 的 wire bytes 做过差分比对**
（`docs/compatibility-matrix.md:326-334`）。PR #25 把从**某一个** gateway 实测到的 400 归纳出的
拆分规则，套用到了该接口面上的**所有** provider
（`crates/zuno-provider-compatible/src/request.rs:16-38`），而测试只钉住了那一个胶囊的形状
（`:805-839`）。

这一项代价最小、价值最高，具体实验见下方「最便宜的实验」。

### 4. `task` 能用，但只是阻塞式的前台委派 —— 代价：大，3 天以上

生产 host 直接拒绝 `background: true`（`crates/zuno-cli/src/cmd/child_turn.rs:221-229`），子任务
事件被丢弃，父会话看不到任何进度（`:257-269`）。子智能体面板是事后的 transcript 投影，不是实时
监视器（`crates/zuno-tui/src/views/subagent.rs:3-28`）。

一个够用的 `JobBoard` 已经写好且无人调用
（`crates/zuno-agent/src/continuation.rs:468-505`、`:730-746`）—— 又一次「造好了不可达」。

### 5. 「撤销这一轮」回滚了工作区但没回滚对话，且跨重启无法撤销 —— 代价：中，1–2 天

`SnapshotHistory` 每次启动都是空的（`crates/zuno-cli/src/cmd/tui.rs:1206-1219`），只记录本进程内
发生的轮次（`:1419-1445`）。`restore_turn` 只换 Git 树（`crates/zuno-snapshot/src/store.rs:250-301`），
而 transcript 回滚是另一条 HTTP/DB 路径（`crates/zuno-server/src/api/session.rs:854-900`）。

于是后续轮次会把那段「已撤销」的对话重新读回来（`crates/zuno-engine/src/loop.rs:742-809`）：
**文件回滚了，模型仍然相信那些操作发生过。** 这比不支持撤销更危险。

### 6. agent 配置被接受、被展示，但有些字段永远到不了请求里 —— 代价：中，1–2 天

`variant`、`temperature`、`top_p`、`options` 会被解析并合并
（`crates/zuno-catalog/src/agent.rs:569-616`），而实际解析器只消费 `model`、`prompt`、`steps`
（`crates/zuno-cli/src/cmd/turn.rs:250-285`）。

**静默降级**：配置合法、在列表里看得见、运行时走 provider 默认值。用户没有任何线索。

### 7. MCP 启动已修好，但动态工具刷新这一环没闭合 —— 代价：中，1–2 天

remote transport 建了刷新通道又立刻把接收端丢掉
（`crates/zuno-mcp/src/remote/transport.rs:53-69`），尽管 `Catalog::refresh` 是现成的
（`crates/zuno-mcp/src/catalog.rs:432-458`）。TUI 监听的是 controller 生命周期，不是
`tools/list_changed`（`crates/zuno-cli/src/cmd/tui.rs:1340-1397`）。

## 追加发现：`AGENTS.md` 指令注入没有任何生产调用者

这一项不在 oracle 的复核范围内，是复核之外查到的，性质与第 4、7 项完全相同。

`crates/zuno-config/src/instructions.rs` 是一份完整的移植：全局 `$CONFIG/AGENTS.md` 或
`~/.claude/CLAUDE.md`、项目级 cascade、配置里的 `instructions[]` 全都实现了。但在 `crates/` 下
排除该模块自身与测试后 grep，只剩 `lib.rs` 的 re-export —— **没有一个生产调用者。**

已实测证实，不只是读码推断：在全局与项目两个 `AGENTS.md` 里各写入标记后发起请求，抓取到的 POST
body 中那段 11,948 字节的 system prompt **既不含任何标记，也不含 `Instructions from` 包装**。

与 PR #21 那四个未接线子系统是同一类缺陷。**修复已在并行任务中进行中**，记为 in-progress，
不是待办。

## 已经过时的审计结论 —— 不要重复修

| 旧结论 | 现状 |
|---|---|
| `task` 未注册 | 已注册，`tool_runtime.rs:157-164` |
| skills 进不了 prompt | 已注入，`turn.rs:1529-1576`，上限 32 KiB |
| 没有 `skill` 工具 | 已有，`tool_runtime.rs:189-192` |
| headless 路径缺 MCP | 已接线，`mcp_runtime.rs:1-30` |
| 注入键泄漏给严格工具 | 已在 `crates/zuno-tool/src/lib.rs:130-159` 剥除 |

**仍然成立的旧结论**：子智能体没有 job 跟踪、MCP `listChanged` 未闭合、agent 采样字段被忽略。

## 文档：不是假承诺，但和目标互相矛盾

「不是 drop-in replacement」这个立场（`README.md:3-5`、`:19-25`；
`docs/compatibility-matrix.md:3-12`）是对**今天这个产品**的诚实描述，不是虚假声明。问题在于它与
「完美替代 opencode」这个既定目标直接冲突，必须**有意识地**二选一，而不是让两种说法同时留在
仓库里。

其余三处需要修正：

- README 的插件支持声明过宽（`README.md:58-71`），考虑到那 6 条 501。
- 基线版本前后不一致：页面写 `1.18.13`（`:3`），而 API 差分用的是钉住的 `1.18.18` 快照
  （`:181-184`）。
- `divergences.md:149-160` 的「每一项行为差异都在本页」已经不能再作为穷尽性声明 ——
  `known_gaps` 漏掉了 transcript 恢复、进程内撤销、后台委派和 agent 采样字段这四项。

## 最便宜、价值最高的实验（对应第 3 项）

让上游 `opencode 1.18.18` 与 Zuno 对同一个抓包 stub 跑同样五轮脚本化对话：

1. 纯文本；
2. assistant 文本 + 工具调用 + 结果；
3. 一个回放 `encrypted_content` 的 Responses reasoning item；
4. 一条在 delta 中途被打断后恢复的流；
5. 在 OpenAI、OpenRouter、Anthropic/Bedrock、Google 四个 option 家族上分别跑
   effort `off`/`low`/`high`。

归一化生成的 id 之后，比对消息顺序、assistant 分段、`tool_calls`、`function_call_output`、胶囊与
effort 选项。测试脚手架已存在：`/tmp/opencode/e2e/proxy.py` 与 `/tmp/opencode/verify`。

## 置信度

| 判断 | 置信度 | 依据 |
|---|---|---|
| 「还不是可直接替换品」 | 高 | 从源码与项目自己的文档双向证明 |
| 是否存在新的 provider 回归 | 中 | 需要上面那个实验才能定论 |
