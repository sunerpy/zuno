# Zuno

> 独立的 Rust AI 编程代理，可通过原生 Harness Runtime 组合不同的 Agent 驱动、能力和工具集。

简体中文 · [English](docs/readme/README.en.md)

## 目录

- [项目定位](#项目定位)
- [安装](#安装)
- [快速开始](#快速开始)
- [Harness Runtime](#harness-runtime)
- [扩展 Agent 与 Workflow](#扩展-agent-与-workflow)
- [文档](#文档)
- [独立运行](#独立运行)
- [构建与开发](#构建与开发)
- [许可证](#许可证)

## 项目定位

Zuno 是一个独立的命令行 AI 编程代理：本地会话存储、可插拔的模型 provider、自带工具集，以及
一个内置 TUI。整个 workspace 禁止 `unsafe_code`。

Zuno 只定义自己的配置、数据、命令、工具参数和扩展协议。它不保留 OpenCode 插件 ABI、
JS hook、HTTP 兼容路由或配置适配层；扩展统一使用下方的原生
[Harness Runtime](#harness-runtime)。

## 安装

Linux 与 macOS 可使用一行安装脚本。仓库为私有仓库，因此先运行 `gh auth login`；下列命令
通过已认证的 GitHub CLI 读取脚本，并将 token 传给 release 下载。脚本默认安装到
`$HOME/.local/bin`：

```sh
GH_TOKEN="$(gh auth token)" sh -c "$(gh api -H 'Accept: application/vnd.github.raw+json' repos/sunerpy/zuno/contents/scripts/install.sh)"
```

可通过 `ZUNO_VERSION` 固定版本，通过 `ZUNO_INSTALL_DIR` 修改安装目录：

```sh
ZUNO_VERSION=0.1.0 ZUNO_INSTALL_DIR=/usr/local/bin \
  GH_TOKEN="$(gh auth token)" sh -c "$(gh api -H 'Accept: application/vnd.github.raw+json' repos/sunerpy/zuno/contents/scripts/install.sh)"
```

也可以从 [GitHub Releases](https://github.com/sunerpy/zuno/releases) 下载对应平台的预编译归档，
或在克隆仓库后运行 `cargo install --path crates/zuno-cli --locked` 从源码安装。

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
[`examples/config/zuno.json`](examples/config/zuno.json) 所示内容。Provider 配置只接受原生
`transport`；不接受 `npm` 字段。

`zuno export` 与 `zuno import` 构成 Zuno 自身的导出/导入闭环。两者都是**顶层命令**，
不是 `session` 的子命令；`zuno session` 只有 `list`、`prune`、`delete`。

## Harness Runtime

Zuno 的扩展单元是原生 Rust `Component`。多个组件组成 `ProfileBundle`，再由
`HarnessProfile` 在一次事务中挂载：候选 Profile 完整验证后才发布，失败时按逆序回滚，热替换
期间旧 Profile 始终可用。`AgentDriver` 与 `ToolManifest` 都是 Profile 服务，因此 benchmark、
workflow、远程执行器或专用编码 Agent 可以替换循环和工具面，而无需修改固定主循环。

会话输入进入持久化 FIFO inbox；用户 prompt、运行中 steer 与子代理报告共享同一交付协议。
后台 `task` 支持 `reportDelivery: nextStep | quiet`，完成状态可用 `job` 工具查询。
`web_search` 接收 `queries` 数组，并发执行、首错取消兄弟请求、等待收敛后按稳定顺序合并及 URL
去重。活动 Goal 对网络、限流、断流、数据库锁和单轮步数耗尽使用持久化指数退避；进程重开后按
SQLite 中的 deadline 恢复，人工输入优先。工具失败作为 tool result 交回模型，不会被调度器
机械重放；只有显式声明 `Safe` 的只读或幂等工具才允许模型在后续 turn 重试，不确定的副作用
必须先核对权威状态。认证、取消与永久配置错误分别暂停或阻断。

`build`、`plan`、`deep` 与各专用 Agent 都来自同一个原生 catalog。最终发给模型的 prompt
按 agent、策略、memory、instructions、skills 分段组装，并在请求前以
`session.prompt.assembled` 持久化其顺序、来源、内容摘要和 hook 后的实际文本。完整生命周期、
恢复矩阵和配置见 [docs/harness-runtime.md](docs/harness-runtime.md)。

## 扩展 Agent 与 Workflow

自定义 Agent 可以放在项目的 `.zuno/agent/` 下。文件路径决定默认名称，frontmatter 决定
模型、模式、权限和步数，正文就是可追踪 prompt section 的内容：

```markdown
---
description: Review a change for security and authorization defects
mode: subagent
permission:
  edit: deny
  bash: deny
---

Inspect trust boundaries, permission checks, durable state, and failure behavior.
Return findings with exact file locations.
```

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
复用。设计取舍见 [Harness 对比](docs/design/harness-comparison.md)，客户端接口见
[GUI/客户端架构](docs/design/client-interfaces.md)。

## 文档

| 页面 | 内容 |
| --- | --- |
| [docs/README.md](docs/README.md) | 后续文档站的信息架构与完整导航 |
| [docs/reference/configuration.md](docs/reference/configuration.md) | `zuno.json` Schema、配置层与独立 `tui.json` |
| [docs/reference/providers.md](docs/reference/providers.md) | Provider、凭证、`myopenai` 与原生 Rust 请求链 |
| [docs/harness-runtime.md](docs/harness-runtime.md) | 原生组件、Profile 事务、持久 inbox 与自定义 Harness |
| [docs/design/harness-comparison.md](docs/design/harness-comparison.md) | DSH、Codex、OMO、pi-agent、OpenCode 与 Claw Code 的借鉴决策 |
| [docs/design/client-interfaces.md](docs/design/client-interfaces.md) | TUI、ACP、HTTP 与未来 GUI 共用的事件和投影接口 |
| [docs/migration.md](docs/migration.md) | Zuno 数据库文件选择与 schema 演进 |
| [docs/session-retention.md](docs/session-retention.md) | 清理操作指南：`--archive` 可逆，`--delete` 不可逆 |
| [docs/resource-gates.md](docs/resource-gates.md) | 六项资源门禁的实测结果、opt-in 命令与已知限制 |
| [docs/perf-methodology.md](docs/perf-methodology.md) | 内存和活性门禁的测量方法 |

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
[docs/resource-gates.md](docs/resource-gates.md)。

## 许可证

本项目采用 [MIT License](LICENSE)。
