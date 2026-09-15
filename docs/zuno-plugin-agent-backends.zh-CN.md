# 插件声明的 Agent 后端

Zuno 插件可以声明 Agent 后端工厂，但不能借此内置业务工作流或指定模型。
工作流通过带命名空间的 `agentRef` 引用后端；可选的 `executionProfile` 才是
provider、模型、推理等级、service tier、权限与沙箱设置的唯一配置来源。

该边界借鉴 DSH 的组合理念，但不引入其运行时或 ABI。后端包、工作流和执行
Profile 三者相互独立，均可替换。

## 声明资源

兼容 Codex 的旧式插件清单可以引用一个后端声明文件：

```json
{
  "name": "team-agents",
  "agentBackends": "./agent-backends.json",
  "workflows": "./workflows"
}
```

可移植 Agent Plugin 则把同一字段放在 `extensions.com.openai` 对象中。
Zuno 只接受位于已安装插件根目录之下、以 `./` 开头的路径。

被引用的 JSON 使用版本化的 `zuno.agent-backends/v1` 契约：

```json
{
  "apiVersion": "zuno.agent-backends/v1",
  "backends": {
    "native-build": {"kind": "native-codex"},
    "claude-review": {
      "kind": "claude-code",
      "command": "claude",
      "envVars": ["CLAUDE_CONFIG_DIR"]
    },
    "external-review": {
      "kind": "acp",
      "command": "./bin/external-agent-acp",
      "args": ["--stdio"],
      "envVars": ["AWS_PROFILE"]
    }
  }
}
```

运行时 ID 为 `<插件清单名称>/<后端键>`，例如
`team-agents/external-review`。用户工作流只引用该逻辑 ID 与执行 Profile：

```yaml
spec:
  routes:
    review:
      agentRef: team-agents/external-review
      executionProfile: review-agent
```

`$ZUNO_HOME/review-agent.config.toml` 仍是 provider、模型、推理等级、
service tier、审批、权限 Profile 与沙箱的权威来源。每种后端会声明自己能执行的
配置能力，并对不支持的必需能力 fail closed。修改 Profile 无需修改工作流或插件包。

## 当前适配范围

- `native-codex` 在进程内使用完整的 Codex execution profile。
- `acp` 会转发 Profile 的模型、推理等级和已选中的命名权限 Profile ID。旧式
  `sandbox_mode` 会根据有效权限保守映射为 `:read-only`、`:workspace` 或
  `:danger-full-access`，不会转发可能已经过期的展示 sidecar。目前尚未提供独立的
  ACP work-mode Profile 字段，因此 `mode` 保持未设置。
- `claude-code` 会转发模型和推理等级。只读 Profile 映射为 Claude `plan`；显式
  选择的 `:danger-full-access` 映射为 `bypassPermissions`；其他 managed/custom
  Profile 使用保守的非交互 `dontAsk`。Zuno 不会从含糊或仅可写的 Profile 推导
  权限绕过。

对于外部产品，Profile 的 provider ID 会固化在 route binding 中；ACP 还会在 Zuno
metadata 扩展中收到 `modelProvider`。Claude Code 的传输与认证仍由所选 Claude CLI
部署负责（例如其 Kiro 兼容 endpoint 设置或 AWS Bedrock 环境）。Zuno 不会把 OpenAI
provider 定义改写成 Claude 凭据。插件通过命令和显式 `envVars` 选择并绑定该部署；若
要同时使用多套 Claude provider，应声明不同的 namespaced backend。

对两个外部适配器而言，子产品自己的权限选项只是纵深防御，并不是沙箱边界。Zuno
会在 spawn 前把精确可执行文件和固定参数交给 Codex 平台沙箱，并使用 execution
Profile 的有效文件系统与网络策略生成启动命令。受限 managed Profile 所需的 Linux、
macOS 或 Windows 沙箱无法准备时，调用会在外部 Agent 启动前以 access-policy 错误
失败，绝不会静默回退为原生进程。显式禁用沙箱的 `:danger-full-access` Profile
仍表示有意的原生启动；`external-sandbox` Profile 则依赖已经存在的外层沙箱，不再
重复嵌套。因此，外部 Agent 若需要访问模型 Provider，其 Profile 的网络策略必须
明确允许该访问。首个预览版尚未把 session-owned managed network proxy 绑定到外部
backend 进程；一旦配置了该代理，dispatch 会在 spawn 前失败，而不是静默绕过域名
或凭据策略。

不得通过向插件声明或工作流中加入 mode/permission 字段来绕过这些边界。

## 声明字段

- `kind`：`native-codex`、`claude-code` 或 `acp`。
- `command`：`claude-code` 可选，`acp` 必填，`native-codex` 禁止设置。
  Claude 可使用 PATH 中的裸命令名或插件包内 `./` 相对路径；ACP 必须使用不可变的
  包内 `./` 命令。路径使用正斜杠，绝对路径和父目录穿越会被拒绝。当前平台选中的
  包内命令还必须解析为插件根目录内的普通文件，
  不能是符号链接，并且在 Unix 上必须可执行。
- `commandWindows`：可选的 Windows 专用命令，规则与 `command` 相同。
- `args`：ACP 进程的固定参数。Claude Code 的参数由宿主管理，插件不能借此
  绕过有边界的非交互契约。
- `envVars`：允许转发给外部进程的宿主环境变量名称。变量值不会写入清单、
  已加载插件缓存或 workflow ledger。
- `startupTimeoutMs`：ACP 启动期限，默认 `20000`，范围 `1..300000`。
- `runTimeoutMs`：可选外部进程期限，范围 `1..86400000`。
- `disposeGraceMs`：回收宽限，默认 `3000`，范围 `1..60000`。
- `maxMessageBytes`：ACP 消息或 Claude 输出上限，默认 `8388608`，最大
  `67108864`。

`native-codex` 禁止所有进程字段；Claude 禁止 `args` 和 `startupTimeoutMs`，
其固定协议参数与启动过程由宿主管理。

未知字段、不支持的 schema 版本、非法 ID、不安全路径或 kind/config 组合错误
都会使该插件贡献失效。插件加载层不会静默去重相同的完整 ID；后端 registry
会对冲突 fail closed。

在 workflow admission 时，Zuno 会绕过普通插件能力缓存并重新加载后端声明。
backend generation 覆盖插件版本、声明文档精确字节、当前平台包内可执行文件、固定
参数与限制，以及显式转发环境变量值的摘要。完整的非 secret route binding 会在执行
前持久化，并包含所选平台沙箱 helper 与 legacy-Landlock 模式；prepared backend
则在内存中为整个 run 保留同一 generation。启动外部
进程前还会再次校验包内可执行文件的 SHA-256。进程重启后，queued run 只有在重新解析
出的 binding 与已持久化 binding 完全一致时才会恢复。因此后续 Profile、声明、
可执行文件或转发环境发生变化，只影响新 admission，并使发生漂移的跨进程恢复
fail closed；已经运行的 generation 不会被中途替换。环境变量原值始终不会持久化。
Claude 的 PATH 裸命令仍是显式外部部署依赖，但 admission 后不会继续漂移。每个
App Server factory snapshot 都会把它解析为规范化的绝对普通文件，把路径与 SHA-256
写入 factory revision 和持久化 route binding，并在 spawn 前再次校验。命令缺失或
字节变化会让选择或 queued recovery fail closed；升级 Claude Code 后需要创建新的
App Server snapshot（通常是重启）。

## 明确不支持的内容

后端声明不接受提示词、工作流节点、模型 ID、provider、推理等级、service tier、
权限模式、沙箱绕过或凭据。工作流仍只能来自 `$ZUNO_HOME/workflows`、
`<project>/.zuno/workflows` 或插件显式声明的 workflow root。应用中不存在内置
`frontend-consensus` 工作流，也不存在隐式模型路由。

可参考 [`examples/zuno-plugins/agent-backends`](../examples/zuno-plugins/agent-backends/)
中的可选包骨架。仓库示例只用于文档，不会自动安装或被 Zuno 自动发现。
