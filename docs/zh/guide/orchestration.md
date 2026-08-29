# Agent 编排与模型路由

本指南解释 Zuno 默认的 `orchestrator` 如何委派工作、如何选择子 Agent 的模型与推理级别，以及何时使用直接 task、配置化 workflow 或 Council。

简短的答案是：

- 在 `agents` 下配置 Agent 身份、工具、权限和委派边界；
- 在 `presets` 下配置可切换的团队级模型路由；
- 用一个具名的 `agent` 和一份完整的类型化工作契约调用 `task`；
- 当图结构与依赖顺序必须由配置拥有、而不是由模型临场发明时，使用 `workflows`。

## Orchestrator 负责什么

Agent 选择按此顺序解析：

1. 客户端显式选择的 Agent；
2. 顶层 `default_agent`；
3. 内置 `orchestrator`。

`orchestrator` 是默认的多 Agent 交付负责人，也是唯一暴露 `task` 委派工具的原生主 Agent。它内置的直接委派目标是：

- `deep`；
- `fixer`；
- `general`；
- `explorer`；
- `librarian`；
- `oracle`；
- 当存在具备视觉能力的路由时，还有 `looker`。

mode 为 `subagent` 或 `all` 的配置或扩展 Agent 可以加入该目标阵容。仅为 `primary` 的 Agent 不能作为委派目标。

内置的工作流是：

```text
durable user input
  -> selected primary Agent and frozen capabilities
  -> primary model request
  -> task, workflow, or council tool call
  -> permission, target, depth, and model-route validation
  -> durable child session or background job
  -> child model/tool loop
  -> durable result/report admitted back to the parent
```

委派不是一个仅靠提示词维持的约定。目标阵容、模型路由、权限、递归上限、取消、job 状态和报告投递都是类型化的运行时决策。

## 内置 Agent 的职责

| agent | 职责 | 委派 |
| --- | --- | --- |
| `orchestrator` | 承担结果、切分工作、整合产出、验证完成 | 可以委派 |
| `build` | 在单一通道内直接完成端到端实现 | 无子级工具 |
| `plan` | 只读调研与可直接实施的规划 | 无子级工具 |
| `deep` | 直接的深度工作模式，或受委派的根因分析与横切实现 | 不递归委派 |
| `fixer` | 聚焦的局部改动及其回归范围 | 不递归委派 |
| `general` | 没有更窄专职 Agent 的有界工作 | 不递归委派 |
| `explorer` | 只读的仓库与调用链调研 | 不递归委派 |
| `librarian` | 当前的外部文档与上游调研 | 不递归委派 |
| `oracle` | 只读的架构与根因评审 | 不递归委派 |
| `looker` | 视觉产物检查 | 不递归委派 |

`deep` 的 mode 是 `all`：`/agent`、TUI 的循环切换键、ACP 配置和无界面的 Agent 选择都可以把它选为会话 Agent，而 `orchestrator` 仍可通过 `task` 以它为目标。直接选择不会赋予递归委派能力；同一份冻结的 profile 会保留不给 `task`，同时暴露具备工作能力的检查、编辑、Shell、Skill/MCP、外部调研、提问和进度类工具。

`explorer` 刻意是原生只读，而不是 shell 只读。它的默认工具面包含 `read`、`glob`、`grep` 和只读的 `lsp`，并拒绝 `shell`、编辑、委派和网络调研。因此 `du`、`stat`、`file` 这类命令不属于 `explorer`：它们是通过 `shell` 触达的可执行文件，不是原生读取操作。请把基于命令的检查委派给 `deep`、`general` 或另一个具备 shell 能力的 Agent，或者在父会话中执行那条有界命令。全局 `permission.mode: "allow_all"` 会跳过常规确认，但不会抹掉这条显式的 Agent 拒绝。用户可以显式替换 Agent 权限覆盖层，但那样做会有意改变 `explorer` 的只读契约，通常不如选择正确的 Agent 来得清晰。

即使在 [Shell 沙箱路线图](https://github.com/sunerpy/zuno/blob/main/docs/design/shell-sandbox-roadmap.md)中的 Linux 沙箱门禁已经可用之后，这条拒绝仍然是一条内置的角色边界。一个自定义的只读 Agent 可以暴露 `shell`；运行时组装随后会编译 `SandboxMode::ReadOnly`，并在当前平台无法证明它时拒绝注册。Zuno 不会仅仅因为存在某个后端就自动放宽专职 Agent，也不会把命令解析、进程组，或一段写着「只读」的提示词当作约束。

## 子级能力上限与必需 Skill

委派保留引擎行为，同时不创造第二个权限来源。父级 Attempt 记录了对父级 provider 请求可见的确切工具 schema。子级只能使用以下几项的交集：

1. 那份被冻结的父级 Attempt schema 集合；
2. 目标 Agent 角色及其扩展工具继承策略；
3. 该 Agent 配置的确切 `tools` 允许列表；
4. 生效的用户与 Agent 权限规则。

这些都是收窄层。角色、允许列表或权限规则可以移除某项能力，但一条 `allow` 无法添加一个在父级 Attempt 中并不存在的工具。schema 身份很重要：把一个工具替换成另一个同名 schema，并不满足所继承的权限。`permission.mode: "allow_all"` 只压制常规的 HITL 询问，不会扩大这个交集。

具备工作能力的角色可以选择自动继承 MCP 与扩展工具。Zuno 不会把父级的每个 MCP 工具都暴露给每个只读专职 Agent：未知的 MCP 操作默认仍然有副作用，而显式拒绝始终优先。运维方可以在某个只读 Agent 自己的 `permission.rules` 中显式允许一个经过审计的动态工具 id；父级 schema 上限与确切 schema 校验仍然生效。这让具备工作能力的 Agent 能继承一个有界的动态工具面，也让某个仓库专职 Agent 能使用一次经过审计的只读查询，同时不削弱面向所有扩展的默认契约。

Skill 走的是另一条路径。每个初始或恢复的子级宿主都会针对自己的工作目录和当前 profile 运行常规的 Skill 发现；父级不会把已加载的 Skill 正文复制进子级提示词。用 `agents.<name>.requiredSkills` 添加稳定的角色指导：

```json
{
  "agents": {
    "explorer": {
      "requiredSkills": ["codegraph"]
    }
  }
}
```

在每一次面向 provider 的输入之前，每个必需名称都必须解析到一个可见来源；Zuno 会确保该来源被加载，并按来源路径去重。Skill 缺失，或者存在两个同名的可见来源，都会让该子级启动失败，而不是挑一个隐藏的优先级胜出者。必需 Skill 仍然只是指令：上面的例子保证这个 Agent 收到 CodeGraph 的指导，而不保证它收到 CodeGraph 的 MCP 工具。那些工具仍然需要一份确切的父级 schema、自动的角色继承或一次针对该 Agent 的确切授予、在 Agent 的 `tools` 允许列表中存活，并且没有显式的权限拒绝。

这个模型借用了 Codex 中有用的一种模式：从父级的生效能力派生子级，同时允许角色覆盖层削减它。这是 Zuno 的运行时设计决策，不是对 Codex 的兼容承诺。

### 可选的配置化设计者

UI 方法由第一方的 `ui-design` Skill 提供，而不是一个新的原生 Agent。当某个项目确实需要独立的模型/上下文通道时，添加 `.zuno/agent/designer.md`：

```markdown
---
description: Review and implement bounded UI and interaction work
mode: subagent
permission:
  mode: standard
  rules:
    "*": deny
    read: allow
    glob: allow
    grep: allow
    lsp: allow
    edit: allow
    shell: ask
    skill: allow
    plan_get: allow
    todo_get: allow
---

Own only the delegated UI/UX implementation scope. Load the `ui-design` Skill before
acting. Respect the existing design system, do not perform broad external research,
do not make product or backend architecture decisions, and do not delegate children.
Return changed files, interaction/accessibility checks, visual evidence, and risks.
```

把这个配置化 Agent 加入 orchestrator 的确切委派允许列表。该列表是替换而不是扩展原生值，所以要显式保留每个默认的专职 Agent：

```json
{
  "agents": {
    "orchestrator": {
      "delegates": [
        "deep",
        "fixer",
        "general",
        "explorer",
        "librarian",
        "oracle",
        "looker",
        "designer"
      ]
    }
  }
}
```

除非项目需要一条专用路由，否则不要硬编码模型。截图、图像、PDF 或视频观察仍归 `looker`；父级把结构化的观察结果传给 `designer`。只有当反复出现的证据表明存在路由错误、上下文污染，或者一条配置无法表达的必要能力边界时，才把 `designer` 提升进原生阵容。

Orchestrator 的提示词要求有界的子级目标、明确的交付物、互不重叠的写入者、感知依赖的调度，以及父级验证。子级的输出是给父级的证据；它不会自动成为最终答案。

## 三个配置层

### Agent 定义

`agents.<name>` 控制一个 Agent 的稳定行为：

- `description` 与 `prompt`；
- `mode`：`primary`、`subagent` 或 `all`；
- `model`，加上 `reasoning` 或 provider 特有的 `variant` 之一；
- `temperature`、`top_p`、provider 选项和 `steps`；
- 确切的、对模型可见的 `tools`；
- `requiredSkills`，其唯一解析出的正文会在每个子级回合预加载；
- 确切的直接子级 `delegates`；
- 逐工具的 `permission`。

`reasoning` 与 `variant` 都要求显式的 `model`，并且互斥。规范的 reasoning 取值是：

```text
off, low, medium, high, xhigh, max
```

当某个 Agent 必须始终使用同一条路由、与当前团队 preset 无关时，请使用 Agent 级别的模型。

### Preset

`presets` 是可切换的团队级路由。它们把已有的 Agent 和语义类别映射到一个带限定的 `provider/model`，以及可选的 provider 中性推理级别。Preset 不创建 Agent、不授予工具、不改变权限，也不授权委派。

当同一套 Agent 阵容需要在 `balanced`、`fast` 和 `thorough` 之类的团队之间切换时，使用 preset。

一个 preset 可以把不同 Agent 路由到不同 provider。这是组合以下搭配的推荐方式：例如用一个长上下文的 Claude 模型做编排与规划，用一个 GPT 模型做实现与架构评审，再用一个更小的多模态模型做探索或视觉检查。这些 provider 仍然在共享目录中各自独立配置；preset 只包含带限定的模型路由和规范的推理级别。仓库中已检入的 [`zuno-multi-provider.json`](https://github.com/sunerpy/zuno/blob/main/examples/config/zuno-multi-provider.json) 给出了 `myopenai`、`kiro-local` 以及覆盖完整原生用户 Agent 阵容的混合 `hybrid` 团队。

### 直接委派的路由

面向模型的 `task` 工具不接受 `model`、`effort` 或 `category`。生效的模型与推理级别由所选 Agent、当前 preset 和父会话决定。这让路由与权限留在经过校验的宿主配置里，而不是允许一段提示词选择任意 provider 或推理策略。

## 推荐的可切换配置

这个例子定义了一个自定义的发布评审者，并让整个团队走一个当前激活的 preset：

```json
{
  "default_agent": "orchestrator",
  "subagent_depth": 1,
  "agents": {
    "orchestrator": {
      "delegates": [
        "deep",
        "fixer",
        "general",
        "explorer",
        "librarian",
        "oracle",
        "release-reviewer"
      ]
    },
    "release-reviewer": {
      "description": "Reviews release safety, evidence, and rollback readiness.",
      "mode": "subagent",
      "prompt": "Review only the supplied release scope. Do not delegate. Return findings, evidence, and residual risk.",
      "tools": [
        "read",
        "glob",
        "grep",
        "lsp",
        "webfetch",
        "web_search"
      ],
      "permission": {
        "mode": "standard",
        "rules": {
          "*": "deny",
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "lsp": "allow",
          "webfetch": "allow",
          "web_search": "allow"
        }
      }
    }
  },
  "preset": "balanced",
  "presets": {
    "balanced": {
      "agents": {
        "orchestrator": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        },
        "deep": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        },
        "fixer": {
          "model": "myopenai/primary-model",
          "reasoning": "medium"
        },
        "general": {
          "model": "myopenai/primary-model",
          "reasoning": "medium"
        },
        "explorer": "myopenai/fast-model",
        "librarian": "myopenai/fast-model",
        "oracle": {
          "model": "myopenai/primary-model",
          "reasoning": "max"
        },
        "release-reviewer": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        }
      },
      "categories": {
        "cheap": "myopenai/fast-model",
        "deliberate": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        }
      }
    }
  }
}
```

这个例子刻意没有在 Agent 定义中写 `model` 与 `reasoning`。配置的 `agents.<name>.model` 比 preset 路由更具体，因此会胜出。如果希望切换 preset 能影响某个 Agent，就把它的模型选择放在 preset 里。

`tools` 数组是一份确切的最终允许列表，不是追加列表。除非你确切知道想要的完整工具面，否则不要在 `orchestrator` 上设置它。如果它隐藏了 `task`，即使存在 `delegates`，该 Agent 也无法委派。同理，如果期望 orchestrator 调用配置化 workflow 与 Council 工具，这些工具也必须保持可见。

`delegates` 数组同样是确切的。它会收窄内置目标集合，并可以加入有效的配置化子 Agent。Zuno 会拒绝不可用的名称，而不是静默忽略它们。

## 直接子级的模型优先级

对带 `agent` 的 `task`，Zuno 按此顺序选择子级模型：

1. `agents.<target>.model`；
2. `presets.<active>.agents.<target>`；
3. 父会话模型。

不可用或未限定的模型会产生一条可见的路由诊断，并落到下一个已配置的候选项。模型 id 必须使用 `provider/model` 形式，并且存在于解析出的目录中。

推理或 variant 的解析使用胜出的那条 Agent 路由上附带的 reasoning 或 variant，然后是所选模型/provider 的默认值。

直接委派示例：

```json
{
  "agent": "explorer",
  "objective": "Trace the authentication call chain",
  "deliverable": "A source-backed call path and affected-file list",
  "instructions": "Trace login, refresh, logout, credential storage, and every caller.",
  "success_evidence": "Cite the owning symbols and distinguish observed behavior from inference.",
  "scope": {
    "include": ["crates/**"],
    "exclude": ["target/**"]
  },
  "constraints": {
    "must": ["Remain read-only"],
    "must_not": ["Edit files"]
  },
  "background": true,
  "reportDelivery": "nextStep"
}
```

未知字段会导致 schema 校验失败，包括已被移除的 `description`、`prompt`、`subagent_type`、`category`、`model`、`effort` 和 `load_skills`。Zuno 不会转换旧形态。

## 宿主拥有的类别路由

Preset 的 `categories` 仍可用于配置化 workflow 与 Council 的路由。它们是语义化的模型层级，不是 Agent，并且不能被面向模型的 `task` 调用选中。一条宿主拥有的类别路由会运行有界的 `general` Agent，并先通过 `presets.<active>.categories.<category>` 解析，然后才回落到父会话模型。凡是专职提示词、工具、权限、必需 Skill 或输出归属很重要的场合，都请使用具名的 `agent`。

## 顶层 Agent 的模型路由

被选中的主 Agent 遵循一套相关但独立的优先级：

1. 客户端/运行时的显式模型选择；
2. `agents.<selected>.model`；
3. `presets.<active>.agents.<selected>`；
4. 顶层 `model`；
5. 常规的目录选择。

TUI 中手动选择的模型或推理级别作用于顶层 Agent。它不会抹掉后续子 Agent 的 preset 路由。

## 后台 job 与报告投递

`background: false` 让子级在前台运行。父级的中断会传到子级，而这次 task 调用只有在子级关闭流程排空之后才结算。

`background: true` 会在容量准入之前提交一个持久的 queued job，并返回一个 job id。随后由 `reportDelivery` 控制终态结果：

- `nextStep` 结算子级，把它的报告准入父级的持久 inbox，并唤醒父级再跑一个回合；
- `quiet` 只把结果落盘，不注入父级输入。

`reportDelivery` 只对后台工作有效。进程重启时，从未开始的 job 变为 cancelled；已在运行的 job 变为 uncertain，并且绝不会被重放。

对独立调研或长时间运行的工作使用后台委派。除非父级还有其他有用的工作、并且能可靠地消费一份稍后到来的报告，否则请把关键路径上的依赖保留在前台。

## 与已挂载的子级交互

TUI 把每个观察到的原生子级当作一个完整的会话界面，而不是一个详情弹窗。用 `Ctrl+X Down` 进入第一个直接子级，用 `Ctrl+X Left` 或 `Ctrl+X Right` 在同级之间循环，用 `Ctrl+X Up` 返回父级。导航切换可见会话时，每个子级都保有自己的编辑区草稿。

在一个正在运行的子级中按 Enter，会把文本准入该子级的持久 inbox 并引导它的活跃回合。在子级结算之后按 Enter，会准入一次续跑，并以其解析出的 Agent、模型、强度、权限和编排血缘唤醒同一个子级身份。解析出的续跑身份会落盘在子会话元数据中，当父级在稍后的某个进程里被恢复时，TUI 会重建持久的子级树和保留的对话记录。因此被恢复的子级支持续跑，而不是变成一个只读的历史面板。子级中的文本是字面文本：像 `/help` 这样看起来是斜杠命令的输入会被发送给子级，不会作为根 TUI 命令执行。产品 Agent 调用和 workflow 投影不会被呈现为可续跑的子对话。

直接的子级输入打开的是与根回合完全相同的 `TurnHost` 路径。它得到组装好的工具调度器、常规的权限与提问桥接、取消与生命周期事件、持久用量、重试行为，以及主动或恢复式压缩。这是引擎能力对等，不是权限扩张：递归的 `task` 注册仍然需要该子级 Agent 显式的委派集合和下文所述的剩余深度。

## 深度、权限与容量

顶层 `subagent_depth` 默认为 `1`。它是跳数上限，不是授予。提高它不会自动把 `task` 工具给某个子级、不会添加委派目标，也不会放松权限。内置的专职 Agent 有意不做递归委派。

委派使用 `task` 权限域。一个 Agent 需要同时具备：

1. 对模型可见的 `task` 工具；
2. 一个位于它确切 `delegates` 集合中的目标；
3. 针对该目标调用 `task` 的权限；
4. 剩余的 `subagent_depth`；
5. 可用的进程本地委派容量。

在一个工作区进程内，原生 task、workflow 节点、Council 席位和产品 Agent 共用 `concurrency.delegations`。后台 job 在一个公平的 FIFO 队列中等待。改变上限不会取消正在进行的工作，而不同的 Zuno 进程目前还不共享一份持久的配额 lease。

## 由配置拥有的 workflow DAG

当允许的 Agent、依赖关系和最大并行度必须是不可变配置、而不是由 orchestrator 临场发明的图时，使用 `workflows`：

```json
{
  "workflows": {
    "release-check": {
      "maxParallel": 2,
      "maxAgents": 4,
      "nodes": [
        {
          "id": "code",
          "agent": "deep",
          "description": "Review implementation and tests",
          "prompt": "Inspect implementation correctness and regression coverage.",
          "dependsOn": []
        },
        {
          "id": "upstream",
          "agent": "librarian",
          "description": "Verify upstream constraints",
          "prompt": "Check current upstream documentation, releases, and authorization constraints.",
          "dependsOn": []
        },
        {
          "id": "risk",
          "agent": "oracle",
          "description": "Integrate residual risk",
          "prompt": "Use the completed evidence to identify release blockers and rollback requirements.",
          "dependsOn": ["code", "upstream"]
        }
      ]
    }
  }
}
```

模型通过这样的调用来触发这份不可变模板：

```json
{
  "workflow": "release-check",
  "prompt": "Review release candidate v0.4.0.",
  "description": "Release readiness",
  "background": true,
  "reportDelivery": "nextStep"
}
```

模型可以选择一份已配置的模板并提供根提示词。它不能改变图结构、Agent、依赖关系或并发上限。Workflow 节点复用直接 Agent 的模型路由：

1. `agents.<node-agent>.model`；
2. 当前 preset 的 Agent 路由；
3. 父会话模型。

`maxParallel` 默认为 4，`maxAgents` 默认为 12，两者都在 `1..=64` 内校验。Zuno 会拒绝重复的 id、缺失的依赖、环、未知或已禁用的 Agent，以及大于 Agent 上限的并行上限。

调度器是工作守恒的，但按模板顺序发布持久结果。只有在所有声明的依赖都成功完成之后，一个节点才会开始。

## Council

Council 是同一条原生子级回合路径的另一个消费者。它没有独立的模型路由器、权限旁路或调度器。每个席位指定一个 Agent，而该 Agent 通过同样的已配置 Agent 与 preset 路由解析。

TUI 的 `/council` 启动器会附加一条单回合的路由指令，请求当前 Agent 在后台以 `nextStep` 投递方式调用一次 `council_run`。原始的用户消息与产生的 job 都保持持久。

当多个独立视角应当评估同一个问题时，使用 Council。当各个席位有不同的提示词或依赖关系时，使用 workflow DAG。当应由 orchestrator 自适应地判断委派是否有用时，使用直接的 `task` 调用。

## 第一方 Skill 与斜杠入口

`crates/zuno-orchestration/src/skills` 中的资源是嵌入的 Skill 描述符与 Markdown 正文。它们不是 CLI 子命令，安装时也不会被复制到用户的配置目录。挂载第一方 profile 会把它们发布进与用户 Skill、扩展 Skill 相同的那份不可变目录，并带上来源身份、摘要、profile 可见性和所需工具元数据。

当某个公布出来的 Skill 名称没有歧义、并且不与真实命令冲突时，TUI 会把它直接暴露为 `/<skill-name>`。这个斜杠入口会在下一次模型请求之前加载所选 Skill；它不会创建第二个宿主侧命令处理器。存在歧义的名称仍可通过 Skill 选择器或类型化的 `skill` 工具选择。

`/develop-zuno` 是第一方的编写指南，用于判断一处改动该落在配置、Agent Markdown、用户拥有的 Skill 或命令、一个 `extension.json` 包，还是原生 Rust。它链接到仓库当前的配置、插件、进程插件、编排和运行时契约。像 `dual-review` 和 `auto-release` 这类用户特定策略仍然是外部 Skill 或命令；Zuno 不把那些工作流编译进产品。

## TUI 与 CLI 控制

在 TUI 中：

- `/agent` 打开 Agent 选择器；
- `/model` 打开模型选择器；
- 推理控制只在所选模型声明的级别之间循环；
- `/preset` 打开 preset 选择器；
- `/preset <name>` 把一个已配置的 preset 应用到当前会话。

切换 preset 会重新挂载已准备好的运行时组合，而不中断正在进行的回合。它会清除先前手动设置的顶层模型与推理覆盖，使所选团队的路由生效。设置顶层 `preset` 可指定启动默认值。

无界面的 `run` 命令目前使用已配置的顶层 `preset`；没有独立的 `--preset` 标志。对可重复的自动化，优先使用项目配置、一个显式的配置层，或一份专门针对某个 preset 的启动配置。

有用的检查命令有：

```sh
# Resolved Agent catalog and effective capability rules.
zuno agent list

# One Agent's resolved model, permissions, live MCP tools, Skill budgets, and sandbox.
zuno debug agent orchestrator

# Verify the restricted Linux sandbox deployment.
zuno debug sandbox --mode workspace-write --network deny --check

# Merged, validated configuration and source layers.
zuno debug config

# Available models and declared model capabilities.
zuno models myopenai --verbose

# Active permission policy.
zuno debug permissions
```

在一个真实回合之后，`zuno debug prompt` 可以检查提示词溯源。只有在可以安全打印完整的、对模型可见的指令、AGENTS、skill 和记忆内容时，才使用 `--show-sensitive`。

## 常见配置错误

### 「preset 没有改变这个 Agent」

`agents.<name>.model` 更具体，会胜过 preset。如果希望由 preset 来决定，请移除 Agent 上固定的模型。

### 「orchestrator 不再委派了」

某个确切的 `tools` 允许列表隐藏了 `task`，某个确切的 `delegates` 列表移除了该目标，权限策略拒绝了该目标，或者 `subagent_depth` 已耗尽。检查 `zuno agent list` 和 `zuno debug permissions`。

### 「某个 workflow 类别用错了模型」

配置 `presets.<active>.categories.<category>`。宿主拥有的类别路由不会使用 `general` Agent 已配置的或 preset 的 Agent 路由。

### 「子级忽略了 task 中的 model 或 effort 字段」

那些字段不属于 `task` 的 schema。请改为配置 `agents.<name>.model`、`agents.<name>.reasoning`，或当前的 preset 路由。

### 「自定义的评审者无法作为目标」

把 `mode` 设为 `subagent` 或 `all`，保持它启用，并且在配置了委派方 Agent 的 `delegates` 列表时，把它的确切名称加进去。

### 「某个已配置的 workflow 没有出现」

被选中的 Agent 必须暴露原生的 `workflow` 工具，并且对每个节点 Agent 都拥有 `task` 权限。一份包含未知 Agent、环或无效上限的已发布配置会在组装期间被拒绝，而不是作为一个占位命令注册。

## 相关文档

- [配置项参考](/zh/config/reference)
- [Harness 运行时](/zh/operate/harness-runtime)
- [插件、自定义 Agent 与 workflow](/zh/guide/plugins)
- [Agent 编排执行路线图](https://github.com/sunerpy/zuno/blob/main/docs/design/agent-orchestration-execution-roadmap.md)
