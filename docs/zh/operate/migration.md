# Zuno 数据库生命周期

Zuno 管理自己的配置根目录和数据根目录。当前数据库格式为 14。空数据库直接创建为当前
格式；受支持的旧格式通过受保护的前向迁移升级。format 5 是第一个受支持的历史格式，
format 5 到 format 13 都会原地升级到 format 14，不需要重建数据库。
迁移链显式覆盖 format 5、format 6、format 7、format 8、format 9、format 10、format 11、format 12 与 format 13。

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

1. **空数据库。** 完整的 format-14 schema 与唯一 `zuno_schema` marker 被原子创建。
2. **Format 14。** 应用查询前校验 marker、表、约束、索引和触发器。
3. **Format 5–13。** 在同一事务中按顺序执行所有剩余迁移：学习（6）、Plan 栈（7）、
   验证账本（8）、会话记忆策略（9）、执行／收件箱状态（10）、记忆版本与检索（11）、
   自动记忆来源与处理水位（12）、持久问题与会话调度（13），以及输入处理回执、
   统一 Context 快照与绑定 revision 的 Goal 恢复选择（14）。
4. **其他任何状态。** 不受支持的更旧格式、未来格式、缺少 marker，或 marker 与必需
   表不匹配，都会失败关闭且不修改文件。

两个进程同时打开或升级同一个数据库时，都按拿到 SQLite 写锁之前看到的 format 做决定。
拿锁失败的一方不会报错：它会重新读取 marker，对赢家实际提交的结果做校验、升级或拒绝，
总共最多尝试四次。不支持的 format 仍然报告为 schema 不匹配；如果 format 在打开过程中
持续变化，则以 `zuno_schema` marker 上的冲突失败关闭。两种路径都不会写库。

### Format 5–13 到 format 14

受支持的迁移使用一个 SQLite `BEGIN IMMEDIATE` 事务：

1. 重新读取表清单，并要求 marker 恰好为 format 5、6、7、8、9、10、11、12 或 13。
2. 在任何变更前要求历史 `session` 与 `work_plan` 表存在。
3. 从 format 5 出发时，创建全部 format-6 learning 表和索引。
4. 从 format 5 或 6 出发时，增加可空的 `parent_plan_id`、默认值为 0 的 `stack_depth`
   与 `work_plan_archive`，不重写活跃 Plan 行。
5. 创建 `verification_receipt` 账本；它初始为空，不重写任何已有行。
6. 创建 `session_memory_policy`；它初始为空，因此已有会话继续采用调用方给出的默认值。
7. 为 `session_input` 增加 `source_key`、`trigger_kind` 与 `cycle_id`，创建
   `session_execution_state`、`completion_delivery` 及其索引；旧输入保留
   `trigger_kind = 'legacy'`。
8. 对 format 11 之前的格式，增加常驻记忆版本、来源验证、租约令牌、检索快照、
   Unicode／CJK 增量索引。
9. 增加候选的可空 `base_revision`／`evidence` 字段、`resident_memory_provenance`、
   `memory_maintenance_state` 及其索引。回填可精确关联的自动记忆来源，不把用户后来的修改
   重新归类为自动记忆。
10. 创建 `question_interaction`、`question_action_receipt` 及索引；为有效旧问题添加带稳定
    问题项 ID 的 companion row，不改写原 `human_request` payload/response。
11. 为 `session_execution_state` 添加可空 `scheduling`。仅在最新结构化 driver event
    确切为 `paused/no_progress` 时修复错误保留 `running` 的旧行，保留周期和进度。
12. 保留已发布问题定义的原始内容，把 purpose 约束扩展为允许原生 `goal_resume`；
    创建 `session_input_receipt`、`session_context_usage` 及索引。旧 consumed 输入只映射为
    `recorded`，没有供应商请求证据就不能晋升为 `applied` 或 `completed`。
13. 最后用精确旧值条件把 marker 更新为 14；全部成功后才提交。

任何失败都会回滚整个事务。迁移不会重写已有的 `session`、`message`、
`memory_candidate`、`verification_receipt` 或 `work_plan` 值；来源验证和租约仅执行已说明的回填。
测试使用 format-5 到 format-13 的精确发布 fixture，比较 Session、Message、Memory 等保留值，
再验证新增对象和 marker。Format-11 用例包含已经发布的记忆版本，并验证最后一个索引创建失败时
整个升级回滚。不需要重建用户数据库。

Format 14 迁移不会调用模型、提炼 Memory、编造 Context 数值或恢复暂停 Goal。
历史补证与必要重处理由可恢复学习任务完成。先用 `/learn repair-history --dry-run`
检查，再决定是否执行 `/learn repair-history`；来源缺失的记录继续保持未验证。

### Session execution 与 completion delivery

`session_execution_state` 把协作模式与所选 Agent 分开持久化，同时保存 Work identity、
精确授权与 handoff-ready 的 Plan revision、continuation cycle、context epoch，以及用户
显式接受 Draft review 风险的原因。`/start-work` 会在同一个 `BEGIN IMMEDIATE` 事务中
读取 Plan 与 review gate、更新 Goal、写入 Work authorization，并接纳 `UserControl` 输入。

`completion_delivery` 是后台命令、子 Agent、workflow 与 product Agent 的 exactly-once
消费权账本。终态 `bg output`、同步 `bg wait` 与异步 callback 竞争唯一的 `inline` 或 `callback` owner；
失败的一方不能再接纳第二个 turn。`session_input.source_key` 保证重启后的 producer
幂等接纳，`trigger_kind` 则区分 user、control、automatic 与 recovery turn，无需伪造
user message。

没有 Goal 的普通会话也使用可执行、指定人工/外部等待、暂停、完成这套调度状态。
callback 不能解除无关等待，也不能伪造来源周期。经验证的回答和显式恢复控制通过同一个
事务性 inbox 消费门控进入模型。

问题把已提交回答与 `draftAnswers` 分开保存；“稍后”和空回答不产生模型输入。
revision 与 command receipt 保障幂等。Plan 批准绑定精确 Plan/review/Work 身份与
来源周期，提前批准只有在该逻辑周期正常交接后才可生效。

`session_input_receipt` 分开记录接收、写入历史、进入模型和执行完成。Goal 暂停期间
记录了 callback，不代表模型已经处理。恢复选择绑定 Goal ID/revision 与已有输入 ID，
确认才原子衔接 Goal、执行状态和输入；跳过不授权，也不重投旧用户消息。

`session_context_usage` 按来源保存 tracker、revision、epoch 和更新时间。主会话、子 Agent、
学习及压缩请求不会互相覆盖上下文窗口。计量由持久请求证据恢复，未知值保持未知。

### Per-session memory policy

`session_memory_policy` 是与 session 一对一的 sidecar，绝不写入不透明的
`session.metadata`。

- `use_memories` 控制该会话是否可以使用 resident 或 retrieved memory。
- `generation` 只能是 `enabled`、`disabled` 或 `excluded`。
- `reason`、`source` 与更新时间构成当前选择的审计依据。
- `revision` 使用 compare-and-set；revision 0 表示尚不存在持久行。

迁移后旧会话缺行时，单纯读取不会写数据库，reader 会返回调用方提供的精确默认值。
新会话会在首次创建持久行时冻结该默认值，child session 在创建事务中继承父会话 policy。
`set` 与 `exclude` 会在同一个事务中更新 policy，并向持久 session event stream 追加
`session.memory.policy.changed`。revision 过期时既不写 policy，也不写 event。

`disabled` 是可恢复的生成关闭状态，会把该会话已排队的自动 extraction job 标记为
`skipped`；`excluded` 执行相同的队列结算，并记录该会话因配置的外部上下文而不能重新
启用。running 或已终结的 job 不会被重放或改写。

只修改 `zuno_schema.format` 永远不是有效修复：应用查询还需要与 marker 匹配的表和
索引。不要手工提升或降低 marker。

### 不受支持、未来或损坏格式

Zuno 会在执行应用查询前拒绝不受支持的 schema 格式，绝不会自动删除或重写被拒绝的数据库。
任何人工恢复前都应保留原文件并创建副本。

重要数据应使用对应旧二进制导出，或实现并验证明确的前向迁移。不要猜测 schema、静默
丢行，也不要要求当前二进制已经支持的格式重建数据库。有效的 format-5、format-6、
format-7、format-8、format-9、format-10 或 format-11 数据库应当自动打开并完成迁移。

## 未来 schema 变更规则

数据库格式一旦随 release 发布，schema 变更必须同时提供：

- 从每个仍声明受支持的格式出发的受保护前向迁移；
- 单一原子事务，并最后更新格式 marker；
- 精确的旧格式 fixture，而不是只修改当前 schema 的 marker；看起来是纯加法的变更也没有例外：
  当迁移通过 `ALTER TABLE` 达到新形状时，列顺序与全新创建的数据库不同，因此由当前 schema
  反推出来的 fixture 走不到真实用户数据库所走的那条路径；
- 比较方式为结构等价，即表、列、类型、索引、外键与 marker，而不是比较 `sqlite_master`
  原文 —— 已迁移库与全新库的原文本来就会不同；
- 对持久用户数据进行行级迁移前后断言，至少覆盖代表性的 session、message 与 memory；
- 验证未来、无 marker 与结构损坏格式失败关闭且不发生修改。

不支持降级和尽力兼容。

## Provider 配置

Provider 覆盖范围按**线路协议族**声明，而不是按厂商名称。SigV4 加 EventStream、
Gemini 的线路格式配 Vertex 认证，以及 OpenAI 兼容族无法共用同一个请求构造器。

如果 Provider id 不被任何协议族声明，Zuno 会返回点名该 id 的错误，而不是静默尝试
OpenAI 兼容路线。可定位的显式失败正是预期结果。


## 企业预览运行时扩展

预览保留 main 的核心格式 14，另用 `zuno_preview_schema` 记录运行时扩展格式 1，
通道为 `enterprise-preview`。稳定内核的格式号与预览归属／根 Job 存储分别演进。

已发布核心格式 5–13 前向迁移并保留会话、消息、Memory 和提问记录；有效核心格式 14
在同一事务内安装预览扩展。旧的未发布预览曾使用不同的格式 13（归属）和 14（运行时 Job），
迁移会验证对应的确切结构，保留归属、预算、检查点和租约 epoch。

缺失或未来扩展标记、必需结构损坏、未标记的两条历史混合均失败关闭。DDL、回填、结构校验
和版本标记同事务提交。预览安装继续使用独立配置及数据目录，迁移验证只操作隔离 fixture。
