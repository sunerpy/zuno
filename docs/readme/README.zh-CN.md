<div align="center">

# Zuno

### 独立的 Rust AI 编程代理，可通过原生 Harness Runtime 组合不同的 Agent 驱动、能力和工具集

[![CI](https://github.com/sunerpy/zuno/actions/workflows/ci.yml/badge.svg)](https://github.com/sunerpy/zuno/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sunerpy/zuno)](https://github.com/sunerpy/zuno/releases)
[![License](https://img.shields.io/badge/license-MIT-green)](../../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98%2B-orange)](../../rust-toolchain.toml)

[安装](#安装) · [快速开始](#快速开始) · [Harness Runtime](#harness-runtime) · [文档](#文档) · [构建与开发](#构建与开发)

[English](../../README.md) · [**简体中文**](./README.zh-CN.md)

</div>

---

## 特性

- **单一静态二进制，无运行时依赖。** 不需要 Node、Python，也没有动态插件加载器；整个 workspace
  禁止 `unsafe_code`。
- **持久会话。** Prompt、工具结果、重试与子代理报告都可从 SQLite 事件重建，重启是续跑而不是重来。
- **可组合 Harness。** `AgentDriver` 与 `ToolManifest` 都是 Profile 服务，替换循环和工具面无需
  改动固定主循环。
- **原生扩展。** Agent、slash-command Workflow、Skill 与可执行工具都以受校验的
  `zuno.extension/v1` 包交付：进程内 WASI Component，或受控子进程。
- **可插拔 Provider。** OpenAI、Anthropic、Google、Bedrock 以及 OpenAI 兼容端点，全部通过原生
  Rust transport。
- **内置 TUI**，以及 headless、ACP 与 HTTP 面，它们消费同一套持久事件。

## 项目定位

Zuno 是一个独立的命令行 AI 编程代理：本地会话存储、可插拔的模型 provider、自带工具集，以及
一个内置 TUI。整个 workspace 禁止 `unsafe_code`。

Zuno 只定义自己的配置、数据、命令、工具参数和扩展协议。它不保留 OpenCode 插件 ABI、
JS hook、HTTP 兼容路由或配置适配层；扩展统一使用下方的原生
[Harness Runtime](#harness-runtime)。

## 安装

支持的平台是 Linux（x86_64、aarch64，静态 musl）、macOS（Intel 与 Apple 芯片）以及 Windows
x86_64。安装脚本会下载当前平台对应的归档，用该 release 的 `SHA256SUMS` 校验，摘要不匹配即
拒绝解包。

**安装脚本** —— Linux 与 macOS。URL 里的 tag 同时固定脚本与二进制：

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/v0.1.0/scripts/install.sh | sh
```

**安装脚本** —— Windows PowerShell：

```powershell
irm https://raw.githubusercontent.com/sunerpy/zuno/v0.1.0/scripts/install.ps1 | iex
```

`ZUNO_VERSION` 固定版本，`ZUNO_INSTALL_DIR` 修改安装目录，默认是 `$HOME/.local/bin`
（Windows 为 `%LOCALAPPDATA%\Programs\zuno`）：

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/v0.1.0/scripts/install.sh \
  | ZUNO_VERSION=0.1.0 ZUNO_INSTALL_DIR=/usr/local/bin sh
```

**预编译归档** —— 不希望把远程脚本管进 shell 时，从
[GitHub Releases](https://github.com/sunerpy/zuno/releases) 下载并自行校验。每个 release 提供
五个归档和一个 `SHA256SUMS`。

**从源码构建** —— 需要 [`rust-toolchain.toml`](../../rust-toolchain.toml) 固定的工具链和一个 C
编译器，因为 SQLite 与 TLS 栈都从源码构建：

```sh
cargo install --path crates/zuno-cli --locked
```

已安装的 Zuno 可原地检查和更新：

```sh
zuno self-update --check
zuno self-update
```

更新器精确选择当前平台的 release 归档，先用同一 release 的 `SHA256SUMS` 校验，再原子替换
当前可执行文件。`--tag v0.2.0` 固定版本，`--yes` 用于非交互确认；完整安全契约见
[Self-update](../reference/self-update.md)。

## 快速开始

```console
$ zuno --help
$ zuno --version --long
```

首次配置从仓库内已校验的原生 provider 示例开始；它使用 Rust `openai` transport，不安装
Node 包，也不加载 AI SDK：

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
install -m 600 examples/config/zuno.json "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
zuno debug config
zuno models myopenai --verbose
```

如果使用预编译安装而没有源码 checkout，直接在同一配置路径创建
[`examples/config/zuno.json`](../../examples/config/zuno.json) 所示内容。Provider 配置只接受原生
`transport`；不接受 `npm` 字段。

`zuno export` 会生成跨 Linux、macOS、Windows 可移植的 `.zuno-bundle`，包含解析后的
Zuno 全局配置根目录和 `$HOME/.zuno`：配置、`AGENTS.md`、Agent、Skill、Markdown 命令、
扩展、Profile 等用户资产。会话数据库、对话、日志、缓存与凭据默认不导出；凭据只有在显式
使用未加密的 `--include-credentials` 时才会进入包。`zuno import` 会校验摘要和可移植路径，
支持 `--dry-run`，并要求先使用 `--replace` 才会事务性替换非空目标。详见
[Zuno 可移植环境包](../reference/portable-bundles.md)。

在 TUI 中粘贴受支持的本地图片路径或剪贴板图片会生成 `[Image #N]` 附件；
`@relative/path` 可加入有界的项目文本或图片，headless 模式可重复使用 `zuno run -f`。
详见[图片与文件引用](../reference/attachments.md)。

## Harness Runtime

Zuno 的扩展单元是原生 Rust `Component`。多个组件组成 `ProfileBundle`，再由
`HarnessProfile` 在一次事务中挂载：候选 Profile 完整验证后才发布，失败时按逆序回滚，热替换
期间旧 Profile 始终可用。`AgentDriver` 与 `ToolManifest` 都是 Profile 服务，因此 benchmark、
workflow、远程执行器或专用编码 Agent 可以替换循环和工具面，而无需修改固定主循环。

会话输入进入持久化 FIFO inbox；用户 prompt、运行中 steer 与子代理报告共享同一交付协议。
Agent 工作时，`Enter` 默认排入下一轮 FIFO，`Ctrl+Enter` 才在下一个安全边界强制 steer，
`Shift+Enter` 用于换行。后台 `task` 支持 `reportDelivery: nextStep | quiet`，完成状态可用
`job` 工具查询。
`web_search` 接收 `queries` 数组，并发执行、首错取消兄弟请求、等待收敛后按稳定顺序合并及 URL
去重。活动 Goal 对网络、限流、断流、数据库锁和单轮步数耗尽使用持久化指数退避；进程重开后按
SQLite 中的 deadline 恢复，人工输入优先。工具失败作为 tool result 交回模型，不会被调度器
机械重放；只有显式声明 `Safe` 的只读或幂等工具才允许模型在后续 turn 重试，不确定的副作用
必须先核对权威状态。认证、取消与永久配置错误分别暂停或阻断。

`build`、`plan`、`deep` 与各专用 Agent 都来自同一个原生 catalog。Provider 无关的
`PromptEnvelope` 会把 kernel、Agent Role、策略、AGENTS、工作状态、Skill 与 memory 保持为
独立区块直到编码阶段；OpenAI Responses 将 kernel 与角色映射到 `instructions`，其余区块映射
为 developer input，并保持用户消息原样。每次 receipt 都以 `session.prompt.assembled` 持久化，
可用 `zuno debug prompt` 查看默认脱敏结果。完整生命周期、恢复矩阵和配置见
[docs/harness-runtime.md](../harness-runtime.md)。

## 扩展 Agent 与 Workflow

自定义 Agent 可以放在项目的 `.zuno/agent/` 下。文件路径决定默认名称，frontmatter 决定
模型、模式、权限和步数，正文就是可追踪 prompt section 的内容：

```markdown
---
description: Review a change for security and authorization defects
mode: subagent
permission:
  mode: standard
  rules:
    "*": deny
    read: allow
    glob: allow
    grep: allow
    lsp: allow
    webfetch: allow
    web_search: allow
    shell: ask
---

Inspect repository files, relevant environment facts, current external evidence,
trust boundaries, permission checks, durable state, and failure behavior.
Return findings with exact file locations.
```

Agent 也可以通过模型工具动态声明一个 `zuno.extension/v1` 包，其中包含 Agent、slash-command
Workflow 和 Skill。`extension_define` 只在当前进程内记录不可变定义，
`extension_run` 激活；`extension_stop`、`extension_undefine` 和
`extension_inspect` 管理生命周期。TUI 会在该轮结束后于同一进程内重组下一轮，退出 Zuno
后动态定义即丢失。

需要跨重启保留时，把同一清单写到
`.zuno/extensions/<id>/extension.json`（全局路径为
`~/.config/zuno/extensions/<id>/extension.json`）并重启。两种加载方式共用验证与冲突检查；
扩展中 `mode: subagent | all` 的 Agent 会进入 `task` 的真实目标列表，并在子会话中使用自身
模型、Prompt 与原生工具权限。文件由 `read/glob/grep/lsp/edit` 提供，网络由
`webfetch/web_search` 提供，环境与普通进程能力由受权限控制的 `shell` 提供；Strict 模式仍会
对所有副作用调用逐次 HITL。

静态包还可以注册可执行工具：首选进程内、按 workspace/network/env 最小授权并带 fuel、内存和
超时限制的 WASI Component；确实需要完整宿主 API 时使用声明 `host.full` 的受控子进程。
两者都作为 Profile effect 初始化，卸载时先撤销路由再逆序异步清理；无法确认停止则标记
`Uncertain`，不会自动重放。Zuno 不执行 JavaScript/Cordis ABI，也不加载 Rust 动态库。
`host.full`、WASI `network` 或 `workspace.write` 工具不能声明只读/安全重放，因此 Strict
模式不会被插件清单中的虚假只读标记绕过。

```sh
zuno plugin add examples/plugins/review-kit --project
zuno plugin list
zuno plugin update examples/plugins/review-kit --project
zuno plugin remove review-kit --project
```

完整清单、能力表、WIT/JSON-RPC 协议以及自定义 Agent、Workflow、WASI 和进程示例见
[插件与扩展指南](../plugins.md)。

原生 workflow 不需要修改默认循环。实现 `AgentDriver`，选择模型可见的 `ToolManifest`，按需
加入原生工具，再把它们组成一个事务化 Profile：

```rust
let profile = zuno_harness::profile_with_tools(
    "release-review",
    Arc::new(ReleaseReviewDriver::new()),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep, BuiltinSlot::Task])?,
    ToolContributions::new([Arc::new(ReleaseSummaryTool::new())])?,
);

runtime.activate_profile(profile).await?;
```

Profile 中还可以挂载 provider、远程执行器、审批、评测或 benchmark 组件。每个注册动作返回
精确 disposer；候选 Profile 完整验证后一次发布，失败则逆序回滚。客户端只消费公共命令、
持久事件、inbox 和 projection，因此同一个扩展可被 TUI、headless、ACP、HTTP 以及未来 GUI
复用。设计取舍见 [Harness 对比](../design/harness-comparison.md)，客户端接口见
[GUI/客户端架构](../design/client-interfaces.md)。

## 文档

| 页面 | 内容 |
| --- | --- |
| [docs/README.md](../README.md) | 后续文档站的信息架构与完整导航 |
| [docs/reference/self-update.md](../reference/self-update.md) | Release 选择、SHA-256 校验、认证、代理与原子替换 |
| [docs/reference/configuration.md](../reference/configuration.md) | `zuno.json` Schema、配置层与独立 `tui.json` |
| [docs/reference/providers.md](../reference/providers.md) | Provider、凭证、`myopenai` 与原生 Rust 请求链 |
| [docs/harness-runtime.md](../harness-runtime.md) | 原生组件、Profile 事务、持久 inbox 与自定义 Harness |
| [docs/plugins.md](../plugins.md) | 插件安装、Agent/Workflow、WASI/进程能力与协议示例 |
| [docs/design/harness-comparison.md](../design/harness-comparison.md) | DSH、Codex、OMO、pi-agent、OpenCode 与 Claw Code 的借鉴决策 |
| [docs/design/client-interfaces.md](../design/client-interfaces.md) | TUI、ACP、HTTP 与未来 GUI 共用的事件和投影接口 |
| [docs/design/zed-acp-integration.md](../design/zed-acp-integration.md) | 稳定协议固定版本、Zed 配置、HITL、diff、持久回放与验收步骤 |
| [docs/design/memory-learning.md](../design/memory-learning.md) | 可审计记忆候选、反思提取、审核、晋升与撤销 |
| [docs/logging.md](../logging.md) | 多进程安全结构化日志、`RUST_LOG`、脱敏、保留与明文诊断 |
| [docs/migration.md](../migration.md) | Zuno 数据库文件选择与 schema 演进 |
| [docs/session-retention.md](../session-retention.md) | 清理操作指南：`--archive` 可逆，`--delete` 不可逆 |
| [docs/resource-gates.md](../resource-gates.md) | 六项资源门禁的实测结果、opt-in 命令与已知限制 |
| [docs/perf-methodology.md](../perf-methodology.md) | 内存和活性门禁的测量方法 |

`cargo test -p zuno-cli --test docs` 校验 Harness 指南覆盖运行时、持久交付和并发搜索，并防止
README 再次宣传已经退役的兼容面。

## 独立运行

Zuno 的默认配置根是 `$XDG_CONFIG_HOME/zuno`，项目配置目录是 `.zuno`，数据根是
`$XDG_DATA_HOME/zuno`。其他产品的根目录和文件不是 Zuno 输入，也不会被探测、迁移或解释。

配置**文件名**同样是 Zuno 自己的：每一层都只读 `zuno.jsonc` 与 `zuno.json` —— 配置根、
从工作目录向上走到 worktree 根的裸文件、`.zuno/`、`ZUNO_CONFIG_DIR` 指定的目录，以及
托管目录。仅支持 JSONC 与严格 JSON，**没有 TOML 配置路径**。其他文件名只是同目录中的
普通文件，不进入 Zuno 的配置图。

Zuno 的用户界面、默认路径、环境变量与扩展协议均使用 Zuno 身份。

## 构建与开发

```sh
make build
./dist/zuno --version --long
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
make hooks
```

`make build` 保留 Cargo 的 debug 构建，并在成功后将可直接运行的二进制原子投放到
`dist/zuno`；原始 Cargo 产物仍位于 `target/debug/zuno`。`make release` 会用优化构建覆盖
`dist/zuno`，其原始产物位于 `target/release/zuno`。

`make hooks` 安装两个共享本地门禁：提交前运行格式化，推送前运行快速测试；完整 workspace
测试仍由 CI 和显式 `make test` 执行。资源门禁另需显式启用，见
[docs/resource-gates.md](../resource-gates.md)。

## 许可证

本项目采用 [MIT License](../../LICENSE)。
