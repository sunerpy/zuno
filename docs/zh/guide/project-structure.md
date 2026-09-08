# 项目结构与执行流

修改行为之前，先用本页找到负责它的 crate。Zuno 是一个包含 48 个 crate 的 Rust
workspace。`crates/zuno-cli` 构建最终的 `zuno` 二进制，其余 crate 把协议、运行时、
存储、工具和客户端职责从入口程序中分离出来。

一项能力只有在接口、实现和消费者都存在时才算完整。如果三者生命周期不同，就应使用
独立 Component 或类型化服务，而不是继续扩大中央循环。

## 仓库布局

| 路径 | 内容 |
| --- | --- |
| `crates/` | 全部第一方 Rust crate，包括 CLI 二进制与测试 fixture |
| `docs/` | 本站的 Markdown 源文件；变更进入 `main` 后由 FirLab 发布 |
| `schemas/` | 经过检查的公共 schema，包括 `schemas/zuno.json` |
| `examples/` | 可运行的配置与集成示例 |
| `packaging/` | Release 打包与平台元数据 |
| `scripts/` | 安装、生成、发布和仓库维护脚本 |
| `benchmarks/` | 性能 workload 与已记录的测量输入 |
| `wit/` | Component 扩展使用的 WIT 契约 |
| `.github/` | CI、Release 与文档发布 workflow |
| `Cargo.toml` | Workspace 成员、统一依赖版本与 lint |
| `crates.expected` | 经过复核的第一方 crate 清单 |

每个 crate 都接入 workspace lint。第一方代码禁止 `unsafe`；操作系统 FFI 留在审计过的
依赖中。

## Crate 地图

### 产品入口

| Crate | 职责 |
| --- | --- |
| `zuno-cli`（`zuno`） | 解析命令、加载配置、组装默认 harness，并提供发布的可执行文件 |
| `zuno-tui` | 终端视图、输入、快捷键、主题与渲染；不拥有私有 Agent 循环 |
| `zuno-server` | HTTP 路由、认证、SSE、PTY 访问与 Server projection |
| `zuno-acp` | Zed 等编辑器使用的 Agent Client Protocol adapter |

### 运行时与编排

| Crate | 职责 |
| --- | --- |
| `zuno-runtime` | 有作用域的 Component 运行时、类型化服务、事务式 Profile 替换与 disposer 跟踪 |
| `zuno-harness` | 发布运行时使用的 `HarnessProfile` 与 `ProfileBundle` 组合 |
| `zuno-engine` | 回合循环、Prompt 组装、Provider stream、工具分发、压缩、重试与取消 |
| `zuno-agent` | Agent 定义、内置预设与原生子任务边界 |
| `zuno-orchestration` | 编译进 Zuno 的第一方 Agent、Skill 与编排描述 |
| `zuno-goal` | 持久 Goal 状态与继续执行策略 |
| `zuno-review` | 宿主采集的审核证据、经过验证的 Council 报告、Ready 门禁与持久审核事件投影 |
| `zuno-continuity` | Session History、Notes 服务及其工具 |
| `zuno-product-agent` | Codex 与 Claude Code Product Agent 的进程 adapter |
| `zuno-extension` | 静态与进程内 Extension Package 的验证和生命周期 |

### 模型、认证与外部协议

| Crate | 职责 |
| --- | --- |
| `zuno-llm` | Provider 无关的请求、stream event、能力与 Provider registry 接口 |
| `zuno-provider-openai` | OpenAI Responses 与 Chat Completions 协议 |
| `zuno-provider-anthropic` | Anthropic Messages、工具调用、推理与 cache-control 协议 |
| `zuno-provider-google` | Gemini、Vertex AI 与 Vertex 托管 Anthropic transport |
| `zuno-provider-bedrock` | Amazon Bedrock Responses 与 Converse transport |
| `zuno-provider-compatible` | 可配置的 OpenAI-compatible endpoint |
| `zuno-auth` | API key 与 OAuth 凭据存储和刷新 |
| `zuno-aws-auth` | AWS 凭据链解析与 SigV4 签名 |
| `zuno-network` | 统一的出站 HTTP client、代理路由与 transport 策略 |
| `zuno-mcp` | MCP stdio/remote client、工具、资源与 prompt |
| `zuno-lsp` | Language Server 进程池、请求与诊断 |

### 工具与操作系统副作用

| Crate | 职责 |
| --- | --- |
| `zuno-tool` | 原生 `Tool` trait、参数 schema、结果类型与暴露元数据 |
| `zuno-tools` | 内置文件、Shell、搜索、Web、工作状态、学习与委派工具 |
| `zuno-permission` | 有序工具调用规则与 ask/allow/deny 决策 |
| `zuno-sandbox` | 平台无关的命令准备与可用执行后端 |
| `zuno-process` | 子进程树约束与回收 |
| `zuno-pty` | 跨平台伪终端 Session 及其进程所有权 |
| `zuno-search` | 遵守 ignore 规则的路径与内容搜索 |
| `zuno-watch` | 合并并限流的文件系统变化事件 |
| `zuno-attachment` | 图像校验、存储引用与 Provider 附件 |
| `zuno-snapshot` | Worktree snapshot、diff 与 `/undo`/`/redo` 来源记录 |

### 持久状态与学习

| Crate | 职责 |
| --- | --- |
| `zuno-db` | SQLite schema、migration、Session、event、inbox、job 与持久 projection |
| `zuno-memory` | 有容量上限的常驻 Memory、候选校验、复核后应用与恢复 |
| `zuno-learning` | Experience 提取与检索、模式挖掘、反馈与 Skill candidate |
| `zuno-eval` | 为待审 Skill candidate 运行离线 cassette evaluation |
| `zuno-atomic-file` | Memory 等 projection 使用的可见性原子文件替换 |
| `zuno-types` | Session、Message、Part 与工具 payload 的共享 wire/domain 类型 |

### 配置与支撑

| Crate | 职责 |
| --- | --- |
| `zuno-config` | 配置发现、合并顺序、schema 类型与变量替换 |
| `zuno-catalog` | 从文件系统和配置发现 Agent、Skill、命令与 reference |
| `zuno-paths` | 数据、缓存、项目与 per-worktree 路径解析 |
| `zuno-error` | 跨 crate 使用的类型化错误与恢复分类 |
| `zuno-observability` | 有界结构化日志、安全 debug sink 与 span 约定 |
| `zuno-testkit` | 共享 fixture、Provider cassette 与集成测试 helper |
| `zuno-reaping-fixture` | 原生进程约束测试使用的进程树 fixture |

## 一个回合如何穿过系统

TUI、`zuno run`、ACP 与 HTTP 使用不同 transport，但在模型执行之前汇合。它们都不拥有
单独的回合循环。

1. **产品入口准入输入。** CLI 解析项目、配置、Agent、模型与沙箱策略。TUI、ACP 和
   HTTP 输入进入同一组宿主服务。任何模型可见输入都会先提交到持久 Session inbox，再尝试执行。
2. **Harness 挂载 Profile。** `zuno-harness` 组合包含类型化 Component 的 Bundle。
   `zuno-runtime` 在不启动副作用的情况下准备完整候选，校验通过后才启动 effect，并原子发布服务。
3. **`TurnHost` 选择 `AgentDriver`。** 默认 Driver 进入 `zuno-engine`；benchmark、
   evaluation、workflow 或 remote Profile 可以替换 Driver，而不改默认循环。
4. **Prompt 组装记录输入。** Agent 指令、运行时策略、已选 Skill、历史、常驻 Memory、
   检索到的 Experience、附件与工具 schema 组成稳定 section。Provider I/O 前会持久化最终
   post-hook Prompt 及其 digest。
5. **Provider 发送类型化 stream event。** `zuno-llm` 提供统一接口；一个原生
   Provider crate 或 `zuno-provider-compatible` 负责 wire protocol，`zuno-network` 负责
   出站路由与 deadline。
6. **工具调用通过两道独立门禁。** Registry 只暴露 Profile 与 Agent 允许的工具；
   `zuno-permission` 再判断具体调用是允许、拒绝还是需要人工回答。Shell 还要单独经过命令风险
   检查与所选 sandbox backend。
7. **结果成为持久输入。** 工具结果、Provider chunk、重试通知、问题、权限答复、子 Agent
   报告和终态都写成 Session event。带副作用的工具默认 at-most-once；结果不确定时先查权威状态，
   不机械重放。
8. **客户端消费 projection。** TUI、ACP 与 HTTP 读取同一套 event、工作状态、学习、权限和
   问题存储。实时 channel 只负责唤醒；重连或重启后仍以 SQLite 为准。

```text
CLI / TUI / ACP / HTTP
          |
          v
  Profile + 类型化 Component
          |
          v
 TurnHost -> AgentDriver -> Prompt receipt -> Provider
          |                                      |
          +-> Tool registry -> Permission -> Effect
          |                                      |
          +---------- Durable event/inbox <------+
                             |
                             v
                       Client projection
```

## Component 生命周期

Component 的 `prepare` 只暂存服务、依赖和延迟 effect，不让它们对外可见。全部 Component
准备成功后，effect 才会启动并返回 disposer。替换 Profile 时先撤下旧服务，按相反顺序停止，
新组合启动成功后才发布新的服务集合。

Disposer 失败或超时不算干净停止。运行时记录类型化 `Failed` 或 `Uncertain` 结果，并拒绝
挂载可能与未解决资源重叠的另一套组合。完整契约见
[Harness 运行时](/zh/operate/harness-runtime)。

## 修改应从哪里开始

| 改动 | 先看这里 | 文档负责人 |
| --- | --- | --- |
| CLI 参数或帮助 | `crates/zuno-cli/src/cmd/` | [`docs/zh/cli/`](/zh/cli/) |
| TUI 行为 | `crates/zuno-tui/` 及 `zuno-cli` 中对应宿主命令 | [终端应用](/zh/guide/tui) |
| HTTP 路由或 wire 行为 | `crates/zuno-server/src/api/` 与 `events/` | [HTTP API 与 OpenAPI](/zh/reference/http-api) |
| ACP 行为 | `crates/zuno-acp/` | [Zed 与 ACP](/zh/guide/editors) |
| 默认回合行为 | `crates/zuno-engine/` 与 Profile provider | [Harness 运行时](/zh/operate/harness-runtime) |
| Agent 名单或委派 | `zuno-agent`、`zuno-orchestration` | [Agent](/zh/guide/agents)、[编排](/zh/guide/orchestration) |
| 工具 schema 或执行 | `zuno-tool`、`zuno-tools` | [工具](/zh/guide/tools) |
| 权限或 Shell authority | `zuno-permission`、`zuno-sandbox`、`zuno-process` | [权限与沙箱](/zh/guide/permissions) |
| Provider protocol | `zuno-llm` 与一个 `zuno-provider-*` crate | [Provider 与凭据](/zh/config/providers) |
| 配置字段或合并 | `zuno-config`、`schemas/zuno.json` | [配置项参考](/zh/config/reference) |
| 持久 schema 或 migration | `zuno-db` | [数据库生命周期](/zh/operate/migration) |
| Memory 或学习 | `zuno-memory`、`zuno-learning`、`zuno-eval` | [Memory 与学习](/zh/guide/memory-learning) |
| Extension 生命周期 | `zuno-extension`、`zuno-runtime` | [开发 Agent 与扩展](/zh/guide/extension-development) |

修改默认 Agent 循环时还必须更新
[Harness 运行时](/zh/operate/harness-runtime)。新增或重命名公共页面时，要同时更新 Zuno
仓库入口与 FirLab 中英文侧栏。

## 验证边界

先运行负责该行为的 crate 测试。发布仓库级变更前运行共享 gate：

```sh
cargo fmt --all --check
cargo test -p <changed-crate>
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

进程、PTY、sandbox、打包和文件系统等平台相关行为，还需要对应原生 OS/架构上的证据。
Cross-compile 不能证明运行时语义。

## 相关页面

- [Zuno 是什么](/zh/guide/what-is-zuno)
- [Harness 运行时](/zh/operate/harness-runtime)
- [Agent 与扩展开发](/zh/guide/extension-development)
- [文档架构与覆盖地图](/zh/design/documentation-coverage)
