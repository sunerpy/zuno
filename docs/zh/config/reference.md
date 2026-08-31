# 配置项参考

## 配置文件

Zuno 只读取 `zuno.json` 与 `zuno.jsonc`。全局文件位于 `$XDG_CONFIG_HOME/zuno`（通常是 `~/.config/zuno`）；项目层是从 worktree 根目录到当前目录的裸 `zuno.json[c]` 文件，以及 `.zuno/` 下的文件。`ZUNO_CONFIG` 追加一个显式文件，`ZUNO_CONFIG_DIR` 追加一个目录，`ZUNO_CONFIG_CONTENT` 提供最后一层环境配置。

对象按优先级从低到高递归合并。数组与标量值直接替换低层值。顶层拒绝未知键。

首次进行普通发现时，Zuno 会把缺失的全局 `zuno.json` 创建为 `{}`，并从 Zuno 自带的起始指引创建缺失的全局 `AGENTS.md`。创建使用排他性新建文件语义，绝不覆盖已有文件。以显式 `ZUNO_CONFIG`、`ZUNO_CONFIG_DIR` 或 `ZUNO_CONFIG_CONTENT` 启动不会生成默认文件，因此仅用 profile 的首次运行之前，应先完成安装或一次普通启动。

### 全局与项目指令

Zuno 按以下顺序加载原生指令文件：

1. `$XDG_CONFIG_HOME/zuno/AGENTS.md`；
2. `ZUNO_CONFIG_DIR/AGENTS.md`，当 profile 目录提供该文件时；
3. 从 worktree 根目录到当前目录的项目目录。

当 `ZUNO_CONFIG_DIR` 选择某个 provider 或团队时，基础全局文件仍然生效。profile 级文件追加范围更窄、优先级更高的指引，而不是替换基础全局规则。在同一个项目目录中，`AGENTS.local.md` 替换 `AGENTS.md`；更近的目录在更后面追加。

起始指引覆盖归属权、验证、范围受限的 Git 操作以及安全的 worktree 决策。详细流程保留在内置的 `git-workflow` 与 `worktree` Skill 中，因此只在相关时才加载。Zuno 不复制 OpenCode、Codex、Claude 或其他产品的指令文件，也绝不覆盖用户维护的全局 `AGENTS.md`。

### 可切换的配置覆盖层

Zuno 目前没有名为 `--profile` 的选项。请改用一个末层配置目录作为可切换的覆盖层。普通启动会保持全局与项目配置不变；设置 `ZUNO_CONFIG_DIR` 会追加一个优先级更高的目录，其中包含 `zuno.json` 或 `zuno.jsonc`：

```sh
zuno

ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

覆盖层是深度合并，因此在其中定义的 provider 是被追加进来，不会删除全局 provider。顶层的 `model`、`small_model` 以及非 null 的 `preset` 会替换其低层值。切换整个 Agent 团队时这个区别很重要：只改 `model` 只会改变根回合，而低层生效的 preset 仍可能把被委派的 Agent 路由到它原本的 provider。不要用 `"preset": null` 作为墓碑值：可选的类型化字段目前把 JSON null 视为「没有更高层的值」，因此继承来的 preset 仍然处于选中状态。正确做法是显式指定一个覆盖层 preset。

一个 `zuno.json` 可以声明多个 provider。provider id 是 catalog 命名空间，而 preset 决定每个 Agent 使用哪条限定的 `provider/model` 路由。已纳入检查的 [`examples/config/zuno-multi-provider.json`](https://github.com/sunerpy/zuno/blob/main/examples/config/zuno-multi-provider.json) 在同一个 catalog 中同时保留 `myopenai` 与 `kiro-local`，并定义了三个团队。

::: tip 本页与英文原文的覆盖范围
安全相关章节（沙箱模式与降级、权限模式与规则、严格 HITL 授权、Agent 能力上限）是完整
翻译，限定条件与英文版逐条一致。

其余章节是要点说明：并发、压缩、Council、Skill 发现等给出关键字段与默认值，但没有逐句
复现英文版对每个字段的展开论述。需要某个字段的完整语义时，查阅英文版
[Configuration reference](https://github.com/sunerpy/zuno/blob/main/docs/reference/configuration.md)
与 [`schemas/zuno.json`](https://github.com/sunerpy/zuno/blob/main/schemas/zuno.json)；
两者出现分歧时以英文版和 schema 为准。
:::

## JSON Schema

仓库自带一份 JSON Schema，可用于编辑器补全与校验：

```json
{
  "$schema": "https://raw.githubusercontent.com/sunerpy/zuno/main/schemas/zuno.json"
}
```

Schema 是从 Rust 类型生成的，因此它与运行时实际接受的内容保持一致。顶层拒绝未知键，这意味着拼错的键会在加载时报错，而不是被静默忽略。

## 主配置与 TUI 配置

`zuno.json` 配置 Agent 运行时。终端应用另有自己的 `tui.json`，两者刻意分开：主题与快捷键只影响一个客户端，不应改变无界面运行、ACP 服务端或 HTTP 服务端的行为。

把 `theme` 写进 `zuno.json` 不会报错，也不会生效。详见 [主题与快捷键](/zh/config/theming)。

## 产品子 Agent

`productAgent` 配置以原生协议接入的产品 Agent。它们拥有自己的凭据与权限边界，默认关闭。

## 图像附件接纳

`attachment.image` 是 TUI、无界面、ACP 与 Server 共用的图像接纳策略：

```json
{
  "attachment": {
    "image": {
      "auto_resize": true,
      "max_source_bytes": 20971520,
      "max_width": 2000,
      "max_height": 2000,
      "max_pixels": 4000000,
      "max_encoded_bytes": 5242880
    }
  }
}
```

所有值都是正数硬上限。`auto_resize: false` 会拒绝必须缩放的源，不会放宽字节、尺寸或像素检查。`max_base64_bytes` 不被接受。规范化、持久对象、旧记录重放、export 与 GC 语义见[图像与文件引用](/zh/guide/attachments)。

## 子模型选择策略

`subagent_model_selection` 由宿主全局持有，但会冻结到每个持久 session：

```json
{
  "subagent_model_selection": {
    "enabled": false,
    "allowed_models": ["provider/model"]
  }
}
```

默认值让 `task` schema 不包含 model/effort 字段。启用后要求 allowlist 非空、无重复，并且每个确切条目都能在当前模型目录解析。Zuno 持久化规范化排序后的策略及 digest；之后修改配置不会改变已有 session 或其子级。详见[模型路由](/zh/config/models#可选的子模型与-effort-allowlist)。

## 并发

`concurrency` 控制同时运行的工作量上限。它约束的是编排层的并行度，而不是单次工具调用内部的并发。

## 可选的 Agent 步数护栏

`steps` 为一个 Agent 设置在最终一次纯文本回复之前，允许的最大工具可用迭代次数。省略即不设固定步数上限。

这是护栏而非目标：它的作用是让失控的循环停下来，而不是鼓励用满步数。

## Agent 能力上限与必需 Skill

`tools` 与 `permission` 在 Agent 层面定义能力上限。Agent 契约只能收窄权限，永远不能放宽 —— 一个只读 Agent 无论配置怎么写都会被钉在 `read-only`。这个方向是单向的，因此选择只读 Agent 是一项保证，而不是一个可以被配置悄悄反转的默认值。

`requiredSkills` 列出每个回合开始时就为该 Agent 加载的 Skill。

安全边界的完整说明见 [权限与沙箱](/zh/guide/permissions)。

## Agent 模型 preset

`presets` 定义命名的团队模型路由，`preset` 选择当前生效的那一个。每个 preset 有两个字段：

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `agents` | object | 按 Agent 指定的模型选择 |
| `categories` | object | 按语义类别指定的模型选择 |

模型路由的详细说明见 [模型路由](/zh/config/models)。

## 原生 Council 启动器

Council 让多个隔离的席位各自独立评估同一个问题，然后综合结论。席位、Agent、模型路由、法定人数、并发、重试策略、端到端超时、预留的综合时间以及输出上限都由配置拥有，模型不能在调用时改写它们。

## 上下文压缩

`compaction` 控制上下文接近上限时的自动压缩：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `auto` | boolean | `true` | 接近上限时自动压缩 |
| `prune` | boolean | `true` | 压缩时裁剪历史 |

## 插件包

`plugin` 相关配置声明扩展包。扩展要么是显式 WASI 授权下的 WebAssembly 组件，要么是使用行分隔 JSON-RPC 的受限子进程。能力经过声明与校验，不会被意外继承。

详见 [插件与扩展](/zh/guide/plugins)。

## 权限模式与规则

`permission.mode` 取三个值：

| 模式 | 行为 |
| --- | --- |
| `standard` | 应用已配置的规则与常规风险门禁。默认值。 |
| `strict` | 每个有副作用的调用都要求一次新的决策。 |
| `allow_all` | 跳过提示，但保留显式拒绝与沙箱校验。 |

`permission.rules` 是有序的，按你书写的顺序求值。规则要么是对整个工具的一个动作，要么是按模式匹配的多个动作。

需要注意 `allow_all` **不会**做什么：它不关闭沙箱，也不覆盖写着 `deny` 的规则。显式拒绝在任何模式下都是终局的，包括这一个。

完整说明与两个门禁如何交互见 [权限与沙箱](/zh/guide/permissions)。

## 沙箱模式与后端不可用策略

顶层 `sandbox` 对象设置模型发起的 Shell 命令所能获得的最大权限：

| 键 | 取值 | 默认值 |
| --- | --- | --- |
| `mode` | `read-only`、`workspace-write`、`danger-full-access` | `workspace-write` |
| `network` | `deny`、`allow` | 受限模式为 `deny`，`danger-full-access` 使用宿主网络 |
| `onUnavailable` | `deny`、`run-unconfined` | `deny` |
| `writableRoots` | 额外的现有可写目录数组 | 空 |
| `protectedPaths` | 重新施加只读保护的路径数组 | 空 |

默认配置明确写出如下：

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "deny"
  }
}
```

`danger-full-access` 始终跳过受限后端发现，以 Zuno 用户的宿主文件系统、进程、凭据和网络
权限原生执行，并把生效权限模式设为 `allow_all`。它不能与 `network: "deny"`、
`writableRoots` 或 `protectedPaths` 组合，因为原生后端无法兑现这些限制。

`run-unconfined` 不是另一个永久无沙箱模式。它让具备写能力的 Agent 所请求的
`workspace-write` 先尝试完整的沙箱发现、能力校验与部署验证，只在符合条件的类型化不可用
错误下改用原生后端：

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "run-unconfined"
  }
}
```

降级时仍保留 `standard`、`strict` 或 `allow_all` 权限模式、显式拒绝、灾难性命令硬拒绝、
后台执行、超时与取消链路；但请求的网络拒绝、可写根目录和受保护路径不会由 OS 强制执行。
只读 Agent 永远不会无沙箱降级。

项目 `zuno.json[c]` 与 `.zuno` 配置只能把 `onUnavailable` 设为 `deny`。只有受信的全局、
显式配置、环境、CLI 或受管层可以启用 `run-unconfined`，受管策略仍拥有最终否决权：

```sh
zuno --sandbox danger-full-access
zuno --sandbox workspace-write --sandbox-on-unavailable run-unconfined
ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined zuno
```

用 `zuno debug sandbox` 查看请求/实际权限、降级资格、`resolutionKind` 和
`fallbackReason`。`--check` 仍严格检查请求的约束是否可部署，不会因为允许降级而成功。

## 严格 HITL 授权

`strict` 模式要求每个有副作用的调用都获得一次新的人工决策。这适用于不希望任何写操作在无人确认下发生的场景。

## Skill 发现

`skills` 配置 Skill 的发现与预算：

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `paths` | array | 额外的 Skill 搜索路径 |
| `urls` | array | 远程 Skill 来源 |
| `includeInstructions` | boolean | 是否把 Skill 指令纳入提示词 |
| `maxContextTokens` | number | Skill 目录的 token 预算 |
| `maxSelectedContextTokens` | number | 已选中 Skill 正文的 token 预算 |

编写 Skill 的目录结构与 frontmatter 见 [编写 Skill](/zh/config/authoring-skills)。

## 可复用工作流：Skill 与 Markdown 命令

`command` 定义自定义命令，`workflows` 定义命名的不可变多 Agent 工作流模板。

`CommandConfig` 中 `template` 是整个 schema 的叶子字段里唯一必填的。

`AgentWorkflowConfig` 的 `maxAgents` 默认为 12，`maxParallel` 默认为 4。

详见 [工作流与命令](/zh/config/workflows)。

## 记忆学习

`memory` 配置持久候选、反思、评审、提升与撤销。记忆写入是提议而非直接生效：候选进入待评审状态，由人决定是否提升为常驻记忆。

## 查看解析结果

配置的合并结果可以直接打印，而不必靠推断：

```sh
zuno debug config
```

这会输出全部层合并后的最终配置，因此某一层没有按预期生效时是可见的。

```sh
zuno debug paths
```

这会输出 Zuno 实际使用的每个目录，是确认配置文件位置的最快方式。

## See also

- [配置文件与优先级](/zh/config/files) —— 各层的发现顺序与合并规则
- [权限与沙箱](/zh/guide/permissions) —— 两个门禁的完整说明
- [Provider 与凭据](/zh/config/providers) —— provider 配置与凭据存储
- [zuno debug](/zh/cli/debug) —— 全部内省子命令
