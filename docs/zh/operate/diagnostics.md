# 诊断

Zuno 的 `debug` 子命令只回答一个问题：这套安装实际认为什么是真的？它们都是读取并报告的
界面，因此运行其中任何一个都是安全的，绝不会修改 session 状态。

当行为与配置不一致时就用它们。几乎每个令人困惑的情况都有一个共同点：对某个合并结果的
假设从未被打印出来。

本页按症状组织。[zuno debug](/zh/cli/debug) 是每个子命令的完整选项参考。

## 我改了配置却没有效果

先看这个可执行文件解析出了哪些根目录。第二套安装、使用了不同数据库的 channel 构建，或者
一个意料之外的 `ZUNO_CONFIG_DIR`，都会表现为「我的修改被忽略了」。

```sh
zuno debug paths
zuno debug config
```

`debug paths` 报告解析后的数据、配置和日志路径。`debug config` 打印合并后的文档，这是唯一
可靠地看清各层如何组合的方式 —— 对象递归合并，而数组和标量是替换，因此你以为会被扩展的
数组可能实际上被替换了。参见 [文件与优先级](/zh/config/files)。

如果 `debug config` 显示了你的取值但行为仍不一致，那么这个取值正在下游被某个 Agent 契约
或某条权限规则收窄。继续看下面的 `debug agent` 或 `debug permissions`。

## 一次工具调用被阻止或意外弹出询问

```sh
zuno debug permissions
```

它同时报告已配置的模式和生效的模式。当某个 Agent 契约收窄了权限，或者
`danger-full-access` 在起作用时，两者会不一致，而这个差异通常就是答案。显式的拒绝在每种
模式下都是终态，包括 `allow_all`，因此你为放宽限制而添加的规则无法覆盖别处的一条拒绝。

完整的权限模型见 [权限与沙箱](/zh/guide/permissions)。

## 某个 Agent 的行为与另一个不同

```sh
zuno debug agent build
```

`debug agent <name>` 打印该 Agent 的生效契约：解析出的模型路由、工具可见性、权限规则集，
以及经 Agent 过滤后的 Skill 视图，包含元数据与已选正文预算、已渲染/已省略/已截断的覆盖
情况，以及一段有界预览。

在改动 `tools`、`permission`、`requiredSkills` 或某个预设之后都应读一次。解析后的集合是
多层作用的产物，仅凭配置无法可靠预测。参见 [自定义 Agent](/zh/config/custom-agents)。

## 某个 Skill 没有被触发

```sh
zuno debug skill
```

这是原始发现结果，位于 Agent 过滤之前。它的输出会显式报告
`view.kind: "raw_discovery"`、`agentFiltered: false` 和
`extensionOverlayApplied: false`。`skills` 数组保留来自不同来源的同名条目，`summary`
报告来源数、已描述数和唯一名称数，以及有歧义的名称。

三种发现结果及其含义：

| 发现结果 | 原因 |
| --- | --- |
| Skill 不存在 | frontmatter 无效，或该根目录不在发现顺序中 |
| 存在但没有描述 | 缺少 `description`，因此它对模型可见的目录隐藏 |
| 被列在有歧义的名称下 | 两个来源声明了同一名称，这会禁用直接的斜杠形式 |

读取之前先重启，因为输出反映的是当前这个进程的发现结果。如果 Skill 出现在这里但某个具体
Agent 不使用它，就与 `zuno debug agent <name>` 对比。参见
[编写 Skill](/zh/config/authoring-skills)。

## 模型没有遵循某个指令文件

```sh
zuno debug prompt
zuno debug prompt --session ses_1a2b3c --step 2
zuno debug prompt --show-sensitive
```

每一个模型可见的提示词分段都被持久记录，因此这是一个有事实答案的问题，而不是推断。
`--session <ID>` 选择一个 session，默认取最新的 receipt；`--step <N>` 在其中选择一个从
1 开始编号的 Provider 请求步骤。

`--show-sensitive` 会包含模型可见的 instruction、AGENTS、skill 和 memory 内容。把这些输出
粘贴进工单之前请把它当作敏感信息处理。不带该标志时这些分段仍会被列出，通常已经足以回答某个
文件是否被包含。

发现规则决定了本应包含哪些内容，参见 [指令与 AGENTS.md](/zh/config/instructions)。

## 某条 shell 命令只在 Zuno 下失败

探测该约束模式在这台宿主上是否真的可部署：

```sh
zuno debug sandbox --mode workspace-write
zuno debug sandbox --mode read-only --check
zuno debug sandbox --mode workspace-write --network allow
zuno debug sandbox --mode workspace-write \
  --sandbox-on-unavailable run-unconfined
```

| 选项 | 取值 | 默认值 |
| --- | --- | --- |
| `--mode <MODE>` | `read-only`、`workspace-write`、`danger-full-access` | `workspace-write` |
| `--network <NETWORK>` | `deny`、`allow` | 受约束模式为 `deny`，`danger-full-access` 为 `allow` |
| `--sandbox-on-unavailable <ACTION>` | `deny`、`run-unconfined` | `deny` |
| `--check` | 当请求的策略无法部署时以非成功状态退出 | |

受限模式会验证 bubblewrap 的部署情况，因此这条命令能区分「我的配置错了」和「这台宿主
无法强制我所要求的模式」。在 CI 或健康检查中使用 `--check`，因为那里非零退出码比输出更有用。

JSON 报告会把请求策略与执行解析分开。重点检查 `requestedMode`、`requestedNetwork`、
`effectiveMode`、`effectiveNetwork`、`fallbackEligible`、`resolutionKind` 和
`fallbackReason`。因此一次符合条件的 `run-unconfined` 结果可以对请求的约束报告
`ready: false`，同时显示 `resolutionKind: "unavailable_fallback"` 与实际宿主权限。

`--check` 始终保持严格：只要请求的约束无法部署，它就以失败退出，即使运行时允许降级。
因此它仍可以安全地作为部署门禁，而不会误把一台无沙箱宿主验证为合格。

约束语义本身见 [权限与沙箱](/zh/guide/permissions)。

## 文件搜索返回的集合不对

搜索后端有自己的忽略规则处理，因此它看到的内容并不总等于普通 shell glob 看到的内容：

```sh
zuno debug rg files --query harness --limit 20
zuno debug rg files --glob '*.rs' --limit 50
zuno debug rg search 'ToolReplayPolicy' --glob '*.rs' --limit 20
```

| 子命令 | 参数 | 选项 |
| --- | --- | --- |
| `rg files` | | `--query <QUERY>`、`--glob <GLOB>`、`--limit <LIMIT>` |
| `rg search` | `<PATTERN>` | `--glob <GLOB>`、`--limit <LIMIT>` |

如果某个文件在这里缺失，那是被忽略规则排除，而不是被查询排除。`zuno excluded` 直接报告
排除决策。

## 诊断或符号缺失

```sh
zuno debug lsp diagnostics src/main.rs
zuno debug lsp symbols ToolRegistry
zuno debug lsp document-symbols file:///abs/path/src/main.rs
```

| 子命令 | 参数 |
| --- | --- |
| `lsp diagnostics` | `<FILE>` |
| `lsp symbols` | `<QUERY>` |
| `lsp document-symbols` | `<URI>` |

`document-symbols` 接受 URI 而不是路径 —— 这是它返回空结果最常见的原因。`diagnostics`
输出为空通常意味着该文件类型没有配置或启动 language server；用 `zuno debug config` 检查
`lsp` 键。

## 需要检查或撤销一次编辑

```sh
zuno debug snapshot track
zuno debug snapshot diff <HASH>
zuno debug snapshot patch <HASH>
```

`track` 报告快照存储当前持有的内容。`diff` 显示某个快照的改动，`patch` 打印它的补丁，
两者都接受一个 `<HASH>`。记录快照就是为了让编辑可以撤销；顶层的 `snapshot` 键控制是否
记录快照，默认为 true。

## 从任何子命令获得更多日志细节

每个 `debug` 子命令都接受这些全局选项：

| 选项 | 取值 |
| --- | --- |
| `--print-logs` | 在结构化本地日志存储之外，把日志打印到 stderr |
| `--log-level <LOG_LEVEL>` | `TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` |
| `--sandbox <SANDBOX>` | `read-only`、`workspace-write`、`danger-full-access` |
| `--sandbox-on-unavailable <ACTION>` | `deny`、`run-unconfined` |

```sh
zuno debug config --print-logs --log-level DEBUG
```

日志默认进入结构化本地存储；`--print-logs` 增加 stderr，但不会禁用该存储。存储的位置与
保留策略见 [日志](/zh/operate/logging)。

## 参见

- [zuno debug](/zh/cli/debug)
- [权限与沙箱](/zh/guide/permissions)
- [配置总览](/zh/config/)
- [日志](/zh/operate/logging)
