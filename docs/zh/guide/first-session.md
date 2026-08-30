# 你的第一个会话

本页走完一个真实会话，并解释运行时在每一步做了什么。任务刻意选得很小：修一个失败的测试。真正重要的是它背后的机制，因为出问题时你要推理的正是这些。

## 启动终端应用

```sh
cd /srv/projects/api
zuno
```

打开欢迎界面不会创建任何持久会话。一个交互式新会话会在进程内存中以稳定身份准备好，浏览模型、Agent 或主题都不会写入任何一行数据。`session` 行及其第一条用户消息，会在第一次面向模型的提交时于同一个事务中插入。

这就是为什么你打开后又放弃的会话不会出现在 `zuno session list` 里。

## 提交第一条提示词

```text
The test users_list_is_paginated fails. Find out why and fix it.
```

空闲状态下按 Enter 会启动一个回合。在 provider 请求发出之前，若干件事按顺序发生：

1. 提示词在一个 SQLite 事务中提交到持久事件日志与 inbox。
2. 指令文件被发现：先是全局 `AGENTS.md`，然后是从 worktree 根目录一路向下到当前目录的项目文件。
3. Skill 目录以有界的元数据形式组装，而不是把每个 `SKILL.md` 正文都塞进去。
4. 宿主根据最终对 provider 可见的工具集生成开发者指令分段，并带上稳定的分段标识符。
5. 完整的 hook 后提示词连同内容摘要一起落盘。

只有到这时请求才被发出。模型能看到的一切事后都可重建：

```sh
zuno debug prompt --step 1
```

加上 `--show-sensitive` 可原样包含指令、AGENTS、skill 和记忆内容。在把这份输出粘贴到任何地方之前，请把它当作敏感信息对待。

## 观察工具调用

典型的第一步是只读调查：用 `grep` 搜测试名、`read` 那个文件、`read` 处理函数。这些被归类为 `ReadOnly`，因此不会触发副作用门禁，并且在模型一次性发出它们时可以重叠执行。无论谁先完成，结果都按模型的调用顺序落盘。

接着 Agent 想运行测试。`shell` 是有副作用的，于是两件互相独立的事发生了：

- **权限**门决定这次调用是否被准入。在默认的 `standard` 模式下，配置的规则和常规风险门禁生效。
- **沙箱**决定被准入的进程能触达什么。在默认的 `workspace-write` 下，宿主根目录只读，工作区可写。

准入一次调用不会放宽沙箱，而一个宽松的沙箱也不会跳过权限门。参见[权限与沙箱](/zh/guide/permissions)。

## 回答一次权限询问

当某条规则是 `ask` 时，编辑区会被这次询问取代，而不是新增一张对话卡片。左右键在选项间移动，Enter 选中，鼠标选择也可用。取消会把该工具解析为一次带类型的拒绝；它绝不会伪造一个批准。

拒绝是一种生命周期结果，不是崩溃。持久的工具状态会保留 `outcome: "blocked"`，并带上 `invalid_arguments`、`unavailable` 或 `denied` 这样的阻塞类别，因此对话记录可以明确说明该效果从未执行，而不是暗示这个工具执行到一半失败了。

## 无需等待即可引导

你不必等回合结束。

| 按键 | 效果 |
| --- | --- |
| 回合进行中按 `Enter` | 准入一个 FIFO 队列项，留给下一个回合 |
| `Ctrl+Enter` | 引导：请求在最近的安全步骤边界处做一次软中断 |
| `Shift+Enter`、`Alt+Enter`、`Ctrl+J` | 插入换行 |
| `Escape` | 中断当前回合；再按一次确认 |

只有在 SQLite 提交之后，某一项才会被报告为已排队。待处理项可以按 revision 编辑或取消，并且能在进程重启后存活。引导可以唤醒一个 provider 流或一段重试等待，但已经在执行的工具不会被抛弃：它的结果会先到达下一个工具安全点。

## 让它编辑并验证

Agent 通过 `apply_patch` 提出补丁，然后重跑测试。默认的模型编辑面是 `apply_patch`，外加用于新文件或有意整体替换的 `write`。

`apply_patch` 会在第一次文件系统改动之前，对照当前文件字节校验每个 section，因此过期的上下文会让整个补丁失败，而不是只应用一半。这正是「重新读文件，然后用更小的补丁重试」成为正确恢复方式的原因，而不是重放同一份 diff。

## 读结果

最后几行会报告解析出的 Agent、目录中的模型显示名，以及配置的推理强度。上下文占用率是最近一次完整的 provider 提示词除以目录中的上下文上限，它在每次 provider 报告时被替换，而不是在整个会话中累加。

累计的 Token 桶保留在用量投影和侧边栏中。

## 稍后续跑

会话是持久的，所以继续并不是一次新的对话：

```sh
zuno run --continue "now cap the page size at 100"
```

```sh
zuno tui --continue
zuno tui --session ses_1a2b3c
```

续跑会重建持久的对话记录、plan 与 todo 状态、待处理的 inbox 输入，以及委派产生的任何子会话。原本待触发的重试截止时间会从 SQLite 重建，而不是丢失。

## 行为不对时该检查什么

```sh
zuno debug permissions
zuno debug prompt --session ses_1a2b3c --step 3
zuno debug agent build
zuno session list --no-roots
```

这四条能回答大多数问题：生效的策略是什么、模型实际收到了什么、这个 Agent 解析出的能力面是什么，以及委派是否创建了你还没看过的子会话。

## 参见

- [会话与回合](/zh/guide/sessions)
- [终端应用](/zh/guide/tui)
- [工具](/zh/guide/tools)
- [排查故障](/zh/operate/diagnostics)
