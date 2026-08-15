# Zuno

> 独立的 AI 编程代理。保留 [`opencode`](https://github.com/sst/opencode) 插件 ABI 接入，
> 但不以其二进制、配置或会话兼容为产品目标。

简体中文 · [English](docs/readme/README.en.md)

## 目录

- [项目定位](#项目定位)
- [安装](#安装)
- [快速开始](#快速开始)
- [文档](#文档)
- [独立运行与插件接入](#独立运行与插件接入)
- [构建与开发](#构建与开发)
- [非功能门禁](#非功能门禁)
- [许可证](#许可证)

## 项目定位

`zuno --version` 输出 `1.18.13`。这不是构建版本，而是有意固定的插件兼容版本：npm
插件会把运行版本作为 semver 范围判断条件，版本不匹配时会跳过加载。因此短版本必须保持为兼容
基线；如需真实构建身份，请显式使用长版本：

```console
$ zuno --version
1.18.13
$ zuno --version --long
Zuno 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
```

同时保留两种身份，是因为混用会导致插件无法加载，或向运维人员报告错误的构建身份。这项差异以
`split-version-identity` 明确登记。

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
或在克隆仓库后运行 `cargo install --path crates/oc-cli --locked` 从源码安装。

## 快速开始

```console
$ zuno --version
1.18.13
$ zuno --help
```

Zuno 只读取自己的配置与数据根目录；它不会导入或恢复 opencode 会话。

## 文档

| 页面 | 内容 |
| --- | --- |
| [docs/compatibility-matrix.md](docs/compatibility-matrix.md) | 每个接口面的状态：implemented、显式 503 gap、added、rejected、not-registered |
| [docs/divergences.md](docs/divergences.md) | 17 项有意差异及各自原因 |
| [docs/rejected-inputs.md](docs/rejected-inputs.md) | 已弃用配置、替代形式与准确错误信息 |
| [docs/migration.md](docs/migration.md) | Zuno 数据库文件选择、旧默认文件名诊断及 schema 演进 |
| [docs/session-retention.md](docs/session-retention.md) | C8 清理操作指南：`--archive` 可逆，`--delete` 不可逆 |
| [docs/plugin-authoring.md](docs/plugin-authoring.md) | 三类插件层级与 Rust 示例 |
| [docs/perf-methodology.md](docs/perf-methodology.md) | 内存和活性门禁的测量方法 |

只有 `<!-- generated:BEGIN … -->` 与 `<!-- generated:END … -->` 标记之间的区域从代码生成并由
`cargo test -p oc-cli --test docs` 做字节级防漂移检查；该测试还针对少量关键章节做派生断言。
标记外的说明性表格与 prose 仍需评审，不能因测试通过就视为已从代码生成。使用
`OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs` 重新生成受管区域。

## 独立运行与插件接入

Zuno 的默认配置根是 `$XDG_CONFIG_HOME/zuno`，项目配置目录是 `.zuno`，数据根是
`$XDG_DATA_HOME/zuno`。它不会回退读取 `$XDG_CONFIG_HOME/opencode`、项目 `.opencode` 或
`$XDG_DATA_HOME/opencode`，也不提供导入或恢复 opencode 会话的功能。旧路径只会在 oracle fixture、
上游源码说明或历史证据中以 **upstream-only** 身份出现。

插件层是唯一保留的 opencode 兼容层。`COMPATIBILITY_VERSION = "1.18.13"` 继续供 npm
`engines.opencode` semver 判断使用；以下六个名称继续作为插件 ABI，不是 Zuno 自身份遗留：
`OPENCODE_CLIENT`、`OPENCODE_CONFIG_CONTENT`、`OPENCODE_CONFIG_DIR`、
`OPENCODE_DISABLE_CLAUDE_CODE`、`OPENCODE_SERVER_PASSWORD`、`OPENCODE_SERVER_USERNAME`。
除此之外，Zuno 的用户界面、默认路径和自有环境变量均使用 Zuno 身份。

仓库暂时保留 differential suites 与 compatibility 文档作为已有验证资产；是否删除或重构它们尚待
单独决定。保留这些测试不等于把跨二进制兼容重新定义为产品目标，也不应据此新增会话导入、恢复或
旧目录 fallback。

## 构建与开发

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
make hooks
```

`unsafe_code` 在整个 workspace 中被禁止。`make hooks` 安装两个共享本地门禁：提交前运行格式化，
推送前运行快速测试；完整 workspace 测试仍由 CI 和显式 `make test` 执行。

## 非功能门禁

六项门禁支撑本实现的资源声明。下列数据均在 Linux 上实测，不是推算值。其中两项需要显式启用，
不属于普通测试套件；另有一项的一半从未在当前环境运行，限制见后文。

### G1 与 G2 — 峰值常驻内存

<!-- generated:BEGIN memory-gate-measurement -->
Derived from the newest committed measurement artefact,
[`.omo/evidence/task-123-opencode-rust.txt`](.omo/evidence/task-123-opencode-rust.txt).
The ceilings are not measured here:
[`benchmarks/ts-baseline.json`](benchmarks/ts-baseline.json) freezes each one
at half the TypeScript median for the same workload, and every other column
below is computed from the five per-repetition Rust peaks the artefact records.

| gate | workload | Rust median peak | frozen ceiling | margin | five-run spread | Rust / TypeScript | verdict |
|---|---|---:|---:|---:|---:|---:|---|
| G1 | `W-idle` | 20,380 KiB | 477,120 KiB | 456,740 KiB | 444 KiB | 0.0214 | PASS |
| G2 | `W-real` | 1,494,024 KiB | 1,513,496 KiB | 19,472 KiB | 17,032 KiB | 0.4936 | PASS |

G2's five `W-real` peaks were 1,493,496 · 1,493,948 · 1,494,024 · 1,510,444 ·
1,510,528 KiB. Every one of the five is under the ceiling, and the median's
19,472 KiB margin — 1.29% of the ceiling — is 2,440 KiB wider than the 17,032
KiB five-run spread. That ordering is the claim worth checking: a margin
narrower than the spread is a coin flip that landed, not a pass. The superseded
measurement in
[`.omo/evidence/task-122-opencode-rust.txt`](.omo/evidence/task-122-opencode-rust.txt)
is the shape being avoided: a 164,552 KiB spread around a median that finished
13,692 KiB over the same ceiling — FAIL.
<!-- generated:END memory-gate-measurement -->

### G3 至 G6

| 门禁 | 约束对象 | 实测值 | 上限 | 结论 |
| --- | --- | --- | --- | --- |
| G3 | 500 轮 soak 中每轮内存增长 | 0.0001775568 MiB/turn | 1.0 MiB/turn | PASS |
| G3 | 最终/中段峰值比 | 0.9938255268 | 1.5 | PASS |
| G4 | soak 期间的活性 | 两个上限均未触发 | 120 秒无状态进展；每轮 1800 秒硬截止 | PASS |
| G5 | 生产者/消费者边界的无界 channel | 17 个有界 + 2 个已声明例外，0 个未声明 | — | PASS |
| G6 | 父进程退出后的孤儿进程 | Linux 上 0 个孤儿，正常关闭和 `SIGKILL` 均验证 | — | Linux 上 PASS；Windows 部分未执行 |

### 四项明确限制

**G2 上限不会随测试对象缩放。** 它是固定值：同一 session 的一次 TypeScript 中位数的一半。
因此，即使代码不变，换成明显更大的 session 也可能使门禁转为 FAIL。上方 margin 与五次运行
spread 决定真实余量，两者的大小关系比任何单个数字更重要。

**G6 的 Windows 部分从未执行。** 上方实测结果来自
`crates/oc-process/tests/containment.rs`，该文件受 `#![cfg(target_os = "linux")]` 限制。
Windows Job Object 路径位于 `crates/oc-process/tests/windows_containment.rs`，受
`#![cfg(windows)]` 限制；它在 Linux 主机上是 **NOT EXECUTED**，不是“跳过但视为通过”，也不能
由 Linux 结果推断。只有在原生 Windows CI 或 Windows 主机上执行后，才能声明 G6 跨平台通过。

**`cargo test --workspace` 通过不代表 G1-G6 通过。** 高成本门禁需要显式启用，普通套件会跳过
或忽略它们：

```sh
# G1 + G2：仅当 mode 为 `run` 时执行。
OC_MEMORY_GATE_MODE=run cargo test -p oc-testkit --test memory -- --nocapture --test-threads=1

# G3 + G4：真实 driver soak。该测试被 #[ignore]，会占用两个真实 language server、
# 一个 50,000 文件 watcher、一个 PTY，以及两小时 wall clock。
OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test soak \
  g3_and_g4_real_driver_soak_stays_bounded_and_live -- \
  --ignored --exact --nocapture --test-threads=1

# G5 与 G6 会在普通套件中运行。
cargo test -p oc-testkit --test backpressure
cargo test -p oc-process --test containment
```

**G2 测试对象已固定，在其他环境复现需要重新捕获。** 实测 session 为
`ses_2bcaee257ffeFZNJrmtpi3ZglR`（931 条消息、3,620 个 part、105,118,812 part bytes），位于
一个以 sha256 标识的 2.6 GB 数据库快照中。`crates/oc-testkit/src/perf/subject.rs` 保存该 pin；
发生不匹配时会打印四步重新捕获流程，第四步要求重测 TypeScript 基线，因为测试对象与上限必须
来自同一次测量。没有该快照的机器会在 pin 校验处失败，而不会测量其他对象并称其为 G2。

测量方法、公式与冻结版本见 [docs/perf-methodology.md](docs/perf-methodology.md)。

## 许可证

本项目采用 [MIT License](LICENSE)。
