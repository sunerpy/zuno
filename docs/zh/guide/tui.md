# 终端应用

`zuno tui` 是交互界面，也是不带参数的 `zuno` 所运行的东西。它是持久运行时之上的一个视图，而不是一个自带 Agent 循环的客户端，这就是为什么你在其中看到的一切事后都能从会话事件重建。

```sh
zuno
zuno tui --continue
zuno tui --session ses_1a2b3c --sandbox read-only
zuno tui --model openai/gpt-5 --prompt "review the diff on this branch"
```

## 屏幕区域

| 区域 | 内容 |
| --- | --- |
| 对话记录 | 持久的 assistant 内容、工具卡片、错误、中断标记 |
| 侧边栏 | 会话、job、用量以及持久的子会话 |
| 编辑区 | 你正在起草的输入 |
| 身份行 | 解析出的 Agent、目录中的模型显示名、配置的推理强度 |
| 末行 | 实时控制面：回合脉冲、中断按键、提示词占用率、命令按键、Agent 与模型徽标 |

身份行会跟在较短回复的末尾，一旦内容填满视口，它就会吸附在编辑区上方。末行以中性徽标重复当前的 Agent、模型和强度，因此在回合运行期间，为下一回合所做的选择仍然可见。按 Tab 会立即更新这个徽标，而真正的宿主替换仍推迟到回合边界。

短暂的「working」行不会插入对话记录。持久活动、错误、中断标记和 assistant 内容会。

上下文占用率是最近一次完整的 provider 提示词除以目录中的上下文上限。它在每次 provider 报告时被替换，而不是在整个会话中累加；累计的 Token 桶位于用量投影与侧边栏。

## 提交、排队与引导

| 按键 | 空闲时 | 回合进行中 |
| --- | --- | --- |
| `Enter` | 启动一个回合 | 准入一个 FIFO 队列项，留给下一个回合 |
| `Ctrl+Enter` | — | 引导：在最近的安全步骤边界处软中断 |
| `Shift+Enter`、`Alt+Enter`、`Ctrl+J` | 换行 | 换行 |
| `Escape` | — | 中断；再按一次确认 |

只有在 SQLite 提交之后，某一项才会被报告为已排队。待处理项可以按 revision 编辑或取消，并且能在进程重启后存活。

引导可以唤醒一个 provider 流或一段重试等待：Zuno 会为部分 assistant 输出打检查点、提升持久输入，然后启动下一个模型步骤。正在执行的工具不会为了引导而被抛弃，因此它的结果会先到达下一个安全点。如果回合在某次引导被消费之前就结束了，已准入的条目会保持待处理，并在下一回合按 FIFO 顺序被提升。它绝不会丢失，也不会重复。

## 默认按键

`Ctrl+X` 是 leader。leader 序列让单个字符仍可作为文本使用。

| 绑定 | 按键 | 用途 |
| --- | --- | --- |
| `leader` | `ctrl+x` | Leader 组合键 |
| `command_list` | `ctrl+p` | 命令面板 |
| `session_interrupt` | `escape` | 中断当前回合 |
| `session_rename` | `ctrl+r` | 重命名会话 |
| `session_delete` | `ctrl+d` | 删除会话 |
| `session_background` | `ctrl+b` | 把工作转入后台 |
| `session_pin_toggle` | `ctrl+f` | 置顶或取消置顶 |
| `session_new` | `<leader>n` | 新会话 |
| `session_list` | `<leader>l` | 会话选择器 |
| `session_timeline` | `<leader>g` | 时间线 |
| `session_compact` | `<leader>c` | 压缩历史 |
| `session_export` | `<leader>x` | 导出 |
| `session_queued_prompts` | `<leader>q` | 已排队的提示词 |
| `sidebar_toggle` | `<leader>b` | 显示或隐藏侧边栏 |
| `status_view` | `<leader>s` | 状态 |
| `theme_list` | `<leader>t` | 主题选择器 |
| `editor_open` | `<leader>e` | 打开外部编辑器 |
| `prompt_skills` | `<leader>k` | Skill 选择器 |
| `mcp_list` | `<leader>p` | MCP server |
| `display_thinking` | `<leader>i` | 切换推理内容显示 |
| `tool_details` | `<leader>o` | 工具详情 |
| `diff_open` | `<leader>d` | Diff 浏览器 |
| `app_exit` | `ctrl+c`、`ctrl+d`、`<leader>q` | 退出 |

`leader_timeout` 默认 5000 毫秒，因此续接提示的浮层会保持可读五秒，除非有另一个按键完成或取消该序列。浮层打开期间的交互会重置这个截止时间。重新绑定见[主题与快捷键](/zh/config/theming)。

## 在子会话之间导航

委派会产生真实的子会话，界面把每个观察到的原生子级当作一个完整的会话界面，而不是一个详情弹窗。

| 绑定 | 按键 | 移动 |
| --- | --- | --- |
| `session_child_first` | `<leader>down` | 进入第一个直接子级 |
| `session_child_cycle` | `<leader>right` | 下一个同级 |
| `session_child_cycle_reverse` | `<leader>left` | 上一个同级 |
| `session_parent` | `<leader>up` | 返回父级 |

每个子级都保有自己的编辑区草稿。在一个正在运行的子级中按 Enter，会把文本准入该子级的持久 inbox 并引导它的活跃回合；在它结算之后按 Enter，会以其解析出的 Agent、模型、强度、权限和血缘唤醒同一个子级身份。子级中的文本是字面文本，所以在子级里输入 `/help` 会被发送给该子级，而不是作为根命令执行。

产品 Agent 调用和 workflow 投影不会被呈现为可续跑的子对话。

## 斜杠命令

原生会话命令在 Markdown command 和 Skill 之前解析，因此用户工作流无法遮蔽运行时控制命令。

| 命令 | 用途 |
| --- | --- |
| `/compact` | 通过持久压缩流水线压缩历史 |
| `/goal [目标 \| action]` | 设置、查看或管理持久 Goal；使用 `/goal help` 查看语法 |
| `/plan` | 进入 Plan 模式，或在已处于规划中时确认开始工作 |
| `/start-plan` | 立即进入只读的 Plan 模式 |
| `/start-work` | 复核持久 plan 并确认开始实现 |
| `/preset` | 切换已配置的模型团队，或选择一个 |
| `/council` | 运行一个原生的多 Agent Council 预设 |
| `/undo` | 恢复到上一个已完成回合之前的 worktree |
| `/redo` | 重新应用最近被撤销的回合 |
| `/stop` | 停止一个后台终端，或选择一个 |
| `/new` | 打开一个空的对话外壳 |
| `/subagent` | 检查席位与节点进度 |
| `/memory` | 复核、编辑、批准、拒绝、移除和撤销持久记忆变更 |

TUI 的 `/goal <目标>` 与 ACP 使用同一个持久宿主命令。尚无 Goal，或上一条 Goal
已完成、已取消时，它会创建新 Goal；其他状态下则更新当前 Goal。`/goal show`、
`/goal edit ...`、`/goal complete` 等显式 action 仍然可用。目标变化也会同步活跃
Plan：上一活跃 epoch 未完成的步骤会转为 `completed`，并在标题前加 `Superseded:`；
活跃 Plan 会绑定当前 `goal_id`，多阶段工作会建立新的 epoch。原子目标不会改绑已终态
的历史 Plan。

资源选择器沿用同一套命名：`/model`、`/agent`、`/session`、`/skill`、`/theme`、`/mcp`、`/diff`、`/commands`、`/help`。

`/council` 只在当前 Agent 的最终能力快照确实能触达 `council_run` 时出现，因此选择器不会公布一次调度器会拒绝的运行。

## 权限询问与提问

由工具发起的人类输入会取代编辑区，而不是新增一张对话卡片。权限询问会报告正在等待批准；Plan 中的结构化提问会报告正在等待回答。普通 Work 不会挂起这一交互，只有在不存在安全默认值时，才会在回合边界直接提问。

权限选项接受左右键、上下键别名、Enter 和鼠标选择；显式展开会把询问移到一个更大的浮层。提问会显示 `Question i/n`、剩余未回答数量、编号选项，以及一个编号的 `Other` 输入项，逐题游标与自定义草稿在导航中保留。取消其中任何一个都会把该工具解析为一次带类型的拒绝，绝不会伪造答案。

## 鼠标与滚动

当 `mouse` 缺省或为 `true` 时，Zuno 捕获按下、拖动、释放和滚轮事件。释放拖动会通过配置的剪贴板复制选区，并保留高亮可见。对话记录的选区会被夹住，不会越入侧边栏；折叠行可点击；内容溢出的对话会挂载一个可拖动的滚动条。

滚轮输入起始是精确的：第一格移动一行，随后持续的快速手势会加速。`scroll_speed` 改为选择一个恒定倍数；`scroll_acceleration.enabled` 显式选择速度加速，且在两者同时存在时优先。

在 `tui.json` 中设置 `"mouse": false` 可把拖动选择交回终端。

## 参见

- [主题与快捷键](/zh/config/theming)
- [无界面运行](/zh/guide/headless)
- [图像与文件引用](/zh/guide/attachments)
- [zuno tui](/zh/cli/tui)
