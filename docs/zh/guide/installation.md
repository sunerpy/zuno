# 安装

Zuno 每个平台只发布一个可执行文件。安装就是三件事：把二进制文件放到 `PATH` 上、校验它的 checksum、确认 `rg` 可用。其他什么都不需要安装，也不需要维护版本对齐。

## 前置条件

| 前置条件 | 原因 |
| --- | --- |
| Linux、macOS 或 Windows | 见下面的发布目标 |
| `rg`（ripgrep）14 或更新 | `glob` 与 `grep` 驱动真正的 ripgrep 可执行文件 |
| `bwrap`（bubblewrap）0.8.0 或更新，仅 Linux | `read-only` 与 `workspace-write` 沙箱后端需要 |
| `curl` 或 `wget`，以及 `tar` | 仅 shell 安装脚本需要 |

在 Linux 上如果没有可用的约束后端，受限沙箱模式会拒绝启动会话，而不是以无约束方式运行。在依赖任一受约束模式之前先安装 bubblewrap，并用 `zuno debug sandbox` 验证。完整的探测项清单以及 Ubuntu AppArmor 这个特例见[权限与沙箱](/zh/guide/permissions)。

## 安装脚本

安装脚本会下载发布归档及其 `SHA256SUMS`，比对该确切资产的摘要，不匹配时拒绝解包。checksum 校验失败是硬错误，绝不只是警告。

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

两个安装脚本都读取两个环境变量：

| 变量 | 含义 | 默认值 |
| --- | --- | --- |
| `ZUNO_VERSION` | 要安装的发布版本，带或不带前导 `v` 均可 | 最新已发布版本 |
| `ZUNO_INSTALL_DIR` | 目标目录 | `$HOME/.local/bin`；Windows 上为 `%LOCALAPPDATA%\Programs\zuno` |

```sh
ZUNO_VERSION=v0.2.0 ZUNO_INSTALL_DIR="$HOME/bin" sh -c "$(curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh)"
```

如果目标目录还不在 `PATH` 上，安装脚本会打印需要添加的那一行。

## 发布目标

| 宿主 | 目标 | 归档格式 |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS aarch64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |

Linux 始终使用静态 musl 产物。`aarch64-pc-windows-msvc` 是有意缺失的：没有可用的 runner 能执行该产物，而流水线不会发布一个自己从未运行过的二进制文件。

## 手动下载与校验

手动做就是安装脚本执行的那三步，在策略禁止把远程脚本管道给 shell 时，这是正确选择。

```sh
version=0.2.0
target=x86_64-unknown-linux-musl
base="https://github.com/sunerpy/zuno/releases/download/v${version}"

curl -fsSLO "${base}/zuno-${version}-${target}.tar.gz"
curl -fsSLO "${base}/SHA256SUMS"

grep " zuno-${version}-${target}.tar.gz\$" SHA256SUMS | sha256sum --check -
tar -xzf "zuno-${version}-${target}.tar.gz"
install -m 755 zuno "$HOME/.local/bin/zuno"
```

要校验你所用那个确切资产的摘要。否则，一个列出了五个归档的 `SHA256SUMS` 文件可以被用来「校验」另一个归档。

## 从源码构建

源码构建适用于本地开发，或者发布矩阵未覆盖的目标平台。

```sh
cargo install --git https://github.com/sunerpy/zuno zuno-cli --locked
```

源码构建没有 channel define，所以它的 channel 是 `local`，打开的是 `zuno-local.db` 而不是发布版的 `zuno.db`。在已安装的发布版与源码构建之间切换后立刻看到空的会话列表，是这个原因，不是数据丢了。如何让一个构建指向另一个的数据库，见[数据库生命周期](/zh/operate/migration)。

## 确认安装结果

```sh
zuno --version
zuno debug paths
zuno debug sandbox --mode workspace-write --check
```

`debug paths` 打印解析出的各个根目录，这是确认这个可执行文件实际使用哪些配置和数据目录的方式：

```text
home       /config
data       /config/.local/share/zuno
bin        /config/.cache/zuno/bin
log        /config/.local/share/zuno/log
repos      /config/.local/share/zuno/repos
cache      /config/.cache/zuno
config     /config/.config/zuno
state      /config/.local/state/zuno
tmp        /tmp/zuno
```

当请求的策略无法部署时，`debug sandbox --check` 会以失败退出，因此它可以当作部署门禁使用，而不是让人用眼睛去读。

## Shell 补全

```sh
zuno completion zsh > "${fpath[1]}/_zuno"
zuno completion bash > /etc/bash_completion.d/zuno
```

支持 `bash`、`elvish`、`fish`、`powershell` 和 `zsh`。参见 [zuno completion](/zh/cli/completion)。

## 升级

```sh
zuno self-update --check
zuno self-update
zuno self-update --tag v0.2.0
```

`self-update` 用一个经 checksum 校验的 GitHub release 替换正在运行的可执行文件。它会下载 `SHA256SUMS`，要求所选归档恰好有一条摘要，并且在任何不匹配的情况下都在触碰当前可执行文件之前停止。没有 `--yes` 时，非交互式调用会失败即拒绝，而不是静默替换二进制文件。

如果可执行文件路径属于另一个用户，请重新安装到一个可写的 `PATH` 目录，例如 `$HOME/.local/bin`，而不是用提升的权限运行更新器。参见[自更新](/zh/operate/self-update)。

## 卸载

没有一个真会干活的 `zuno uninstall`；这个命令存在只是为了说明这件事。请自己移除各个部分：

```sh
rm "$HOME/.local/bin/zuno"
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/zuno"
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/zuno"
```

数据根目录保存会话数据库、日志和凭据存储，所以移除它会丢弃持久的会话历史。如果其中任何内容还有用，先导出：

```sh
zuno export "$HOME/zuno-backup.zuno-bundle"
```

默认 bundle 携带配置、Skill、扩展和 Agent，并有意排除会话数据库与凭据存储。参见[可移植 bundle](/zh/operate/portable-bundles)。

## 参见

- [快速开始](/zh/guide/quick-start)
- [自更新](/zh/operate/self-update)
- [可移植 bundle](/zh/operate/portable-bundles)
- [数据库生命周期](/zh/operate/migration)
