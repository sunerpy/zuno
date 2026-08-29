# Zuno 数据库生命周期

Zuno 只读取自己的配置根目录和数据根目录。由于项目尚未发布，数据库直接以当前格式创建，
永远不会通过增量迁移链升级。

## Channel 数据库

这是第一件让人意外的事，看起来像状态丢失：**你运行二进制文件，而 session 列表是空的。**

什么都没丢。是两个构建选择了不同的数据库文件。

文件名由构建 channel 决定：

| 条件 | 文件 |
|---|---|
| `ZUNO_DB` 为 `:memory:` | 内存中 |
| `ZUNO_DB` 是绝对路径 | 就是该路径，原样使用 |
| `ZUNO_DB` 是相对路径 | 拼接到数据目录之后，**不是**工作目录 |
| channel 为 `latest`、`beta` 或 `prod`，或者 `ZUNO_DISABLE_CHANNEL_DB` 恰好是 `1` 或 `true` | `zuno.db` |
| 其他情况 | `zuno-<channel>.db` |

从源码构建没有 channel define，因此它的 channel 是 `local`，解析出 `zuno-local.db`。已安装的
发布版本解析出 `zuno.db`。这两个文件名及其数据根目录都属于 Zuno。

要从源码构建去读取 Zuno 发布版的数据库，选其中一种：

```sh
ZUNO_DISABLE_CHANNEL_DB=1 zuno session list
ZUNO_DB="${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db" zuno session list
```

`ZUNO_DISABLE_CHANNEL_DB` 是**区分大小写地**与恰好 `1` 或 `true` 比对。`TRUE`、`yes` 和
`on` 不起作用 ——
`crates/zuno-paths/src/files.rs::disable_channel_db_forces_the_unsuffixed_name_case_sensitively`
固定了这一点。

在得出任何结论之前，先确认你在哪个文件上：

```sh
zuno debug paths
```

当 session prune 报告无法把 artifact 归属到它打开的那个数据库时，它也会给出警告。参见
[session-retention.md](/zh/operate/session-retention#读懂-artifact-警告)。

## 打开一个已存在的 Zuno 数据库

Zuno 尚未发布，并且刻意没有增量数据库迁移链。它识别两种状态：

1. **空文件。** 完整的当前 schema 和一个 `zuno_schema` 格式标记被原子地创建。
2. **非空文件。** 格式标记必须等于 `zuno_db::migration::CURRENT_FORMAT`；否则这个不受支持的
   预发布格式会被拒绝，且文件不被修改。

不存在 ALTER、回填、降级或尽力兼容的路径。schema 变更会提升格式版本，开发数据库随之重建。
这让当前 schema 成为唯一的事实来源，也避免把未发布的历史带进产品。

当某个格式被拒绝时，如果其中的数据重要就先保留该文件，然后自行选择一个新的数据库路径，
或者自己删除旧的开发数据库：

```sh
ZUNO_DB=/tmp/zuno-current.db zuno
```

Zuno 绝不会自动删除或重写一个被拒绝的数据库。

## Provider 配置

Provider 覆盖范围是按**线路协议族**声明的，而不是按厂商名称。SigV4 加 EventStream、
Gemini 的线路格式配 Vertex 认证，以及 OpenAI 兼容族无法共用同一个请求构造器，因此声明的
单位是族。

实际后果是：如果你的 Provider id 不被任何族声明，你会得到一个点名该 id 的错误，而不是被
静默地尝试按 OpenAI 兼容 profile 路由。一个点名 id 的失败正是预期结果 —— 它比一个以错误
形状发出去的请求更容易处理。
