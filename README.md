# Zuno

> 独立的 AI 编程代理。用 Rust 写成，可加载 opencode 插件，但不以其二进制、配置或会话兼容为
> 产品目标。

简体中文 · [English](docs/readme/README.en.md)

## 目录

- [项目定位](#项目定位)
- [安装](#安装)
- [快速开始](#快速开始)
- [插件](#插件)
- [文档](#文档)
- [独立运行](#独立运行)
- [构建与开发](#构建与开发)
- [许可证](#许可证)

## 项目定位

Zuno 是一个独立的命令行 AI 编程代理：本地会话存储、可插拔的模型 provider、自带工具集，以及
一个内置 TUI。整个 workspace 禁止 `unsafe_code`。

它读自己的配置与数据，不读 opencode 的。跨二进制兼容不是目标 —— 唯一保留的兼容面是插件层，
见下方[插件](#插件)。

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

## 插件

Zuno 支持 opencode 插件：已安装的 npm 插件按其原有 ABI 加载，包括
`OPENCODE_CLIENT`、`OPENCODE_CONFIG_CONTENT`、`OPENCODE_CONFIG_DIR`、
`OPENCODE_DISABLE_CLAUDE_CODE`、`OPENCODE_SERVER_PASSWORD`、`OPENCODE_SERVER_USERNAME`
这六个握手环境变量。它们标识插件契约，不是 Zuno 自身的身份。

JavaScript 插件运行时默认关闭，需显式开启（`ZUNO_ENABLE_JS_PLUGINS=1`，或配置
`"plugin_runtime": {"javascript": true}`）—— 默认不启动 JS 运行时，启动耗时从约 1465 ms
降到约 30 ms。

**新插件推荐使用 Rust 编写。** 一等公民 SDK 是 `zuno-plugin-sdk`：进程内、无运行时依赖、
自带一致性测试套件。三类插件层级、21 个 hook 与可运行的 Rust 示例见
[docs/plugin-authoring.md](docs/plugin-authoring.md)。

## 文档

| 页面 | 内容 |
| --- | --- |
| [docs/plugin-authoring.md](docs/plugin-authoring.md) | 三类插件层级、hook 表与 Rust 示例 |
| [docs/compatibility-matrix.md](docs/compatibility-matrix.md) | 每个接口面的状态：implemented、显式 503 gap、added、rejected、not-registered |
| [docs/divergences.md](docs/divergences.md) | 17 项有意差异及各自原因 |
| [docs/rejected-inputs.md](docs/rejected-inputs.md) | 已弃用配置、替代形式与准确错误信息 |
| [docs/migration.md](docs/migration.md) | Zuno 数据库文件选择、旧默认文件名诊断及 schema 演进 |
| [docs/session-retention.md](docs/session-retention.md) | 清理操作指南：`--archive` 可逆，`--delete` 不可逆 |
| [docs/resource-gates.md](docs/resource-gates.md) | 六项资源门禁的实测结果、opt-in 命令与已知限制 |
| [docs/perf-methodology.md](docs/perf-methodology.md) | 内存和活性门禁的测量方法 |

只有 `<!-- generated:BEGIN … -->` 与 `<!-- generated:END … -->` 标记之间的区域从代码生成并由
`cargo test -p zuno-cli --test docs` 做字节级防漂移检查；该测试还针对少量关键章节做派生断言。
标记外的说明性表格与 prose 仍需评审，不能因测试通过就视为已从代码生成。使用
`ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs` 重新生成受管区域。

## 独立运行

Zuno 的默认配置根是 `$XDG_CONFIG_HOME/zuno`，项目配置目录是 `.zuno`，数据根是
`$XDG_DATA_HOME/zuno`。它不会回退读取 `$XDG_CONFIG_HOME/opencode`、项目 `.opencode` 或
`$XDG_DATA_HOME/opencode`，也没有任何接管 opencode 会话的途径：`zuno import` 只读取
Zuno 自己 `zuno export` 出的文档。旧路径只会在 oracle fixture、上游源码说明或历史证据中以
**upstream-only** 身份出现。

配置**文件名**同样是 Zuno 自己的：每一层都只读 `zuno.jsonc` 与 `zuno.json` —— 配置根、
从工作目录向上走到 worktree 根的裸文件、`.zuno/`、`OPENCODE_CONFIG_DIR` 指定的目录，以及
托管目录。仅支持 JSONC 与严格 JSON，**没有 TOML 配置路径**。`opencode.jsonc`、
`opencode.json` 以及配置根下的 `config.json` 都不再被读取；仍留着旧文件名的用户会得到一条
指明该文件、该目录与应改成的新文件名的启动错误，而不是被静默忽略。

除插件 ABI 之外，Zuno 的用户界面、默认路径和自有环境变量均使用 Zuno 身份。

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
