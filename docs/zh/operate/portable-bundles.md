# 可移植 Zuno 环境 bundle

`zuno export` 与 `zuno import` 在 Linux、macOS 和 Windows 之间搬迁一份 Zuno 用户环境，
且不会嵌入源机器的绝对路径。归档文件是一个本地 ZIP 容器，后缀为 `.zuno-bundle`，包含带
版本的 `bundle.json` 清单、逻辑根目录、逐文件的 SHA-256 摘要、大小和文件模式。

这是环境备份，不是 session 导出。

## 包含哪些内容

默认情况下，`zuno export` 会遍历为当前进程解析出的两个 Zuno 自有的用户根目录：

- 全局 Zuno 配置根目录，包括 `zuno.json`、`AGENTS.md`、Agent、Skill、Markdown 命令、
  扩展、profile、主题，以及该根目录下其他由用户创建的文件；
- `$HOME/.zuno`，包括存放在其中的 Zuno 原生用户资产。

来自 `zuno-orchestration` 的内置 Skill 已编译进可执行文件，不需要在 bundle 中保留物理副本。
外部共享 Skill 根目录，例如 `~/.agents/skills` 和通过 `skills.paths` 显式选择的目录，不属于 Zuno 所有，
因此不会被导出。

默认 bundle 刻意排除：

- session 数据库、session 消息、transcript 以及 WAL/SHM 文件；
- Provider 与 MCP 凭据存储；
- 日志、缓存、快照、提示词历史、工具输出和临时文件；
- `.git`、`.omo` 和 `__pycache__` 目录。

如果一个符号链接的最终目标是同一个导出根目录内的常规文件，它会在该链接的逻辑路径上被
实体化为一个常规 bundle 文件。指向目录、被排除内容、外部根目录、断裂目标或特殊文件系统
条目的链接会被拒绝或排除，而不是被跟随。单个文件最大 256 MiB，解包后的载荷最大 1 GiB，
清单最多可描述 50,000 个文件。

## 导出

```sh
# Creates zuno-export-YYYYMMDDTHHMMSSZ.zuno-bundle in the current directory.
zuno export

# Choose the destination explicitly.
zuno export /path/to/workstation.zuno-bundle

# Replace an existing bundle file.
zuno export /path/to/workstation.zuno-bundle --force
```

输出文件不能放在任一导出根目录内部。导出会在目标位置旁边写入一个临时归档，同步它，然后
安装到位；除非带上 `--force`，不会覆盖已存在的文件。

凭据需要显式选择加入：

```sh
zuno export /path/to/private.zuno-bundle --include-credentials
```

这会加入解析出的 Provider 与 MCP 凭据存储。bundle 未加密；Zuno 会打印一条警告，运维者需要
自行负责受保护的传输、存储与删除。

## 导入

把 bundle 复制到目标机器，然后在改动文件之前先验证它：

```sh
zuno import /path/to/workstation.zuno-bundle --dry-run
```

导入只接受本地文件。它会验证格式与 schema 版本、清单与归档的一致性、大小上限、哈希、
路径安全性以及目标冲突。非空的目标根目录绝不会被隐式合并：

```sh
# Validate a replacement while leaving the destination unchanged.
zuno import /path/to/workstation.zuno-bundle --replace --dry-run

# Transactionally replace the roots carried by the bundle.
zuno import /path/to/workstation.zuno-bundle --replace
```

不带 `--replace` 时，已存在的非空目标会在暂存之前失败。带 `--replace` 时，Zuno 会把每个
根目录暂存到其目标位置旁边，把已有根目录移动为临时备份，并在后续某次替换失败时按逆序
回滚已提交的根目录。导入成功后会移除这些备份。

## 跨平台路径规则

清单只存储一个逻辑根目录以及使用正斜杠的相对路径。在导入时，Zuno 使用目标操作系统与
环境解析 `config`、`home-zuno` 以及可选的凭据根目录。它绝不会把一个 Linux 绝对路径还原到
Windows 上，反之亦然。

为了可移植性与解压安全，导出与导入会拒绝：

- 绝对路径、`.`/`..` 穿越、反斜杠、带盘符冒号的路径以及 NUL；
- 以点或空格结尾的路径片段；
- Windows 保留设备名，例如 `CON`、`NUL`、`COM1` 和 `LPT9`；
- 在大小写不敏感文件系统上会冲突的不同名称；
- 清单中缺失的归档条目，或清单未声明的意外条目。

Unix 权限位会在 Unix 上还原。在其他平台上，会在文件系统 API 允许的范围内应用可移植的
只读位。

## 推荐的迁移流程

1. 在源机器上运行 `zuno export`，不带凭据。
2. 通过受信通道传输 `.zuno-bundle`。
3. 在目标机器上运行 `zuno import ... --dry-run`。
4. 如果目标机器已有 Zuno 文件，先检查或备份它们，然后运行
   `zuno import ... --replace --dry-run`。
5. 运行对应的真实导入，并用 `zuno debug paths`、`zuno debug config`、`zuno debug skill`
   和 `zuno plugin list` 验证。
6. 在目标机器上重新登录 Provider 与 MCP server。只有在复制未加密的存储属于一个明确的
   安全决策时，才使用 `--include-credentials`。
