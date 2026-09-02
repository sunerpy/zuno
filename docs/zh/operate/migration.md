# Zuno 数据库生命周期

Zuno 管理自己的配置根目录和数据根目录。当前数据库格式为 8。空数据库直接创建为当前
格式；受支持的旧格式通过受保护的前向迁移升级。format 5 是第一个受支持的历史格式，
format 5、format 6 与 format 7 都会原地升级到 format 8，不需要重建数据库。

## Channel 数据库

切换二进制文件后会话列表为空，通常表示两个构建选择了不同数据库文件，而不是历史被删除。

文件名由构建 channel 决定：

| 条件 | 文件 |
|---|---|
| `ZUNO_DB` 为 `:memory:` | 内存中 |
| `ZUNO_DB` 是绝对路径 | 就是该路径，原样使用 |
| `ZUNO_DB` 是相对路径 | 拼接到数据目录之后，**不是**工作目录 |
| channel 为 `latest`、`beta` 或 `prod`，或者 `ZUNO_DISABLE_CHANNEL_DB` 恰好是 `1` 或 `true` | `zuno.db` |
| 其他情况 | `zuno-<channel>.db` |

源码构建没有 channel define，因此 channel 是 `local`，通常解析为 `zuno-local.db`；
已安装发布版解析为 `zuno.db`。

Linux 与 macOS：

```sh
ZUNO_DISABLE_CHANNEL_DB=1 zuno session list
ZUNO_DB="${XDG_DATA_HOME:-$HOME/.local/share}/zuno/zuno.db" zuno session list
```

Windows PowerShell：

```powershell
$env:ZUNO_DISABLE_CHANNEL_DB = "1"
zuno session list

$env:ZUNO_DB = Join-Path $HOME ".local\share\zuno\zuno.db"
zuno session list
```

`ZUNO_DISABLE_CHANNEL_DB` 会**区分大小写地**与恰好 `1` 或 `true` 比对。`TRUE`、
`yes` 和 `on` 不起作用。排查状态缺失前先运行 `zuno debug paths`。

当 session prune 报告无法把 artifact 归属到它打开的数据库时，也会给出警告。参见
[session-retention.md](/zh/operate/session-retention#读懂-artifact-警告)。

## 打开已存在的 Zuno 数据库

数据库打开流程识别以下状态：

1. **空数据库。** 完整的 format-8 schema 与唯一 `zuno_schema` marker 被原子创建。
2. **Format 8。** 在执行应用查询前校验 marker 与当前格式要求的表。
3. **Format 7。** 原地增加 `verification_receipt` 账本与其索引。
4. **Format 6。** 原地增加 Plan 栈字段、`work_plan_archive` 与 `verification_receipt`
   账本。
5. **Format 5。** 在一个事务中增加 learning schema、Plan 栈 schema 与
   `verification_receipt` 账本，并升级到 format 8。
6. **其他任何状态。** 不受支持的更旧格式、未来格式、缺少 marker，或 marker 与必需
   表不匹配，都会失败关闭且不修改文件。

### Format 5、6 或 7 到 format 8

受支持的迁移使用一个 SQLite `BEGIN IMMEDIATE` 事务：

1. 重新读取表清单，并要求 marker 恰好为 format 5、6 或 7。
2. 在任何变更前要求历史 `session` 与 `work_plan` 表存在。
3. 从 format 5 出发时，创建全部 format-6 learning 表和索引。
4. 从 format 5 或 6 出发时，增加可空的 `parent_plan_id`、默认值为 0 的 `stack_depth`
   与 `work_plan_archive`，不重写活跃 Plan 行。
5. 创建 `verification_receipt` 账本；它初始为空，不重写任何已有行。
6. 通过带旧值条件的更新把 singleton marker 从 5、6 或 7 改为 8。
7. 只有全部 schema 操作和 marker 更新成功后才提交。

任何失败都会回滚整个事务。迁移不会重写已有的 `session`、`message`、
`memory_candidate` 或 `work_plan` 值。测试构造精确的 format-5、format-6 与 format-7
形态，比较迁移前后的代表性行，再查询新增 learning、Plan archive 与 verification 表。

只修改 `zuno_schema.format` 永远不是有效修复：应用查询还需要与 marker 匹配的表和
索引。不要手工提升或降低 marker。

### 不受支持、未来或损坏格式

Zuno 会在执行应用查询前拒绝不受支持的 schema 格式，绝不会自动删除或重写被拒绝的数据库。
任何人工恢复前都应保留原文件并创建副本。

重要数据应使用对应旧二进制导出，或实现并验证明确的前向迁移。不要猜测 schema、静默
丢行，也不要要求当前二进制已经支持的格式重建数据库。有效的 format-5、format-6 或
format-7 数据库应当自动
打开并完成迁移。

## 未来 schema 变更规则

数据库格式一旦随 release 发布，schema 变更必须同时提供：

- 从每个仍声明受支持的格式出发的受保护前向迁移；
- 单一原子事务，并最后更新格式 marker；
- 精确的旧格式 fixture，而不是只修改当前 schema 的 marker；只有经过证明的纯加法变更
  才能通过删除全部新增表和索引来构造 fixture；
- 对持久用户数据进行行级迁移前后断言，至少覆盖代表性的 session、message 与 memory；
- 验证未来、无 marker 与结构损坏格式失败关闭且不发生修改。

不支持降级和尽力兼容。

## Provider 配置

Provider 覆盖范围按**线路协议族**声明，而不是按厂商名称。SigV4 加 EventStream、
Gemini 的线路格式配 Vertex 认证，以及 OpenAI 兼容族无法共用同一个请求构造器。

如果 Provider id 不被任何协议族声明，Zuno 会返回点名该 id 的错误，而不是静默尝试
OpenAI 兼容路线。可定位的显式失败正是预期结果。
