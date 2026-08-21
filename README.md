# Zuno

> 独立的 Rust AI 编程代理，可通过原生 Harness Runtime 组合不同的 Agent 驱动、能力和工具集。

简体中文 · [English](docs/readme/README.en.md)

## 目录

- [项目定位](#项目定位)
- [安装](#安装)
- [快速开始](#快速开始)
- [Harness Runtime](#harness-runtime)
- [文档](#文档)
- [独立运行](#独立运行)
- [构建与开发](#构建与开发)
- [许可证](#许可证)

## 项目定位

Zuno 是一个独立的命令行 AI 编程代理：本地会话存储、可插拔的模型 provider、自带工具集，以及
一个内置 TUI。整个 workspace 禁止 `unsafe_code`。

它读自己的配置与数据，不读 opencode 的。Zuno 不保留 OpenCode 插件 ABI、JS hook、HTTP
兼容路由或配置兼容层；扩展统一使用下方的原生 [Harness Runtime](#harness-runtime)。

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

`zuno export` 与 `zuno import` 构成 Zuno 自身的导出/导入闭环：`import` 只接受 `export`
生成的本地文档，不接受 opencode 会话，也不接受 share URL。两者都是**顶层命令**，不是
`session` 的子命令 —— `zuno session` 只有 `list`、`prune`、`delete`。

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
机械重放；认证、取消与永久配置错误分别暂停或阻断。完整生命周期、恢复矩阵、配置和自定义
Harness 示例见
[docs/harness-runtime.md](docs/harness-runtime.md)。

## 文档

| 页面 | 内容 |
| --- | --- |
| [docs/harness-runtime.md](docs/harness-runtime.md) | 原生组件、Profile 事务、持久 inbox 与自定义 Harness |
| [docs/rejected-inputs.md](docs/rejected-inputs.md) | 已弃用配置、替代形式与准确错误信息 |
| [docs/migration.md](docs/migration.md) | Zuno 数据库文件选择、旧默认文件名诊断及 schema 演进 |
| [docs/session-retention.md](docs/session-retention.md) | 清理操作指南：`--archive` 可逆，`--delete` 不可逆 |
| [docs/resource-gates.md](docs/resource-gates.md) | 六项资源门禁的实测结果、opt-in 命令与已知限制 |
| [docs/perf-methodology.md](docs/perf-methodology.md) | 内存和活性门禁的测量方法 |

`cargo test -p zuno-cli --test docs` 校验 Harness 指南覆盖运行时、持久交付和并发搜索，并防止
README 再次宣传已经退役的兼容面。

## 独立运行

Zuno 的默认配置根是 `$XDG_CONFIG_HOME/zuno`，项目配置目录是 `.zuno`，数据根是
`$XDG_DATA_HOME/zuno`。它不会回退读取 `$XDG_CONFIG_HOME/opencode`、项目 `.opencode` 或
`$XDG_DATA_HOME/opencode`，也没有任何接管 opencode 会话的途径：`zuno import` 只读取
Zuno 自己 `zuno export` 出的文档。旧路径只会在 oracle fixture、上游源码说明或历史证据中以
**upstream-only** 身份出现。

配置**文件名**同样是 Zuno 自己的：每一层都只读 `zuno.jsonc` 与 `zuno.json` —— 配置根、
从工作目录向上走到 worktree 根的裸文件、`.zuno/`、`ZUNO_CONFIG_DIR` 指定的目录，以及
托管目录。仅支持 JSONC 与严格 JSON，**没有 TOML 配置路径**。`opencode.jsonc`、
`opencode.json` 以及配置根下的 `config.json` 都不再被读取；仍留着旧文件名的用户会得到一条
指明该文件、该目录与应改成的新文件名的启动错误，而不是被静默忽略。

Zuno 的用户界面、默认路径、环境变量与扩展协议均使用 Zuno 身份。

## 构建与开发

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
make hooks
```

`make hooks` 安装两个共享本地门禁：提交前运行格式化，推送前运行快速测试；完整 workspace
测试仍由 CI 和显式 `make test` 执行。资源门禁另需显式启用，见
[docs/resource-gates.md](docs/resource-gates.md)。

## 许可证

本项目采用 [MIT License](LICENSE)。
