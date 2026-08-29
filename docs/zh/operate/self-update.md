# 自更新

`zuno self-update` 用 `sunerpy/zuno` GitHub Releases 中的预构建产物替换正在运行的 Zuno
可执行文件。它是一条原生 Rust 命令；不会调用安装脚本、包管理器或 shell。

```sh
zuno self-update --check
zuno self-update
zuno self-update --yes
zuno self-update --tag v0.2.0
zuno self-update --tag v0.2.0 --force --yes
```

- `--check` 只比较正在运行的包版本与最新发布版本。它与所有会产生变更的选项互斥。
- `--tag` 选择一个明确的 semver 发布版本。开头的 `v` 可选。
- `--force` 允许重新安装相同或更旧的所选版本。
- `--yes` 跳过终端确认。没有它时，非交互式输入会失败并拒绝，而不是静默替换二进制文件。

## 发布与完整性契约

更新器只支持由 `.github/workflows/release.yml` 发布的那些目标：

| 宿主 | 发布目标 | 归档格式 |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS aarch64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |

Linux 始终选择静态 musl 产物，即使当前运行的二进制文件是本地针对 GNU 目标构建的。资产
选择是精确匹配：`zuno-<version>-<target>.<archive>`。子串匹配和重复资产都会被拒绝。

在解压之前，Zuno 会下载该发布版本的 `SHA256SUMS`，为所选归档找到恰好一个摘要，计算本地
SHA-256 并进行比对。校验和缺失、重复、格式错误或不匹配都会在触碰当前可执行文件之前中止
操作。解压出的替换文件必须是非空的常规文件，并且在 Unix 上带有可执行模式。替换使用了
按平台适配的原子自替换实现。

## 认证、代理与权限

公开发布版本不需要凭据。对于私有仓库，请提供 `GITHUB_TOKEN` 或 `GH_TOKEN`；
`GITHUB_TOKEN` 优先，空值被忽略：

```sh
GH_TOKEN="$(gh auth token)" zuno self-update --check
GH_TOKEN="$(gh auth token)" zuno self-update --yes
```

Release API 与资产下载会继承进程的代理环境，包括 HTTP 客户端所支持形式的
`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY`。

Zuno 替换操作系统为当前运行进程解析出的那个可执行文件。如果该路径归另一个用户所有，请
改为安装到一个可写的 PATH 目录，例如 `$HOME/.local/bin`，或者用拥有现有文件的权限重新运行。
