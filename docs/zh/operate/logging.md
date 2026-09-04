# 运维日志

Zuno 把两类持久数据分开：

- Session 事件包含重建一次请求所需的确切模型可见提示词、外部输入、工具结果、重试和
  subagent 报告。
- 运维日志包含有界的诊断元数据：进程生命周期、session/turn 关联、Provider 尝试、工具
  生命周期、耗时、带类型的结果，以及资源事件。

运维日志不能变成第二份 transcript。在记录写出之前，所有文本 sink 与 SQLite sink 都会让
字段值经过同一个基于字段名的脱敏判定：字段名拼出 `authorization`、`api_key`、
`access_token`、`refresh_token`、`private_key`、`password`、`passphrase`、`secret`、
`credential`、`cookie`、`token`、`bearer`、`signature`、`prompt`、`content`、`body`、
`command`、`input`、`output`、`report`、`stdin`、`stdout`、`stderr` 的字段会写成
`[redacted]`。分隔符与大小写会被忽略，因此 `api_key`、`apiKey`、`api-key`、`API.KEY`
是同一个名字；规则复数同样匹配（`commands`、`cookies`、`access_tokens`）。Zuno 无法归类的
字段名（含非 ASCII 字节的，或没有任何可识别成分的）按失败即拒绝处理，一律脱敏。脱敏只替换
单个字段的值：记录本身、它的事件文本以及其余字段仍会写出，所以脱敏不会让你失去这条诊断。

有两个例外是有意保留的，都属于需要提前规划的边界：

- **只拼出「量」的字段名保持可读**，这样调用方有安全的方式报告规模：`command_bytes`、
  `prompt_tokens`、`output_tokens`、`input_tokens`、`content_tokens`、`command_tokens`、
  `secret_bytes`、`cookie_count`。只有上述五个 token 计量类别才允许 `tokens` 的复数读法；
  `secret_tokens`、`cookie_tokens`、`auth_tokens`、`total_tokens`、`max_tokens` 都会被脱敏。
  绝不要把值放进以「量」命名的字段：`secret_bytes` 必须存放字节数。
- **名为 `message` 的字段永不脱敏。** `message` 是 `tracing` 给事件自身文本的字段名，
  formatter 打印它时不带 `name=` 前缀，因此调用点放在这里的任何内容都会原样写入纯文本日志、
  `--print-logs` 的 stderr，以及 `logs.sqlite` 的 `message` 列，并且读起来就是事件消息本身。
  Zuno 目前仍有若干调用点把外部文本经由 `message` 输出（最需要知道的是 MCP server 的
  stderr 行），所以纯文本日志中可能出现子进程的原始输出。请给载荷单独的字段名，让判定能够
  归类它，或只记录一个「量」（`bytes`、`limit`、`truncated`）。

需要模型可见载荷的组件必须使用 session 事件日志。

## 默认存储

每个初始化运行时的命令都以 `INFO` 级别写入：

```text
$XDG_DATA_HOME/zuno/log/logs.sqlite
```

该数据库使用 SQLite WAL 和五秒的 busy timeout，因此多个 TUI、headless、ACP 和 server
进程可以并发写入。初始的 WAL/schema 锁竞争会以有界退避重试。每条记录都携带
`process_uuid` 和 `pid`；在运行时 span 下发出的记录还携带 `session_id`、`turn_id`、
`tool_call_id`、Provider、模型、尝试次数和操作。

保留策略由写入方强制执行：

- 最新的 50,000 条记录；
- 大约 32 MiB 的记录载荷；
- 不保留超过 10 天的记录。

该队列是有界且允许丢弃的，而不是让它阻塞一次 Agent 轮次。关闭时会刷新排队的记录，并把
丢弃的记录或写入失败报告到 stderr。

检查示例：

```sh
sqlite3 "$XDG_DATA_HOME/zuno/log/logs.sqlite" \
  "select datetime(timestamp_ms / 1000, 'unixepoch'), level, target, message
   from log_record order by id desc limit 50;"
```

## 级别与过滤器

`INFO` 是默认值。简单的 CLI/环境变量控制方式是：

```sh
zuno --log-level DEBUG
ZUNO_LOG_LEVEL=TRACE zuno
ZUNO_PRINT_LOGS=1 zuno
```

接受 `TRACE`、`DEBUG`、`INFO`、`WARN` 和 `ERROR`。`--print-logs` 与 `ZUNO_PRINT_LOGS=1`
会增加一个 stderr sink；stdout 永远不作为日志目的地，因为 ACP 和其他 stdio 协议在那里
封帧数据。

没有设置显式的 Zuno 级别时，可以使用标准的、按 target 过滤的 Rust 方式：

```sh
RUST_LOG='zuno_engine=trace,zuno_tools=debug,zuno_db=warn' zuno
```

显式的 `--log-level` 或 `ZUNO_LOG_LEVEL` 是进程级覆盖，优先于 `RUST_LOG`。

## 可选的纯文本日志

纯文本日志默认关闭。只在一次有界的调试过程中启用它：

```sh
ZUNO_PLAINTEXT_LOGS=1 zuno
```

每个进程创建自己的文件：

```text
zuno.<pid>.<process_uuid>.log
```

在 Unix 上，日志目录是 `0700`，`logs.sqlite` 与纯文本文件都是 `0600`。按进程区分的文件名
避免了写入交错和跨进程轮转竞争。在有界的运维历史方面，结构化存储始终是权威。

## 运行时插桩

真实的运行时，而不只是测试 fixture，会打开：

- 每个 `RunTurnRequest` 一个 `turn` span；
- 每次 Provider 尝试一个 `provider_request` span，包括标题、摘要、压缩以及普通轮次操作；
- 每次已准备的分发一个 `tool_call` span，带有 pending、running、completed、blocked、error
  或 abandoned 生命周期记录。

Provider 诊断记录带类型的结果/状态元数据，而不是请求或响应正文。破坏性命令风险门禁记录
判定结果、shell 语法和命令字节长度，绝不记录命令本身。它的判定值是 `run`、`confirm` 和
`deny`；`confirm` 意味着已有的 `shell` 权限请求被升级为一次新的、面向在场用户的决策，
而不是意味着模型应该换参数重试。TUI 中断请求记录 session id 以及它是否中断了一个活跃轮次；
它绝不记录提示词、模型输出或工具参数。实时 steering 同样只记录 session 与准入标识符，加上
活跃轮次是被唤醒了，还是那条持久输入仍处于 pending。
