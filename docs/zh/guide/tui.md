# 终端应用

`zuno tui` 是交互界面，也是不带参数的 `zuno` 所运行的东西。它是持久运行时之上的一个视图，而不是一个自带 Agent 循环的客户端，这就是为什么你在其中看到的一切事后都能从会话事件重建。

```sh
zuno
zuno tui --continue
zuno tui --session ses_1a2b3c --sandbox read-only
zuno tui --model openai/gpt-5 --prompt "review the diff on this branch"
```

## 后台 TUI 与 SSH 重连

`zuno tui --background` 会启动或复用当前用户的 supervisor，在一个保留型伪终端中运行真正
的 `zuno tui`，再把当前终端连接上去。supervisor 拥有子进程与 scrollback，attachment
只拥有当前 SSH 终端。因此 SSH 断开只会关闭 attachment，不会向 TUI 发送 shutdown。

```sh
zuno tui --background
# 用 Ctrl+] detach

zuno tui --background-list
zuno tui --attach pty_01abc...
```

重新连接会从头重放保留的终端内容，然后无缝切换到实时输出。连接期间客户端也会转发终端尺寸
变化。`--background-stop <pty-id>` 终止一个后台 TUI；
`--background-shutdown` 停止 supervisor 及其拥有的全部 PTY。

控制 server 只绑定 `127.0.0.1` 的系统分配端口。它使用随机 Basic-auth 密码；在 Unix 上，
密码位于 mode `0700` 数据目录中的 mode `0600` 状态文件。连接还需要一张带作用域、单次使用的
PTY ticket。只有这个显式的 loopback 控制面会绕过环境代理。Unix 通过 `nohup` 加新进程组脱离
SSH，Windows 使用 detached process flags。

保留型终端维持的是同一个 TUI 进程，活跃回合也随之继续。它不同于普通 `--session` 续跑：
后者是在旧进程结束后启动新进程，并从 SQLite 重建会话视图。

## 屏幕区域

| 区域 | 内容 |
| --- | --- |
| 对话记录 | 持久的 assistant 内容、工具卡片、错误、中断标记 |
| 侧边栏 | 会话、job、用量以及持久的子会话 |
| 队列停靠栏 | 活跃工作期间固定在编辑区上方的持久 FIFO 后续消息 |
| 编辑区 | 你正在起草的输入 |
| 身份行 | 解析出的 Agent、目录中的模型显示名、配置的推理强度 |
| 末行 | 实时控制面：回合脉冲、中断按键、提示词占用率、命令按键、Agent 与模型徽标 |

身份行会跟在较短回复的末尾，一旦内容填满视口，它就会吸附在编辑区上方。末行以中性徽标重复当前的 Agent、模型和强度，因此在回合运行期间，为下一回合所做的选择仍然可见。按 Tab 会立即更新这个徽标，而真正的宿主替换仍推迟到回合边界。以 `--continue` 或 `--session` 打开应用时，身份行显示的是该会话上次使用的 Agent、模型与推理强度；`--model` 或 `--agent` 参数在本进程内优先于保存的值。

短暂的「working」行不会插入对话记录。持久活动、错误、中断标记和 assistant 内容会。

Plan 与 Todo 侧边栏由提交 SQLite mutation 的同一个 `WorkStateObserver` 推送。挂载的 root
按 session id 过滤后立即应用精确的新 revision 并唤醒界面；侧边栏不再等待 provider 回合结束，
child Plan 也不能覆盖 root 面板。

上下文占用率是最近一次完整的 provider 提示词除以目录中的上下文上限。它在每次 provider 报告时被替换，而不是在整个会话中累加；累计的 Token 桶位于用量投影与侧边栏。

## 提交、排队与引导

| 按键 | 空闲时 | 回合进行中 |
| --- | --- | --- |
| `Enter` | 启动一个回合 | 准入一个 FIFO 队列项，留给下一个回合 |
| `Ctrl+X` 后按 `Enter`（支持时也可用 `Ctrl+Enter`） | 发送草稿；输入框为空时选择队列项 | 把草稿或选中队列项送入当前回合 |
| `Shift+Enter`、`Alt+Enter`、`Ctrl+J` | 换行 | 换行 |
| `Escape` | — | 中断；再按一次确认 |

只有在 SQLite 提交之后，某一项才会被报告为已排队。最早的条目会按持久 FIFO 顺序固定
在编辑区正上方，并标记为 `next` 或 `steer`。停靠栏显示实际生效的
`input_force_submit` 绑定，而不是假定终端支持 `Ctrl+Enter`，同时显示队列管理器按键。
打开 `/queue`，用上下键或鼠标选中任意一条，再使用显示的立即发送按键，或点击
**Send selected now**。发送的是选中行，不会误发输入框草稿，也不局限于队首；其他行保持原顺序。
待发草稿可以按 revision 编辑或取消，文字编辑会保留已准入的图片附件，进程重启后仍存在。
已准入当前回合的 steering 内容固定下来，但消费前仍可取消。条目被提升后才进入对话
历史；取消只会移除条目，不会伪装成已经发送。
操作绑定显示的行 revision。其他客户端编辑行时会解除旧取消确认，不会未重新确认就取消
更新后的内容。

引导可以唤醒 provider 流或重试等待：Zuno 为部分 assistant 输出打检查点、提升持久输入，
然后在**同一个回合**中开始下一个模型步骤，不调用 Stop、不硬中断、不重建会话。
正在执行的工具先安全结束，其结果不会因引导而丢失。已准入但因回合终止未消费的条目仍保留在队列。

如果显示的 turn 在**准入前**结束或改变，立即发送会被拒绝：原队列条目的 revision 和顺序不变；
新草稿连同粘贴块、图片一起恢复。若已经输入了更新的草稿，不会覆盖它，失败草稿会保留到该
输入框清空后再恢复；不会自动改投另一个回合。独立的消费回执确认消息确实进入了模型输入。

## 输入历史与粘贴

输入框聚焦时，上下键不会滚动对话。在**整个文本缓冲区**的开头或末尾，两个方向都用于浏览
已提交的输入历史；位于内部时移动光标。回到最新历史之后，会恢复原草稿的光标、选区、
撤销状态、粘贴内容与图片归属。弹窗上下键仍操作弹窗选项；滚动对话请用滚轮、Page Up/Down
或对话详情视图。

多行粘贴作为一个完整文本块处理，CRLF、CR 统一为 LF，末尾换行也保留；块内换行不会提交。
优先使用 bracketed paste，旧终端的连续按键突发在快捷键分发前聚合。只有粘贴完成后另一次
明确的 Enter 或立即发送操作才提交。长粘贴可以显示折叠占位符，但发送的是完整内容。

Windows 本地剪贴板优先使用 PowerShell 7（`pwsh.exe`），缺失时使用 `powershell.exe`，
读写均异步进行。读取期间不会提交不完整草稿；若读取完成前输入内容、光标或会话已变化，
不会把旧结果插入新目标。

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

可打印输入始终只有一个所有者。弹窗打开时，未被快捷键认领的文本只进入当前弹窗，不会同时
写进背后的编辑区。终端可能把一次按键报告为 Press、Repeat 与 Release：Press 写入一次，
Repeat 保留正常的长按连输语义，Release 永远不写入文本。因此 Windows 上的一组
press/release 事件在模型、会话、Agent、主题、Skill 等可搜索弹窗中只会产生一个字符。

`Ctrl+C` 与 `Ctrl+D` 都需要确认。第一次按下后，末行显示
`ctrl+c again to exit` 或 `ctrl+d again to exit`；必须在 1.5 秒内再次按下同一个组合键。
回合运行中第一次按键还会请求硬中断。换成另一个组合键，或超过时间窗口，只会重新开始确认，
不会意外退出。`Ctrl+]` 属于外层后台 attachment，只做 detach，不会把退出键发送给被保留的
TUI。

`leader_timeout` 默认 5000 毫秒，因此续接提示的浮层会保持可读五秒，除非有另一个按键完成或取消该序列。浮层打开期间的交互会重置这个截止时间。重新绑定见[主题与快捷键](/zh/config/theming)。

## 在子会话之间导航

委派会产生真实的子会话，界面把每个观察到的原生子级当作一个完整的会话界面，而不是一个详情弹窗。

| 绑定 | 按键 | 移动 |
| --- | --- | --- |
| `session_child_first` | `<leader>down` | 进入第一个直接子级 |
| `session_child_cycle` | `<leader>right` | 下一个同级 |
| `session_child_cycle_reverse` | `<leader>left` | 上一个同级 |
| `session_parent` | `<leader>up` | 返回父级 |

每个子级都保有自己的编辑区草稿。子级运行中普通 Enter 只进入该子级的持久队列，
显式“立即发送”才引导它当前显示的回合；结算之后 Enter 会以其解析出的 Agent、模型、
强度、权限和血缘唤醒同一个子级身份。子级中的文本是字面文本，因此 `/help` 也作为消息发送，
不会作为根命令执行。

根 Agent 还会得到仅根会话可见的 `session_message` 工具。它可以向同一项目中的另一个根会话，
或当前根会话的后代发送持久 peer context。子 Agent 看不到发送工具；即使重放了过期 schema，
运行时仍会拒绝 child 源、跨项目、发送给自己、已归档目标，以及其他根会话的 child。消息会明确
标注为 peer context，而不是用户授权。活跃 root/child 在下一个安全点接收；空闲 TUI 轮询持久
inbox 并启动目标回合；离线目标则一直保留 queued 行，直到再次加载。

产品 Agent 调用和 workflow 投影不会被呈现为可续跑的子对话。

## 斜杠命令

原生会话命令在 Markdown command 和 Skill 之前解析，因此用户工作流无法遮蔽运行时控制命令。

| 命令 | 用途 |
| --- | --- |
| `/compact` | 通过持久压缩流水线压缩历史 |
| `/goal [目标 \| action]` | 设置、查看或管理持久 Goal；使用 `/goal help` 查看语法 |
| `/plan` | 幂等进入 Plan 模式 |
| `/start-plan` | 立即进入只读的 Plan 模式 |
| `/start-work` | 授权精确的 handoff-ready Plan revision 并开始实现 |
| `/preset` | 切换已配置的模型团队，或选择一个 |
| `/council` | 运行一个原生的多 Agent Council 预设 |
| `/undo` | 恢复到上一个已完成回合之前的 worktree |
| `/redo` | 重新应用最近被撤销的回合 |
| `/stop` | 停止一个后台终端，或选择一个 |
| `/new` | 打开一个空的对话外壳 |
| `/subagent` | 检查席位与节点进度 |
| `/memory` | 复核、编辑、批准、拒绝、移除和撤销持久记忆变更 |
| `/memories` | 配置当前会话是否使用记忆、是否生成学习 |
| `/learn [action]` | 查看或管理经验、反馈、模式和待审 Skill 候选 |
| `/reflect [turn\|session]` | 手动运行持久、无工具权限的学习提取器 |

TUI 的 `/goal <目标>` 与 ACP 使用同一个持久宿主命令。尚无 Goal，或上一条 Goal
已完成、已取消时，它会创建新 Goal；其他状态下则更新当前 Goal。`/goal show`、
`/goal edit ...`、`/goal budget <正整数 token|none>`、`/goal complete` 等显式
action 仍然可用。目标变化也会同步活跃 Plan：多阶段工作会归档此前可见 Plan，并安装一个绑定当前 `goal_id` 的新根 Plan。
原子目标不会改绑已终态的历史 Plan；属于上一个 Goal 的终态 Plan 会归档为已完成的历史。

Zuno 的通知——无法抓取的远程规则文件、被 token、工具调用次数或墙上时间额度停下的回合、
预算策略要求的压缩——以 toast 显示，级别跟随通知的 severity（`info`、`warning`、`error`），
末尾带方括号内的通知 code。它们不是模型输出。

资源选择器沿用同一套命名：`/model`、`/agent`、`/session`、`/skill`、`/theme`、`/mcp`、`/diff`、`/commands`、`/help`。

`/session` 会以目标会话自己保存的 Agent、模型与推理强度重新打开它——当前会话的选择不会跟过去；而 `/model`、`/agent`、`/preset` 或强度的选择会写回当前会话，下一次续跑就从这里开始。若目标会话的 Shell 会被本平台拒绝，则保留当前会话并显示 `warning: keeping the current turn host:`，而不是先拆掉它。

`/council` 只在当前 Agent 的最终能力快照确实能触达 `council_run` 时出现，因此选择器不会公布一次调度器会拒绝的运行。

### `/undo` 覆盖什么

`/undo` 与 `/redo` 在 Zuno 于一个回合前后捕获的两棵树之间移动整个 worktree。捕获不局限于
启动 Zuno 时所在的目录，因此在子目录里启动的会话也能恢复它旁边的文件。

快照并不包含每个文件。有三类路径被排除在外，恢复操作从不改动它们：

- 大于 2 MiB 的未跟踪文件；
- 在拍快照的那一刻被某条 `.gitignore` 规则覆盖的路径；
- Git 无法读取的路径。

被排除的路径保留它原有的内容，因此一次成功的恢复也可能让 worktree 的一部分看起来没有被
动过。Zuno 会在成功恢复后打印的那行里数出它们：

```text
undo complete: 3 file(s) restored to tree 250c08c795d9 (1 created, 1 modified, 1 deleted); 2 path(s) are outside this snapshot and were not restored: 1 over the 2 MiB untracked-file limit, 1 matching an ignore rule
```

如果被列出的某个路径对你很重要，请从你自己的版本控制或备份中恢复它。快照存储从未持有过
它的副本。

失败或被中断的回合同样会拿到自己的快照，因为它通常已经写过文件了。

一次恢复还可能以**不确定的结果**结束：文件已经被改写，但事后无法确认到达了所请求的那棵树。
Zuno 会如实报告这一点，而不是把它说成一次拒绝，同时把 `zuno-restore-uncertain.json` 写入
快照存储，并拒绝之后的每一次 `/undo` 和 `/redo`，直到那份记录被清除。任何操作都不会被自动
重试。请阅读那份记录、对照你的 worktree、自己解决差异，然后删除该文件以重新启用恢复。

## 权限询问与提问

由工具发起的人类输入会取代编辑区，而不是新增一张对话卡片。权限询问会报告正在等待批准；Plan 中的结构化提问会报告正在等待回答。普通 Work 不会挂起这一交互，只有在不存在安全默认值时，才会在回合边界直接提问。

权限选项接受左右键、上下键别名、Enter 和鼠标选择；显式展开会把询问移到一个更大的浮层。提问会显示 `Question i/n`、剩余未回答数量、编号选项，以及一个编号的 `Other` 输入项，逐题游标与自定义草稿在导航中保留。取消其中任何一个都会把该工具解析为一次带类型的拒绝，绝不会伪造答案。

## 鼠标与滚动

当 `mouse` 缺省或为 `true` 时，Zuno 捕获按下、拖动、释放和滚轮事件。释放拖动会
通过配置的剪贴板复制选区，并保留高亮可见。Windows 本地优先使用原生剪贴板，助手程序
完成写入后才提示成功。SSH 远端优先使用 OSC 52；由于协议没有成功回执，只提示“复制请求已发送”。
终端写入失败时，Zuno 回退到一个
本地助手程序——macOS 上是 `pbcopy`，Linux 上是 `wl-copy`、`xclip` 或 `xsel`，
Windows 上是通过 PowerShell 的 `Set-Clipboard`。没有可用机制时会明确报告失败，
不会伪装成复制成功。
同一个串行剪贴板执行器跨会话切换复用；更新的复制尝试失败后，旧回执不能冒充当前选区
复制成功。
每个助手程序都把选区当作剪贴板数据从标准输入读入，绝不当作要执行的脚本，因此一次
复制永远不会执行对话记录里的内容。选区高亮与复制共享最后一帧的行／字素映射，覆盖中文、
组合字符和 emoji。复制可见 Markdown 文本，不按原始 Markdown 的字符位置切片；
说话者标签、边框、填充和终端软折行不会进入剪贴板，明确的内容换行会保留。
选区会被夹住，不会越入侧边栏；折叠行可点击；内容溢出的对话会挂载一个可拖动的滚动条。

滚轮输入起始是精确的：第一格移动一行，随后持续的快速手势会加速。`scroll_speed` 改为选择一个恒定倍数；`scroll_acceleration.enabled` 显式选择速度加速，且在两者同时存在时优先。

在 `tui.json` 中设置 `"mouse": false` 可把拖动选择交回终端。只有对话滚动拥有方向键时才启用
alternate scroll；输入框或弹窗聚焦时不启用。

退出时会先关闭自己开启的捕获模式，再丢弃尚未读取的输入，因此在会话收尾期间到达的点击或
滚轮事件不会以 `0;54;31M` 这类残留报文出现在 Shell 提示符上。

## 参见

- [主题与快捷键](/zh/config/theming)
- [无界面运行](/zh/guide/headless)
- [图像与文件引用](/zh/guide/attachments)
- [zuno tui](/zh/cli/tui)
