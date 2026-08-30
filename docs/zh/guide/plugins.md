# 插件、自定义 Agent 与 workflow

Zuno 只有一种扩展包格式，以及三个执行层级。包清单始终是 `extension.json`，带 `apiVersion: "zuno.extension/v1"`；变化的只是可执行行为在哪里运行。

| 层级 | 用于 | 权限与生命周期 |
| --- | --- | --- |
| 声明式包 | 自定义 Agent、斜杠命令 workflow 和 Skill | 没有客户代码。贡献进入 Zuno 的原生目录，并随拥有它的组合一起被移除。 |
| WASI 组件 | 可在有界权限下工作的运行时可加载工具 | 通过 Wasmtime 的 Component Model 在 Zuno 进程内运行。文件系统、网络和环境都是选择性开启的。Fuel、内存、墙钟时间、取消和逆序关停由宿主强制执行。 |
| 受信进程宿主 | 需要一个普通宿主可执行文件、或 WASI 无法暴露的 API 的插件 | 必须声明 `host.full`。它继承 Zuno 进程环境和 OS 权限，通过 stdio 说行分隔 JSON-RPC，并随 profile 一起被停止与回收。 |

已编译的第一方或部署方拥有的 Rust 行为，仍然是 `ProfileBundle` 中的原生 `Component`、类型化服务或 `AgentDriver`。Zuno 不加载 Rust 动态库：Rust 没有稳定的插件 ABI，而卸载一个库无法证明它的线程、回调或借用值都已消失。

内置的 `/develop-zuno` Skill 帮助选择这个扩展层级，并链接到当前的编写参考。它只是指导：加载它不会授予插件能力、工具、文件系统访问或权限旁路。

## 安装与检查包

项目包位于 `.zuno/extensions` 之下；全局包位于 Zuno 配置根之下，通常是 `~/.config/zuno/extensions`。

```sh
# Install into the current project.
zuno plugin add examples/plugins/review-kit --project

# Inspect the packages active for this directory.
zuno plugin list

# Replace the installed directory transactionally.
zuno plugin update examples/plugins/review-kit --project

# Remove it. Running hosts keep their already-mounted composition until stopped.
zuno plugin remove review-kit --project
```

`add` 会拒绝一个已存在的包。`update` 在已安装目录旁边暂存一份完整副本，原子地交换它，并在交换失败时恢复旧目录。包含符号链接或特殊文件系统条目的来源会被拒绝。包目录名必须等于其清单中的 `id`。

模型也可以使用 `extension_define`、`extension_run`、`extension_stop`、`extension_undefine` 和 `extension_inspect` 来处理进程本地的声明式包。这条路径绝不写磁盘，并且刻意拒绝可执行运行时声明。

## 自定义 Agent 与 workflow

扩展提供的 Agent 就是一个普通的 Zuno Agent。决定它能使用哪些对模型可见的工具的，是它的 `permission` 对象，而不是插件的运行时能力列表：

- `read`、`glob`、`grep` 和只读的 `lsp` 提供文件检查能力；
- `edit` 提供原生的文件修改能力；
- `webfetch` 与 `web_search` 提供网络调研能力；
- `shell` 在工作区中运行一个宿主进程，并继承 Zuno 的进程环境、文件系统可见性、网络、代理变量和凭据；
- `skill` 加载可复用指令；
- `task` 再次委派，受配置的深度上限约束。

这复用了与原生 Agent 相同的授权路径。不存在第二套仅供插件使用的权限语言。顶层 `permission.mode: "strict"` 仍然要求每一次有副作用的调用都获得一次新的人类批准，即使某条 Agent 规则写着 `allow`。一条 `ask` 规则在没有在场审批者的无界面场景下无法执行。

随包提供的评审示例显式允许仓库读取与网络调研，并在 shell/环境访问之前询问：

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "review-kit",
  "description": "Adds a network-aware release reviewer and workflow.",
  "agents": {
    "release-reviewer": {
      "description": "Reviews release safety and rollback evidence.",
      "mode": "subagent",
      "prompt": "Use repository, environment, and current external evidence. Do not delegate.",
      "permission": {
        "mode": "standard",
        "rules": {
          "*": "deny",
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "lsp": "allow",
          "webfetch": "allow",
          "web_search": "allow",
          "shell": "ask"
        }
      }
    }
  },
  "workflows": {
    "release-review": {
      "description": "Run the packaged reviewer.",
      "prompt": "Call task once with agent=\"release-reviewer\", objective=\"Review release safety\", deliverable=\"A source-backed release risk report\", instructions=\"Review $ARGUMENTS\", success_evidence=\"Cite every blocking finding\", and background=false."
    }
  }
}
```

当配置或扩展提供的子 Agent 的 mode 是 `subagent` 或 `all` 时，它们会被纳入确切的 `task` 目标阵容。它们配置的模型与 variant 参与常规的子级模型优先级阶梯，并且子级回合会重新解析同一个扩展包、提示词、权限、Skill、工具和工作目录。仅为 `primary` 的 Agent 不是委派目标。

上面的 workflow 提示词应当产生与原生 Agent 相同的类型化 task 契约：

```json
{
  "agent": "release-reviewer",
  "objective": "Review release safety",
  "deliverable": "A source-backed release risk report",
  "instructions": "Review the requested release scope.",
  "success_evidence": "Cite every blocking finding.",
  "background": false
}
```

确切的直接 Agent 与宿主拥有的类别优先级阶梯、推理策略、后台报告投递、配置化 workflow DAG 与 Council，见 [Agent 编排与模型路由](/zh/guide/orchestration)。

Workflow 是一个斜杠命令提示词模板。`$ARGUMENTS` 与位置占位符使用常规的命令展开。当一个 workflow 必须运行某个特定的自定义 Agent 时，它的提示词应当像示例那样，用该 `agent` 和一份完整的类型化委派契约发出 `task`；它不会创建一条隐藏的第二编排路径。

参见 [`examples/plugins/review-kit/extension.json`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/review-kit/extension.json)。

## WASI 组件工具

WASI 运行时声明一个相对于包目录的组件产物：

```json
{
  "runtime": {
    "kind": "wasi",
    "artifact": "plugin.wasm",
    "capabilities": ["workspace.read", "network"],
    "environment": ["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"],
    "fuel": 10000000,
    "memoryMiB": 64,
    "timeoutMs": 30000
  }
}
```

能力是授予，不是描述性标签：

| 声明 | 宿主授予 |
| --- | --- |
| `workspace.read` | 只读的 `/workspace` preopen 与初始工作目录 |
| `workspace.write` | 可读写的 `/workspace` preopen；它包含读权限 |
| `network` | DNS 以及通过 WASI socket 的 TCP/UDP |
| `environment` 中的名称 | 只有被列出的宿主变量会被复制进 guest |

在没有任何授予的情况下，该组件没有工作区 preopen、没有继承的环境、没有 stdio，也没有网络。网络与代理变量是分开的：一个必须使用代理的组件需要 `network` 能力，并且必须显式把相关代理变量加入白名单。密钥只应在 guest 确实需要时传入。

规范接口是 [`wit/zuno-plugin/plugin.wit`](https://github.com/sunerpy/zuno/blob/main/wit/zuno-plugin/plugin.wit)：

```text
initialize: func(package-id: string, workspace: string, capabilities: list<string>) -> result<string, string>;
invoke: func(tool: string, arguments-json: string, session-id: string, message-id: string, call-id: string, agent: string) -> result<tuple<string, string, string>, string>;
shutdown: func() -> result<_, string>;
```

`initialize` 返回确切的协议版本 `zuno.plugin/1`。`invoke` 返回一个标题、对模型可见的文本输出，以及一个 JSON 对象形式的元数据字符串。宿主把对一个组件实例的调用串行化，为每次调用补充 fuel，限定线性内存与实例资源，施加墙钟时间与用户取消，并把陷入 trap 或超时的实例标记为不可用。围绕可能副作用的响应丢失是 `Uncertain`，且绝不会被重放。

完整的 Rust guest 示例位于 [`examples/plugins/wasi-word-count`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/wasi-word-count)。用以下命令构建并演练：

```sh
sh scripts/check-plugin-examples.sh
```

## 受信进程工具

进程插件是通往完整宿主 API 的逃生舱：

```json
{
  "runtime": {
    "kind": "process",
    "command": "python3",
    "args": ["plugin.py"],
    "capabilities": ["host.full"],
    "timeoutMs": 30000
  }
}
```

该声明必须恰好是 `["host.full"]`；一个普通 OS 进程无法真实地强制执行更窄的进程内授予。因此安装本身就是一个信任决定。子进程以该包作为工作目录运行，并继承常规环境变量，包括 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY`。

「托管」描述的是生命周期归属，不是一个安全沙箱。一个恶意或已被攻陷的进程插件可以读取继承来的凭据、访问 Zuno 进程能访问的任何东西、打开网络、让后代进程脱离，或者修改工作区之外的状态。Zuno 会撤回路由，并对它拥有的进程树做有界的尽力清理，但无法撤销外部副作用，也无法证明恶意代码没有逃出那棵进程树。请把不受信的扩展作为 WASI 组件运行，或者把整个 Zuno 进程放进一个 OS/容器沙箱里。

由于 `host.full` 无法强制执行只读边界，每个由进程支撑的工具都必须保持 `sideEffecting` 且 `replay: never`。因此严格授权会在每次进程插件工具调用之前询问。同理，一个带 `network` 或 `workspace.write` 的 WASI 工具不能声称 `readOnly`；只有其授予本身就排除了修改能力的组件，才可以选择 read-only/safe 重放。

协议是 JSON-RPC 2.0，每行一个 JSON 对象：

- `initialize` 收到 `protocolVersion`、`packageId`、`packageRoot`、`workspace` 和已声明的能力，并返回 `{"protocolVersion":"zuno.plugin/1"}`；
- `tools/call` 收到工具名、JSON 参数、session/message/call 坐标和当前 Agent，并返回 `title`、`output` 以及对象形式的 `metadata`；
- `shutdown` 在 Zuno 终止并回收进程树之前请求优雅清理。

协议帧与捕获的 stderr 都是有界的，诊断信息会针对已知的密钥环境值做脱敏，超时与取消会停止整棵进程树，而请求发出之后的协议丢失会被报告为 `Uncertain`。

最小可执行示例见 [`examples/plugins/process-review`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/process-review)。完整实现指南是[受信进程插件开发](https://github.com/sunerpy/zuno/blob/main/docs/process-plugin-development.md)，其中包含协议帧、安全评审、取消与不确定结果、测试，以及运维方本地的 OpenCode Antigravity 搜索桥接。

那个桥接被刻意记录为一个外部工具适配器。它在运维方显式授权之后，复用由已安装的 OpenCode 包拥有并刷新的凭据；它不复制 OAuth 身份，也不完成 Zuno 原生的 Antigravity 登录、凭据或 Integration 生命周期。

## 工具声明与 HITL

每个运行时工具都声明四项互相独立的策略：

```json
{
  "tools": {
    "review_outline": {
      "description": "Create a review outline.",
      "parameters": {
        "type": "object",
        "properties": {
          "subject": { "type": "string" }
        },
        "required": ["subject"],
        "additionalProperties": false
      },
      "effect": "readOnly",
      "replay": "safe",
      "concurrency": "exclusive",
      "uiIntent": "generic"
    }
  }
}
```

默认的 effect 是 `sideEffecting`，replay 是 `never`，concurrency 是 `exclusive`，UI intent 是 `generic`。版本 1 只对那些 WASI 能力信封同样排除了网络与工作区写入的 `readOnly` 工具接受 `safe` 重放，并让运行时插件调用保持独占。进程工具永远是有副作用、不可重放的。运行时插件不能声称 `userMediated` 或 `delegating`，因为那些副作用要求由 Zuno 原生控制交互或子级调用。一个没有任何工具的运行时会被拒绝，因为它没有消费者，而且会仅仅因为启动 Zuno 就执行代码。严格授权在插件被调用之前就消费掉经过校验的 effect。

## 生命周期保证

可执行宿主是延迟的 profile 效果。所有候选包都在它们的路由表发布之前完成初始化。启动失败会按逆序停止已经启动的宿主。卸载先撤回路由，然后按逆序调用 `shutdown` 并等待静默。清理失败、超时或丢失会把该 profile 标记为 `Uncertain`；Zuno 不会报告它已停止，也不会启动一个重叠的替代者。

这项保证覆盖框架拥有的注册、任务、进程树、组件实例和路由。它无法撤销插件已经完成的外部修改。这类修改仍然是一个持久事实，需要一次显式的补偿操作。
