# 通过 ACP 在 Zed 中使用 Zuno

Zuno 通过标准输入与标准输出暴露一个原生的 Agent Client Protocol（ACP）server。Zed 可以把该 server 作为自定义外部 Agent 启动。上游 Zed 的配置契约记录在 [External agents](https://zed.dev/docs/ai/external-agents)；Zuno 已实现的协议边界与已固定的上游证据记录在 [Zed ACP integration](https://github.com/sunerpy/zuno/blob/main/docs/design/zed-acp-integration.md)。

## 1. 验证已安装的 Zuno 二进制

定位 Zed 应当启动的那个二进制：

```sh
# Linux and macOS
command -v zuno
zuno acp --check
```

```powershell
# Windows PowerShell
(Get-Command zuno).Source
zuno acp --check
```

这项检查必须在不启动会话的情况下完成，并打印：

```text
ACP stdio adapter ready (protocol v1; schema v1.21.0)
```

如果终端能找到 `zuno` 而 Zed 找不到，请使用 `command -v zuno` 或 `Get-Command zuno` 报告的绝对路径。桌面应用拿到的 `PATH` 往往与交互式 shell 不同。

## 2. 把 Zuno 添加为自定义 Zed Agent

打开 Zed 的 Agent Panel，打开 Agent Settings，选择 **Add Agent**，然后选择 **Add Custom Agent**。等价的 Zed 设置条目是：

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/absolute/path/to/zuno",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

使用可执行文件的绝对路径最可靠。示例：

### Linux

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/home/you/.local/bin/zuno",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

### macOS

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/Users/you/.local/bin/zuno",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

### Windows

JSON 字符串中的反斜杠需要转义：

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "C:\\Users\\you\\.local\\bin\\zuno.exe",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

不要把这条命令包在一个会向 stdout 写横幅或状态文本的 shell 脚本里。ACP 的 stdout 只包含以换行分隔的 JSON-RPC 帧。

## 3. 选择 Zed 所使用的 Zuno 配置

Zed 把所选项目作为绝对工作目录发送过来。Zuno 解析与它在 TUI 中相同的全局与项目 `zuno.json`/`zuno.jsonc` 链：

- 平台配置根之下的全局配置；
- 来自 worktree 与 `.zuno/` 层的项目配置；
- 已配置的 Agent 定义、Skill、扩展、MCP server、权限、沙箱策略、provider 和模型。

Provider 登录与凭据仍归 Zuno 拥有。请在启动 Zed Agent 之前完成配置与验证：

```sh
zuno debug config
zuno auth list
zuno models
```

不要为了让 ACP 能启动就把 provider 密钥复制进 Zed 设置。请使用 Zuno 的凭据存储，或者 [Provider 与凭据](/zh/config/providers)中描述的 provider 环境变量。

要为这个 Zed Agent 选择一个已有的可切换配置覆盖层，请在自定义 Agent 的环境中设置 `ZUNO_CONFIG_DIR`：

```json
{
  "agent_servers": {
    "Zuno (Kiro profile)": {
      "type": "custom",
      "command": "/absolute/path/to/zuno",
      "args": ["acp"],
      "env": {
        "ZUNO_CONFIG_DIR": "/home/you/.config/zuno/profiles/kiro"
      }
    }
  }
}
```

在 Windows 上请使用转义后的绝对路径。多个 Zed 条目可以用不同的 `ZUNO_CONFIG_DIR` 覆盖层启动同一个 Zuno 二进制。

## 4. 选择 `deep` 或其他会话 Agent

一个新的 ACP 会话会解析 Zuno 常规的默认 Agent 与模型。随后 Zuno 向 Zed 发布这些会话控制项：

- **Mode**：Build 或 Plan；
- **Agent**：可用的实现类 Agent；
- **Model**：来自解析出的 Zuno provider 目录的模型；
- **Reasoning**：`Configured default`，加上所选模型支持的规范级别，例如 Low、High、Extra High 或 Maximum。

要使用可直接选择的 `deep` Agent：

1. 创建一个 Zuno 外部 Agent 线程；
2. 保持 **Mode** 为 **Build**；
3. 打开 **Agent** 配置选择器并选择 `deep`；
4. 如果当前 Zuno profile 暴露了多个模型，选择想要的那个；
5. 当所选模型公布了推理能力时，选择一个推理级别。

Plan 模式总是激活只读的 `plan` Agent。回到 Build 模式会恢复所选的实现类 Agent。Agent 与模型的更改是会话本地的，并且在提示词正在运行时会被拒绝。

`zuno acp` 不接受 `--agent` 启动参数。Agent 选择是一个 ACP 会话配置操作，不是第二个进程级配置面。

## 5. 斜杠命令与 Skill

在会话创建、加载、恢复或一次成功的重新配置之后，Zuno 会发布原生会话控制项、来自其常规命令目录的可执行命令，以及可用斜杠调用的无歧义 Skill。随后 Zed 会在 `/` 补全中暴露它们。

来源与其他 Zuno 界面相同：

- 带真实运行时处理器的原生会话控制项：`/compact`、`/goal`、`/plan`、`/start-plan` 和 `/start-work`；
- Zuno 配置目录下的全局 `command/*.md` 或 `commands/*.md`；
- 项目的 `.zuno/command/*.md` 或 `.zuno/commands/*.md`；
- 拥有真实处理器的内置命令；
- 名称不与命令冲突的已发现 Skill。

`/compact` 不接受参数。它调用与 TUI 相同的持久压缩路径，只在该命令达到终态生命周期事件之后才返回，并且不会把字面的斜杠命令发送给模型。原生控制项优先，因此一个用户定义的名为 `compact` 的命令或 Skill 不会作为第二个有歧义的条目被发布。

`/goal` 暴露与 TUI 相同的持久 goal 处理器。它接受 `show`、`history`、`create <objective>`、`edit <objective>`、`pause`、`resume`、`block <reason>`、`complete` 和 `cancel`；省略动作则显示当前 goal。该命令的输出投影为一条普通的 Agent 消息，而不是推理内容。

`/plan` 在 Build 与 Plan 之间切换。`/start-plan` 直接进入只读的 Plan 模式，而 `/start-work` 返回 Build。离开 Plan 需要存在一个持久 plan，因此过早的交接会显式失败，而不是削弱模式边界。成功的更改会发出 ACP 的 `current_mode_update` 与 `config_option_update` 通知，让 Zed 的选择器保持同步。这些原生命令都不会被发送给模型。

执行 `/name arguments` 使用 Zuno 已有的命令模板或 Skill driver，包括常规的权限与持久会话行为。ACP 不会创建产品特定的 `/dual-review`、`/auto-release` 或其他工作流；用户可以在自己的命令或 Skill 目录中定义它们。

## 6. 图像、选区、分支 diff 与附件

Zuno 公布对 ACP `image` 与 `embeddedContext` 的支持。在 Zed 中这会启用图像附件以及通用的嵌入上下文，例如当前选区、诊断信息、抓取的上下文和分支 diff。

- 内联与嵌入图像支持 PNG、JPEG、GIF 和 WebP，base64 载荷需有效且不超过 5 MiB。
- 嵌入的文本资源会在持久提示词信封中保留它们的 URI、MIME 类型和文本，每个限制为 50 KiB 与 2,000 行。
- 除图像之外的二进制嵌入资源会被拒绝。
- 普通文件引用可能以 `resource_link` 形式到达；Zuno 会让这些字段在持久存储与加载重放中保持类型化。
- 音频仍不受支持，也不会被公布。

所选的 provider/model 也必须公布图像输入能力。ACP 的能力协商无法让一个纯文本模型接受图像。

## 7. 权限、工具、diff 与生命周期

Zed 呈现权限与征询请求，但策略拥有者仍然是 Zuno：

- 由 Zuno 的权限规则决定某个工具是运行、被拒绝还是询问；
- 可复用的 ACP 询问提供 `Allow once`、`Allow for session` 和 `Reject`；会话级授予对权限与资源模式是确切匹配的，能在 Agent/模型/推理重挂载后存活，并由 `session/close` 清除；
- strict 或 Shell 风险类的仅人工询问只提供 `Allow once` 与 `Reject`；生效的 `allow_all`，包括 `danger-full-access`，完全不会发出权限请求；
- Zuno 的 Shell 沙箱控制文件系统与网络权限；
- 原生文件工具为 Zed 发出类型化的创建与编辑 diff；
- 当所选 Agent profile 允许时，Zuno 配置的 MCP server 仍然可用；
- 取消、会话加载、恢复、关闭、plan 状态、用量和工具历史使用与 TUI 相同的持久运行时。

结构化的 `question` 调用使用 ACP 表单控件，而不是通用提示：

- 单选项从 `oneOf` 渲染；
- 多选项渲染为数组选择；
- 当该问题允许自定义答案时，选项仍可点击，并额外显示一个可选的 `Other` 字段；
- 提交 `Other` 优先于已选中的选项，这与 Zuno TUI 一致，而一个空的可选表单会被报告为未回答。

在完成之后以及历史重放时，该问题仍是一张静态工具卡片，显示它的提示、选项、状态，以及在持久答案元数据可用时显示被选中的值。`rawInput` 与 `rawOutput` 在工具详情中仍然可用；加载历史绝不会重新打开一次征询请求。

只有 provider 的推理增量会被投影到 Zed 的 Thinking 界面。生成的标题使用 ACP 的 `session_info_update`，而运维状态或 provider 失败文本由生命周期/错误报告处理，而不是被渲染成模型的思考内容。

历史重放会为将来的 provider 请求保持 provider 推理胶囊持久，但当同一条消息已经包含它可见的推理摘要时，不会再渲染一份完全相同的胶囊副本。仅存在于 provider 侧的推理仍然可见，因此重放去重不会隐藏唯一可用的思考内容。

Shell 工具调用的标题是提交时的确切命令，而不是加了解释器前缀的伪命令。例如，Zed 收到的可复制标题是 `git diff --check`，而解析出的 `zsh` 身份单独通过 `_meta.zuno.interpreter` 提供。完成与历史重放保留同样的形态。

### 被委派的子会话

普通的 `task` 工具始终是兼容界面。它的卡片显示 Agent、目标、状态，并在已知时显示子会话/job/模型/强度身份，同时保留原始工具详情。

Zuno 还支持经过评审的官方 `codex-acp` 适配器所使用的草案版原生子 Agent 投影。它只有在 ACP 客户端发送以下直接 initialize 能力时才启用：

```json
"clientCapabilities": {
  "subagents": {}
}
```

协商成功后，前台委派会被按会话树路由：

- 父级收到 `subagent_spawned`；
- 子会话获得自己的重放、提示词、消息、推理、工具、plan 和用量；
- 在子级输出排空之后，直接父级恰好收到一次终态的 `subagent_state_update`。

嵌套的前台子级使用它们直接的持久父级。历史子级树会在 `session/load` 时恢复，但它们的状态显示为 `disconnected`，因为一个重启后的进程无法证明旧工作仍然存活。针对子级的取消/关闭尚未公布。

后台委派刻意留在稳定的 task/job 生命周期上，即使已经协商了原生子 Agent 也是如此。关闭一个根会话只会取消并合入该根自己的后台 job，然后才释放它的运行时资源。

由子级发起的权限与提问只在原生模式下使用子会话 id。对于不支持原生子 Agent 的客户端，Zuno 会在已知的根会话上发送该请求，并在 `_meta.zuno.childSessionId` 中包含持久的子级 id；这既避免客户端收到一个未知的会话 id，又保留了归属信息。

ACP 提供的客户端 MCP、客户端文件系统 RPC 和终端 RPC 都不会被公布。Zuno 通过自己的工具、权限策略和沙箱处理文件与 Shell 工作，而不是声称实现了 Zed 客户端 RPC 处理器。

恢复一个线程刻意是冷启动的。`session/load` 与 `session/resume` 会校验该会话、暴露它的选择器并发布命令，但不启动 `TurnHost`，也不连接已配置的 MCP server。第一条提示词才执行这次激活。每个打开的 ACP 会话只发送一次加载重放，并受这些上限约束：最新 512 条保留消息、16 MiB 的已存储 part 与总投影预算，以及每次更新 8 MiB 的帧上限。当历史超出这些边界时，Zuno 会发出一条省略通知。已存储的 part blob 会先在 SQLite 中测量大小，再做 JSON 水合，因此一个过大的工具输出不会先被加载进进程内存然后丢弃。

历史文件引用不会仅因为它们是持久的就被信任。只有那些确实存在、且规范化后位于项目 worktree 内的普通文件，才仍然可作为 diff 路径、位置或本地资源链接使用。缺失的、外部的或通过符号链接逃逸的本地资源会显示为不可操作的说明文本。一个 ACP stdio 连接最多保留 32 个打开的会话；`session/close` 会释放该槽位，并关停任何已激活的宿主与 MCP 运行时。

## 8. 排障

### Agent 启动失败

在终端里运行那条确切配置的命令：

```sh
/absolute/path/to/zuno acp --check
```

检查该二进制是否可执行、它的配置/数据目录是否可写，以及它配置的 provider 能否被解析。使用绝对命令路径可以避免大多数 GUI 的 `PATH` 差异。

### provider 或模型缺失

运行：

```sh
zuno debug config
zuno auth list
zuno models
```

如果 Zed 条目使用了 `ZUNO_CONFIG_DIR`，运行这些命令时请使用同样的环境。项目特定的配置取决于在 Zed 中打开的文件夹。

### 协议或工具流格式错误

在 Zed 中运行：

```text
dev: open acp logs
```

若需临时的 Zuno 诊断，把参数改为：

```json
"args": ["acp", "--print-logs", "--log-level", "DEBUG"]
```

`--print-logs` 把诊断写到 stderr。它不会把日志放到 ACP 的 stdout 上。诊断结束后请移除详细日志。

### 反复打开工作区会恢复一个旧线程或占用 CPU

关闭或隐藏 Zed 的 Agent 面板并不一定会发送 `session/close`。Zed 可能在后台保持它的外部 Agent 进程与工作区线程选择存活。

当前的 Zuno 版本会让被恢复的会话在第一条提示词之前保持休眠、对重复的加载重放去重、限定对话记录重放、过滤过期的可操作文件路径，并把一个 ACP 连接的打开会话数限制在 32 个。这些保护措施让 Zuno 不会仅仅因为 Zed 恢复了一个线程就急切地重连 MCP server 或重放一份无界的历史对话。

如果问题仍然存在：

1. 运行 `dev: open acp logs`，确认 Zed 是否在反复发出 `session/load` 或重新打开同一个会话；
2. 关闭那个外部 Agent 线程，而不只是关闭面板；或者停止并重启已配置的 Agent server，让它的 stdio 进程达到 EOF；
3. 如果 Zed 在重启后立刻又选中同一个已知有问题的线程，请在备份 Zed 状态之后，按所安装 Zed 版本的维护流程清除该工作区最后一个活跃的 Agent 线程关联；
4. 单独检查 Zed 日志中反复出现的 worktree、watcher 或 `OpenBufferByPath` 活动。Zuno 不拥有也不移除由 Zed 创建的 worktree 与文件系统 watcher。

一个已激活但空闲的 Zuno 会话会保持挂载，直到 Zed 关闭它或 ACP 进程退出。Zuno 目前不会按空闲计时器降级活跃会话。

### Agent 或模型选择器不见了

先确认 Zed 已成功连接，然后创建一个新的外部 Agent 线程。运行 `zuno acp --check` 验证生产适配器，并检查 ACP 日志中的初始化或会话创建错误。

### 一个 Kiro 提示词以 `unsupported_content_block_projection` 失败

2026-08-28 的 `kiro-provider` 构建接受连续的全文本块，并且仅在 Kiro 最终的标量文本边界处逐字节拼接、不插入任何分隔符。请使用：

```json
"options": {
  "baseURL": "http://127.0.0.1:8787/v1",
  "maxTokens": null,
  "timeout": false,
  "headerTimeout": 330000,
  "chunkTimeout": 210000
}
```

移除过期的 `responsesTextBlocks: "single"` 选项：Zuno 的通用兼容模式会插入一个空行，那会改变当前 provider 的确切投影。文本与非文本块混合、且 Kiro 无法保留其顺序时，仍然会失败即拒绝。如果纯文本仍然产生旧的错误，请确认 Zed 确实连到了新构建的 provider 进程。

`headerTimeout` 与 `chunkTimeout` 刻意超过 kiro-provider 对应的 300 秒请求超时与 180 秒流空闲超时。这让网关能在 ACP 客户端关闭请求之前返回它自己的类型化超时。

`kiro-provider` v0.5.0 及以后版本还会区分可重试的流失败与致命的协议失败。Zuno 只重试 `upstream_stream_error`、`upstream_stream_incomplete`、`upstream_stream_idle_timeout` 和 `request_deadline_exceeded`；旧式的通用 `upstream_error` 不足以授权重试。每次调用都记录在 `session.provider.attempt.1` 之下，并复用同一份持久会话亲和性。由于 ACP 无法撤回一个已追加的消息块，Zuno 只在该次尝试被打上检查点之后才提交 provider 文本、推理和待处理的工具行。一次失败的部分尝试会被丢弃，而不是与它的替代者拼接在一起。

## 9. 验收检查

配置完成之后：

1. 在 Zed 中打开一个真实的项目文件夹并创建一个 Zuno Agent 线程；
2. 选择 `deep`、目标模型，以及 `xhigh` 或 `max`，然后确认该选择显示在会话控制项中；
3. 输入 `/`，确认 `/compact`、`/goal`、`/plan`、`/start-plan` 和 `/start-work` 各出现且仅出现一次；
4. 执行 `/goal create verify ACP`，然后执行 `/goal show`，确认结果作为 Agent 输出出现，而不是作为 Thinking；
5. 执行 `/start-plan`，确认 Zed 切换到 Plan，然后创建一个持久 plan 并执行 `/start-work`；
6. 在积累了足够的对话历史之后，执行 `/compact` 并确认摘要能在一次会话重新加载后存活；
7. 执行一个已配置的命令或一个无歧义的 Skill；
8. 附加一张图像、一段选区和一份分支 diff，确认它们到达了该回合；
9. 发送一个只读的仓库问题，确认已提交的推理、答案和待处理工具行各出现一次；注入一次可重试的流失败，确认失败的部分尝试不存在；
10. 委派一个前台子级，并根据客户端能力确认得到的是协商后的子会话流，或者完整的稳定 task 卡片；
11. 委派一个后台子级，关闭根线程，确认该 job 被取消且没有前台原生子级流；
12. 在 ask 策略下请求一次文件编辑，确认 Zed 同时显示权限请求和类型化 diff；
13. 取消一个正在运行的提示词，确认会话回到空闲；
14. 关闭并重新加载该会话，确认内容、question/task 卡片、子级历史、工具、plan 和用量都被重放且仅一次；
15. 再次加载同一个已打开的会话，确认对话记录没有被重复。

仓库级的 ACP 验证是：

```sh
cargo test -p zuno-acp
cargo test -p zuno-cli --test acp_stdio
```
