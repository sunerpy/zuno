# zuno export

`zuno export` 把一个 Zuno 安装中属于用户的那一侧收集进单个 `.zuno-bundle` 文件：配置、
Skill、扩展、Agent 以及其他用户资产。用它把环境搬到另一台机器、在做有风险的改动前留一份
快照，或者把一套可复现的配置交给别人。

除非你主动要求，凭据不会被包含。`--include-credentials` 会把 Provider 与 MCP 凭据存储写入
一个未加密的 bundle，因此这样产出的 bundle 必须当作机密来处置：任何能读到该文件的人都能
使用那些凭据。

## 用法

```sh
zuno export [OPTIONS] [bundle]
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `[bundle]` | Bundle 路径；默认为 `zuno-export-<UTC timestamp>.zuno-bundle` |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--include-credentials` | 把 Provider 与 MCP 凭据存储包含进未加密的 bundle 中 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--force` | 替换已存在的输出文件 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

在当前目录用默认的带时间戳文件名写出一个 bundle。

```sh
zuno export
```

写到显式路径，好让备份任务能找到它。

```sh
zuno export ~/backups/zuno-environment.zuno-bundle
```

覆盖该路径上已存在的 bundle。

```sh
zuno export --force ~/backups/zuno-environment.zuno-bundle
```

包含凭据存储，并接受产出文件未加密、必须像机密一样被保护这一事实。

```sh
zuno export --include-credentials ~/private/zuno-with-credentials.zuno-bundle
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno import](/zh/cli/import)
- [可移植 bundle](/zh/operate/portable-bundles)
- [配置参考](/zh/config/reference)
- [Provider 参考](/zh/config/providers)
