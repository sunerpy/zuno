# 安装

Zuno 在每个平台上发布一个可执行文件。预编译二进制无需 Node、Python、ripgrep 或
bubblewrap，就能启动、加载配置、打开数据库、连接 Provider，并提供 TUI、headless、
ACP 与 HTTP 客户端。

## 依赖边界

| 前置条件 | 作用范围 |
| --- | --- |
| Linux、macOS 或 Windows | 支持的发布宿主见下表 |
| `rg`（ripgrep）14 或更新 | 只作为 `glob` 与 `grep` 工具后端；不是 Zuno 启动或核心运行依赖 |
| `bwrap`（bubblewrap）0.8.0 或更新 | 只作为 Linux 上 `read-only` 与 `workspace-write` 的受约束 Shell 后端 |
| `curl` 或 `wget`、`tar`、`sha256sum` 或 `shasum` | 仅 Linux/macOS 安装器需要 |
| Windows PowerShell 5.1 或 PowerShell 7 | Windows 安装器使用 `Invoke-WebRequest`、`Get-FileHash` 与 `Expand-Archive` |

没有 `rg` 时 Zuno 仍能启动，只有真正调用 `glob` 或 `grep` 的操作缺少后端。没有
`bwrap` 时 Linux 无法提供受约束执行，但这不会阻止 Zuno 本身启动，也不会阻止显式
受信的原生执行模式。

### 各平台沙箱行为

| 平台 | 受约束的 `read-only` / `workspace-write` | 原生执行 |
| --- | --- | --- |
| Linux | 需要受信的 bubblewrap 0.8.0 或更新版本 | 显式 `danger-full-access`、对每个 Agent 生效的受信 `sandbox.backend: native`，或符合条件且受信的 `workspace-write` `run-unconfined` 降级 |
| macOS | 尚未实现 | 显式 `danger-full-access`、对每个 Agent 生效的受信 `sandbox.backend: native`，或符合条件且受信的 `workspace-write` `run-unconfined` 降级 |
| Windows | 尚未实现 | 显式 `danger-full-access`、对每个 Agent 生效的受信 `sandbox.backend: native`，或符合条件且受信的 `workspace-write` `run-unconfined` 降级 |

`run-unconfined` 不是通用的“忽略沙箱”开关。它只在具备写能力的
`workspace-write` 请求遇到 typed、符合条件的后端不可用错误时生效。`read-only`
永不降级，仍然失败即拒绝；只读 Agent 唯一的原生路径是显式受信的
`sandbox.backend: native`（`--sandbox-backend native`），它保留权限模式，并把契约记录为
未强制执行。参见[权限与沙箱](/zh/guide/permissions)。

## Release 安装器

安装器会下载发布归档与 `SHA256SUMS`，只选取该确切资产对应的行，比对 SHA-256，
任何不匹配都会在解包前失败。

### Linux 与 macOS

Shell 安装器通过 `uname` 选择 x86_64 或 aarch64，需要 `curl` 或 `wget`、`tar`，
以及 `sha256sum` 或 `shasum`。默认安装到 `$HOME/.local/bin`：

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

固定版本或安装目录：

```sh
ZUNO_VERSION=vX.Y.Z \
ZUNO_INSTALL_DIR="$HOME/bin" \
sh -c "$(curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh)"
```

将 `X.Y.Z` 替换为准备安装的确切已发布版本。前面的未固定版本命令会解析最新公开版本。

### Windows

在 Windows PowerShell 5.1 或 PowerShell 7 中运行。安装器会根据原生进程架构选择
x86_64 或 ARM64 MSVC 归档，默认安装到 `$env:LOCALAPPDATA\Programs\zuno`：

```powershell
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

固定版本或安装目录：

```powershell
$env:ZUNO_VERSION = "vX.Y.Z"
$env:ZUNO_INSTALL_DIR = Join-Path $HOME "bin"
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

Windows 安装器会在目标目录的确切条目缺失时，将它安全地前置到用户 `PATH`，再单独
更新当前 PowerShell 进程，因此安装后可以立即运行 `zuno --version`。它只读取持久化
的用户值，保留 `%JAVA_HOME%\bin` 等引用，不调用 `setx`，也不会把当前进程合并后的
系统与用户 `PATH` 复制回用户值。其他已经打开的终端仍需重新启动。

## 发布目标

| 宿主 | 目标 | 归档格式 |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS aarch64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `.zip` |

Linux 使用静态 musl 产物。

## 手动下载与 checksum 校验

如果策略禁止把远程脚本通过管道交给 shell，就手动复现安装器步骤。Linux x86_64：

```sh
version=X.Y.Z
target=x86_64-unknown-linux-musl
asset="zuno-${version}-${target}.tar.gz"
base="https://github.com/sunerpy/zuno/releases/download/v${version}"

curl -fsSLO "${base}/${asset}"
curl -fsSLO "${base}/SHA256SUMS"
grep " ${asset}\$" SHA256SUMS | sha256sum --check -
tar -xzf "$asset"
install -m 755 zuno "$HOME/.local/bin/zuno"
```

Windows 根据原生进程架构选择目标：

```powershell
$version = "X.Y.Z"
$target = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) {
  "X64"   { "x86_64-pc-windows-msvc" }
  "Arm64" { "aarch64-pc-windows-msvc" }
  default { throw "不支持的 Windows 架构：$_" }
}
$asset = "zuno-$version-$target.zip"
$base = "https://github.com/sunerpy/zuno/releases/download/v$version"

Invoke-WebRequest "$base/$asset" -OutFile $asset
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS
$line = Get-Content SHA256SUMS |
  Where-Object { $_ -match "\s\*?$([Regex]::Escape($asset))$" } |
  Select-Object -First 1
if (-not $line) { throw "$asset is absent from SHA256SUMS" }
$expected = ($line -split "\s+")[0].ToLowerInvariant()
$actual = (Get-FileHash -Algorithm SHA256 $asset).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch for $asset" }
Expand-Archive $asset -DestinationPath .
```

必须匹配确切资产名。包含多个归档的 checksum 文件不能当作另一个文件的校验证明。

## 配置与数据路径

Zuno 在所有平台上都使用自己的 XDG 风格布局，包括 macOS 与 Windows：

| 平台 | 默认配置目录 | 默认持久数据目录 |
| --- | --- | --- |
| Linux | `${XDG_CONFIG_HOME:-$HOME/.config}/zuno` | `${XDG_DATA_HOME:-$HOME/.local/share}/zuno` |
| macOS | `${XDG_CONFIG_HOME:-$HOME/.config}/zuno` | `${XDG_DATA_HOME:-$HOME/.local/share}/zuno` |
| Windows | `$HOME\.config\zuno` | `$HOME\.local\share\zuno` |

Windows 配置不会自动放到 `%APPDATA%`。受管部署需要其他根目录时，可以设置
`XDG_CONFIG_HOME` 或 `ZUNO_CONFIG_DIR`。`ZUNO_CONFIG_DIR` 会增加最后一个更高优先级
的配置目录；使用 `zuno debug paths` 和 `zuno debug config` 核对解析结果。

PowerShell 示例：

```powershell
$config = Join-Path $HOME ".config\zuno"
New-Item -ItemType Directory -Force -Path $config | Out-Null
Copy-Item .\examples\config\zuno.json (Join-Path $config "zuno.json")
notepad (Join-Path $config "zuno.json")

# 可选的切换式覆盖层：
$env:ZUNO_CONFIG_DIR = Join-Path $config "profiles\work"
zuno debug paths
zuno debug config
```

## 从源码构建

源码构建需要：

- Git；
- Rust 1.98.0 与 Cargo；仓库门禁还需要 `rustfmt` 和 Clippy；
- 可用的 C 编译器和原生 linker，因为 bundled SQLite 与 `aws-lc-sys` 会构建原生代码；
- Linux：GCC 或 Clang，以及正常工作的原生 linker；
- macOS：Xcode Command Line Tools（`xcode-select --install`）；
- Windows：Visual Studio 2022 Build Tools、MSVC v143 C++ 工具链与 Windows SDK，
  并在 x64 developer 环境中运行。

ripgrep 与 bubblewrap 是前述运行时工具/后端依赖，不是源码编译前置。

```sh
rustup toolchain install 1.98.0 --component rustfmt clippy
git clone https://github.com/sunerpy/zuno.git
cd zuno
cargo build --locked -p zuno --bin zuno
cargo test -p zuno --test docs
```

通过 Cargo 安装尚未发布的 Git checkout：

```sh
cargo install --git https://github.com/sunerpy/zuno zuno --locked
```

源码构建的 channel 是 `local`，通常打开 `zuno-local.db`；发布版打开 `zuno.db`。
切换构建后会话列表看似为空，通常只是选中了不同的 channel 数据库。参见
[数据库生命周期](/zh/operate/migration)。

## 验证安装

Linux 与 macOS：

```sh
command -v zuno
zuno --version
zuno debug paths
```

Windows PowerShell：

```powershell
Get-Command zuno
zuno --version
zuno debug paths
```

单独验证可选工具后端：

```sh
rg --version
# 仅 Linux 受约束模式：
bwrap --version
zuno debug sandbox --mode workspace-write --check
```

macOS 与 Windows 上，受约束的 `workspace-write` 检查应报告 OS 后端尚未实现。若只想
验证显式原生路径而不运行模型任务：

```powershell
zuno debug sandbox --mode danger-full-access --check
```

## Shell 补全

可以先把脚本生成到 stdout，供检查或手工放置：

```sh
zuno completion bash
```

也可以安装到当前用户确定的补全目录：

```sh
zuno completion bash --install
zuno completion zsh --install
zuno completion fish --install
zuno completion powershell --install
zuno completion elvish --install
```

安装只会创建或原子替换补全文件，不会编辑任何 Shell profile；命令会打印安装路径与激活
说明。参见 [Shell 补全](/zh/cli/completion)。

## 升级

```sh
zuno self-update --check
zuno self-update
zuno self-update --tag vX.Y.Z
```

`self-update` 会先校验确切归档，再原子替换可执行文件。非交互式替换需要 `--yes`。
如果可执行文件不可写，请安装到用户拥有的目录，不要提升 updater 权限。参见
[自更新](/zh/operate/self-update)。

## 卸载

可执行文件、配置和持久数据需要分别移除。删除数据根目录会丢弃会话数据库、日志和凭据。

```sh
rm "$HOME/.local/bin/zuno"
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/zuno"
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/zuno"
```

Windows PowerShell：

```powershell
Remove-Item (Join-Path $env:LOCALAPPDATA "Programs\zuno\zuno.exe")
# 仅在确定不再需要配置和历史时删除：
Remove-Item -Recurse -Force (Join-Path $HOME ".config\zuno")
Remove-Item -Recurse -Force (Join-Path $HOME ".local\share\zuno")
Remove-Item -Recurse -Force (Join-Path $HOME ".cache\zuno")
```

删除持久数据前先导出需要保留的内容。可移植 bundle 有意排除会话数据库与凭据存储；
参见[可移植 bundle](/zh/operate/portable-bundles)。

## 参见

- [快速开始](/zh/guide/quick-start)
- [权限与沙箱](/zh/guide/permissions)
- [自更新](/zh/operate/self-update)
- [数据库生命周期](/zh/operate/migration)
