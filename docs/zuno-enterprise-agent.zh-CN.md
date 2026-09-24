# 基于 Codex harness 的企业级 Agent

本文是在 Zuno 之上构建企业级 Agent 的设计基线。它记录 Codex harness 已经提供什么、
Zuno 今天额外提供什么、哪些企业关注点还没有对应原语，以及 Zuno 打算按什么顺序补齐。
这是一份设计来源，不是功能公告：所有标为*规划中*的能力，在按 `FORK_DELTA.toml` 的
显式契约实现之前都不进入产品。

## 1. 什么是「Codex harness」

OpenAI 把 harness 定义为 "the agent loop and logic that underlies all Codex
experiences"，App Server 则是其上面向客户端的双向 JSON-RPC 边界（《Unlocking the
Codex harness: how we built the App Server》，2026-02-04，
<https://openai.com/index/unlocking-the-codex-harness/>，中文版
<https://openai.com/zh-Hans-CN/index/unlocking-the-codex-harness/>）。harness 就是
`codex-rs/core` 及其组合的 crate；App Server（`codex-rs/app-server`）把 core 的事件流
压成少量稳定通知，并接受针对 thread、turn、item 的请求。

企业级 Agent 需要组合的构件及其在本仓库中的位置：

| 关注点 | 原语 | 位置 |
| --- | --- | --- |
| 会话状态 | Thread、Turn、Item；create/resume/fork/archive；持久化 rollout 历史 | `core/src/thread_manager.rs`、`thread-store`、`protocol` |
| 客户端边界 | App Server JSON-RPC（stdio、Unix socket、WebSocket）；`initialize` 能力协商 | `app-server`、`app-server-protocol`、`app-server-transport` |
| 工具执行 | `ToolRouter`/`ToolRegistry`、沙箱化的 shell 与文件工具、unified exec | `core/src/tools`、`core/src/unified_exec` |
| 隔离 | Seatbelt（macOS）、bubblewrap + seccomp + Landlock（Linux）、原生 Windows 沙箱；`execve` 包装器处理提权 | `sandboxing`、`linux-sandbox`、`windows-sandbox-rs`、`shell-escalation` |
| 策略 | `approval_policy`、permission profile、Starlark `execpolicy` 规则、网络代理 allow/deny | `core/src/config`、`execpolicy`、`network-proxy` |
| 托管配置 | 来自系统、MDM、云端的分层 `requirements.toml`，用户不可覆盖 | `config`、`cloud-config` |
| 生命周期 hook | 12 个 hook 事件（`SessionStart` … `Stop`）、MCP 型 hook、托管 hook 白名单 | `hooks`、`core/src/hook_runtime.rs` |
| 扩展 | 插件（skills、MCP server、hooks、apps）、skills、MCP 客户端、`ext/extension-api` contributor | `plugin`、`core-plugins`、`skills`、`rmcp-client`、`ext/` |
| Code mode | 模型编写的脚本在 V8 宿主中运行，经同一策略层调用工具 | `code-mode-*`、`core/src/tools/code_mode` |
| 二次评审 | Guardian reviewer 替人审批越界动作，带断路器 | `core/src/guardian`、`guardian-context`、`ext/guardian-v2` |
| 身份 | ChatGPT 与 API key 登录、keyring 存储、agent identity JWT、workload identity | `login`、`keyring-store`、`agent-identity`、`workload-identity` |
| 遥测 | OpenTelemetry trace、metrics、日志事件；analytics 可关闭 | `otel`、`analytics` |

OpenAI 自己文档中的两条约束决定了设计走向：

- `codex app-server` "experimental and isn't supported for production workloads"，
  WebSocket 传输 "experimental and unsupported"
  （<https://developers.openai.com/codex/app-server>）。因此 Zuno 把 App Server 当作
  自己拥有并测试的内部边界，而不是从上游消费的公共 API。
- 命令级网络代理只约束沙箱内命令；MCP server、浏览器和模型流量都绕过它
  （<https://learn.chatgpt.com/docs/sandboxing.md>）。企业部署的出口控制必须在
  harness 之外完成。

## 2. Zuno 今天已经补上的部分

每一项都是 `FORK_DELTA.toml` 中已实现的能力。

- **单一静态二进制。** `zuno-standalone-x86_64-unknown-linux-musl` 内嵌 code-mode
  host，服务器部署就是一个文件，没有动态加载器，也不依赖 Node.js
  （`server-strict-standalone`）。
- **原生 ACP。** `zuno acp` 把 Agent Client Protocol 投影到同一套 thread 与 turn 之上，
  没有第二个 agent 循环（`native-acp`，见 `docs/zuno-acp.zh-CN.md`）。
- **严格审批。** `approval_policy = "untrusted"` 对每条命令和每次编辑都提示，除非有
  显式规则放行，包括向已批准 shell 写入的终端输入（`server-strict-standalone`）。
- **可组合的 Agent。** 插件可通过 `zuno.agent-backends/v1` 挂载原生 Codex、Claude Code
  或 ACP agent 工厂；用户拥有的 YAML 工作流用图引擎或动态脚本引擎编排它们，二进制里
  不内置任何业务工作流（`agent-backends`、`workflow-runtime`）。
- **跟踪上游。** 每个 Codex 正式版都被重放到已评审的 Zuno 差量上，只构建一次，再从封存
  字节晋升；仅因 Zuno 改写 Codex 文案而产生的冲突自动解决（`upstream-sync`，见
  `docs/zuno-upstream-sync.zh-CN.md`）。
- **继承物 fail closed。** 继承的安装器、daemon 更新器与标签发布器全部禁用，fork 不会
  悄悄改动机器（`zuno-ci-release`）。

## 3. 企业关注点与缺口

下表区分 harness 已提供的与必须自建的。「在其上构建」指工作落在 Zuno 自有路径中，
让上游同步成本接近零；「shared-modified」改动会连同显式契约记入 `FORK_DELTA.toml`。

| 关注点 | harness 现有原语 | Zuno 需要补齐的缺口 |
| --- | --- | --- |
| 身份与 SSO | ChatGPT 登录、API key、Bedrock IAM、`allowed_login_methods`、agent identity JWT | 把企业 IdP（OIDC）身份映射为 Zuno 主体；识别每个 ACP/App Server 会话的调用者；不依赖 ChatGPT 工作区 |
| 授权 | `requirements.toml` 分层、permission profile、`execpolicy`、MCP 与 marketplace 白名单 | 按主体、按项目的策略分发与版本化；策略变更审计 |
| 审计 | OTel 事件（`tool_decision`、`tool_result`、`api_request`）、rollout JSONL、hooks | 独立于工作机器的只追加审计汇；保留与脱敏策略；自托管场景下等价于 OpenAI Compliance API 的导出 |
| 密钥 | keyring 存储、对 `.env` 与 `~/.ssh` 的 `deny_read`、`secrets` 脱敏器 | vault 集成与按 thread 的短期凭据；日志、转写与审计共用同一脱敏策略 |
| 出口 | `network-proxy` 对沙箱内命令的 allow/deny | 覆盖 MCP、模型与浏览器流量的主机级出口策略；企业 CA 注入 |
| 隔离 | 操作系统沙箱；容器模式下用 `danger-full-access` 把隔离交给容器 | 每 thread 一个容器或 worktree；无 user namespace 主机的兜底策略 |
| 模型路由 | 自定义 `model_provider`（OpenAI 兼容、Azure、Bedrock、本地）、能力探测 | 按租户、成本与数据驻留的路由；集中配额 |
| 成本 | `thread/goal` token 预算、OTel 中的 token 用量 | 组织级预算、准入控制（今天 WebSocket 服务端过载时只会以 `-32001` 拒绝）、分账 |
| 可观测性 | OTel exporter、WebSocket 监听器上的 `/readyz` 与 `/healthz` | ACP → App Server → 工具的 trace 关联；SLO；集中日志 |
| 多租户 | 无：一个进程共享 `ZUNO_HOME`、配置与 code-mode host | 按租户分进程或容器；thread 归属校验；存储分区 |
| 部署 | stdio（稳定）、WebSocket/Unix socket（实验）、面向 CI 的 `codex exec` | 把单二进制作为服务生产化：TLS、认证、限流、外置 `ThreadStore` |
| 升级 | 上游承诺 App Server 向后兼容；每个 release 生成 JSON Schema | 钉住 Zuno 依赖的 App Server 与 ACP 表面的契约测试，在每个上游候选上运行 |
| 合规 | Guardian 策略可替换；`[analytics] enabled = false`；外部提示词扫描 hook | 自托管的提示词 DLP、数据驻留、超出来源白名单的插件溯源 |

## 4. 目标架构

```text
              企业客户端（IDE 经 ACP、CI 经 exec、Web 控制台）
                       │ ACP over stdio ─┐   │ App Server JSON-RPC（ws/uds）
                       ▼                 ▼   ▼
   ┌────────────────────────────────────────────────────────────────┐
   │  zuno（单一静态二进制）                                          │
   │                                                                │
   │  网关层（Zuno 自有）                                             │
   │    主体解析 · 策略包 · 准入 · 审计                                │
   │                                                                │
   │  Codex harness（App Server + core）                              │
   │    threads · turns · tools · sandbox · hooks · plugins · MCP   │
   │    guardian · code-mode host（内嵌）                             │
   │                                                                │
   │  Zuno 扩展                                                      │
   │    ACP 投影 · 工作流运行时 · agent backends                       │
   └───────────────┬───────────────────────────┬────────────────────┘
                   │ OTel + 审计事件            │ thread store（可插拔）
                   ▼                           ▼
        collector / SIEM / 对象存储        本地 SQLite+JSONL → 外部数据库
```

由第 1–3 节推出的设计规则：

1. **harness 始终是唯一的执行权威。** 网关层从不执行工具，也不与模型对话；它只决定
   调用者是谁、适用哪一份策略包、请求是否准入、记录什么。这与 ACP 适配器已遵守的规则
   相同（`zuno-acp/src/lib.rs`）。
2. **策略是数据，经由 harness 自己的通道下发。** 企业策略就是带签名的
   `requirements.toml` 包加 `execpolicy` 规则与 hook 白名单；Zuno 补的是分发与版本化，
   不是第二种策略语言。
3. **每一项企业能力都是 Zuno 自有 crate，或有文档的 shared-modified 路径。** 否则会
   抬高上游同步成本，而同步正是让 fork 保持最新的机制。
4. **上游实验性表面只包装、不暴露。** WebSocket 传输、`plugin/*`、permission profile、
   code mode 在上游都标为实验性。Zuno 用契约测试钉住自己依赖的精确行为，并在每个上游
   候选上运行，让变化表现为红掉的 PR 门禁而不是生产事故。
5. **Fail closed。** 缺少策略、审计汇不可达或主体未知都拒绝请求。这把现有的严格审批
   姿态从命令延伸到会话。

## 5. 路线图

阶段按依赖而非日历排序。每个阶段以契约记入 `FORK_DELTA.toml` 并被 PR 门禁覆盖为
结束标志。

### 阶段 0 —— 基线（已完成）

单二进制、原生 ACP、严格审批、插件 agent backends、工作流运行时、带改名重放的
自动上游同步。

### 阶段 1 —— 为 Zuno 依赖的表面建立契约测试

- 用请求/响应夹具钉住 ACP 适配器与工作流运行时消费的 App Server 方法和通知形状
  （`thread/*`、`turn/*`、`item/*`、审批），并在每个上游候选的 `zuno/pr-gate` 中运行。
- 对照已发布的 ACP schema 钉住 Zuno 实现的 ACP v1 表面（`IMPLEMENTED_METHODS`、
  错误码、prompt 块映射）。
- 结果：改变了被依赖形状的上游 release 会让候选 PR 失败，而不是被发布出去。

### 阶段 2 —— 主体与审计

- 新增 Zuno 自有的网关 crate，为每个 ACP 与 App Server 会话解析调用者主体
  （OIDC bearer token 或 mTLS 证书），作为会话元数据附着，并拒绝未知调用者。
- 对每一次审批决定、工具执行与模型请求写入一条审计记录到只追加的汇（先 OTel 日志
  exporter，后对象存储），脱敏策略与转写共用。
- 结果：每个动作都能回答「谁、做了什么、依据哪份策略、结果如何」，且不依赖
  ChatGPT 工作区。

### 阶段 3 —— 策略分发

- 把 `requirements.toml`、`execpolicy` 规则、hook 白名单与 MCP 白名单打成带签名的
  策略包，二进制启动时和定时拉取；企业模式下没有有效策略包就拒绝启动。
- 策略包版本化，每条审计记录写入策略包摘要，`zuno doctor` 展示当前摘要。
- 结果：管理员集中修改策略，并能证明过去某个动作运行在哪份策略之下。

### 阶段 4 —— 服务化部署

- 把单二进制作为长驻服务加固：TLS 终结、WebSocket 监听器上的阶段 2 认证、按主体的
  准入限制、与策略和审计可用性绑定的 `/readyz` 语义。
- 基于现有 `ThreadStore` trait 外置 thread 存储，让多个实例共享历史；先做进程级租户
  隔离（每租户一个二进制），再考虑进程内多租户。
- 结果：运维者像运行任何无状态服务一样运行 Zuno，状态在托管数据库中，租户彼此隔离。

### 阶段 5 —— 出口、密钥与成本

- 记录并测试覆盖模型、MCP 与浏览器流量的主机级出口拓扑（代理或网络策略）；注入企业 CA。
- 集成 vault，为交给 MCP server 与工具的按 thread 凭据服务。
- 在 harness 已有的 token 计量之上加组织预算与分账。

## 6. 非目标

- Zuno 不内嵌业务工作流、默认模型路由或默认插件集；它们留在用户、项目或插件根中。
- Zuno 不重新实现 agent 循环、沙箱或工具路由；企业能力包装 harness。
- Zuno 的企业故事不依赖 ChatGPT 工作区功能（Compliance API、automations、云端托管
  配置）；有则可用，但自托管路径必须在没有它们时也成立。

## 7. 来源

- OpenAI，《Unlocking the Codex harness: how we built the App Server》，2026-02-04：
  <https://openai.com/index/unlocking-the-codex-harness/>
- Codex App Server 文档：<https://developers.openai.com/codex/app-server>
- Codex 沙箱与审批：<https://learn.chatgpt.com/docs/sandboxing.md>、
  <https://learn.chatgpt.com/docs/agent-approvals-security.md>
- 托管配置：<https://learn.chatgpt.com/docs/enterprise/managed-configuration.md>
- Hooks、规则、插件：<https://learn.chatgpt.com/docs/hooks.md>、
  <https://learn.chatgpt.com/docs/agent-configuration/rules.md>、
  <https://learn.chatgpt.com/docs/build-plugins.md>
- Agent Client Protocol：<https://agentclientprotocol.com/>
- Codex 的参考 ACP 适配器：<https://github.com/agentclientprotocol/codex-acp>
