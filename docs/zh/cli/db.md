# zuno db

Session、提示、工具结果和 inbox 状态都存放在一个本地 SQLite 数据库里。`zuno db` 直接对它
执行查询，当 [`zuno session`](/zh/cli/session) 的成型输出回答不了你的问题时，这就是那道
逃生门。

它接受单个可选的 `QUERY` 参数，没有子命令。输出默认是 TSV，需要解析时可以改成 JSON。

查询是针对真实的持久存储执行的。优先使用只读语句；写入型语句会改变运行时视为权威的
harness 状态。

## 用法

```sh
zuno db [OPTIONS] [QUERY]
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `[QUERY]` | |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--format <FORMAT>` | 可选值：`json`、`tsv` | `tsv` |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

以 TSV 形式列出 session 数据库中的表。

```sh
zuno db "select name from sqlite_master where type = 'table' order by name"
```

统计已存储的 session 数量，并以 JSON 读取结果。

```sh
zuno db --format json "select count(*) as sessions from session"
```

直接从存储中读取按最后活动时间排序的最近 session。

```sh
zuno db "select id, title from session order by time_updated desc limit 5"
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno session](/zh/cli/session)
- [已排除的命令](/zh/cli/excluded)
- [Session 保留](/zh/operate/session-retention)
- [Harness 运行时](/zh/operate/harness-runtime)
