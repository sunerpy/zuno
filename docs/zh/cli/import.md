# zuno import

`zuno import` 把由 [`zuno export`](/zh/cli/export) 产出的 `.zuno-bundle` 还原到本机的用户
环境中。它是环境可移植性的接收侧：同一套配置、Skill、扩展与 Agent 会落到新机器上，无需
手工复制目录。

导入会写入真实的用户所有根目录。先运行 `--dry-run` 看看一个 bundle 会改动什么。`--replace`
会以事务方式替换非空的目标根目录，这会丢弃其中当前的内容，因此在使用它之前先确认 dry run
的结果。

## 用法

```sh
zuno import [OPTIONS] <bundle>
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `<bundle>` | 由 `zuno export` 产出的 `.zuno-bundle` 的路径 |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--replace` | 以事务方式替换非空的目标根目录 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--dry-run` | 验证并报告导入结果，但不改动文件 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

验证一个 bundle 并报告它会写入什么，同时不改动任何文件。

```sh
zuno import --dry-run ~/backups/zuno-environment.zuno-bundle
```

把 bundle 导入到空的目标根目录中。

```sh
zuno import ~/backups/zuno-environment.zuno-bundle
```

在 dry run 确认改动之后，以事务方式替换非空的目标根目录。

```sh
zuno import --replace ~/backups/zuno-environment.zuno-bundle
```

导入失败而摘要里看不出原因时，在 stderr 上追踪这次导入。

```sh
zuno import --dry-run --print-logs --log-level DEBUG ~/backups/zuno-environment.zuno-bundle
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno export](/zh/cli/export)
- [可移植 bundle](/zh/operate/portable-bundles)
- [配置参考](/zh/config/reference)
- [迁移](/zh/operate/migration)
