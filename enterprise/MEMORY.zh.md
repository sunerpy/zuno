# 企业私有 Memory

控制面拥有 PostgreSQL Memory，Worker 通过已认证的状态 API 访问。共享 `MemoryService`
继续负责验证、字符上限、候选审核、应用、撤销及渲染；`MemoryPersistence` 和
`MemoryAuthority` 一起注入。个人模式保留 SQLite 和本地文件投影，企业适配器使用
逻辑文档标识，不导入或写入宿主 Memory 文件。

## 归属与事务

文档、候选、版本、证据、维护水位、学习 Job、请求回执和审计均包含租户与用户作用域。
`global` 指当前用户跨工作区的 Memory，并非组织共享 Memory；
`project:<workspaceId>` 指该用户在指定工作区的 Memory。模型和 API 请求不能指定
其他所有者或文档路径。

公共应用与 Worker 路由共享同一个 `PostgresMemoryBackend`。服务先取得有界容量，
再开启数据库事务；当前组织／应用授权、工作区归属、强制 RLS 和用户级事务锁共同保护
请求。Worker 请求还在执行前与提交前按数据库时间核验 Job 租约。

同步的共享 Memory 服务在数据所有者的有界阻塞任务中运行；一次请求中的所有存储调用
使用同一 PostgreSQL 事务。候选、文档、版本、证据归属、去重回执和审计同时提交或回滚。
事务内不等待模型或外部网络请求。调用方断线后仍保留容量直到事务结算；独立期限限制
处理时间，避免无限占用连接或阻塞线程。
取消后会等待明确的回滚确认再归还容量；回滚清理另有 12 秒上限。无法确认清理结果时
返回存储失败，不能报告 Memory 操作成功。

## 授权、读取与遗忘

企业默认允许使用已有私有 Memory，默认关闭自动生成。只有已认证用户通过组织配置的
受信任审批应用才能修改策略，或进行手动应用、编辑、拒绝、撤销、遗忘。普通已允许
API 应用可以读取和提交候选，但不能自行授权或绕过审核。带版本的会话覆盖项只能进一步
收窄所有者策略。Worker 不能自行授权、审批候选、修改
策略、导入文件或遗忘其他来源。

用户手动修改先生成待审候选，再明确应用。前台 `memory_update` 必须具有当前私有
生成授权，并通过相同版本和字符上限验证。该授权仅覆盖私有数据维护，不允许执行命令、
修改工作区、安装 Skill 或写入组织共享 Memory。共享 Memory 及其组织审批流程尚未注册。

`useMemories: false` 从后续模型请求移除 Memory；关闭 `generatePrivate` 阻止新模型
修改并跳过已排队的 Memory 学习工作，不等于删除已有记录。在途维护提交时仍需核验当前
授权和精确学习租约。

证据可以引用已接纳的用户输入或执行网关确认成功的操作。服务检查归属、工作区、原始
内容与摘要，不接受客户端声称“已验证”。召回会重新核验来源存在性、摘要和遗忘状态；
关闭未来生成不撤销已有证据的读取资格。仍有独立有效来源的条目继续可用，没有任何有效
来源的派生条目立即停止召回，不等待维护完成。

明确遗忘在一个事务中标记来源、拒绝相关待审候选并撤回失去支持的派生条目，不会逆转
替换来复活旧事实。用户重新确认的内容归用户所有；自动维护不能覆盖用户条目，也不能
复活明确淘汰的内容。维护批次保留候选、证据、文档版本、学习 Job 结算和水位的原子边界。

## 应用与 Worker API

安装真实 Memory 后端后，两个公共应用前缀均提供
`POST /workspaces/{workspace}/memory`。浏览器继续使用同源 BFF、Origin 与 CSRF 检查；
外部调用方使用委派用户 API access token。请求示例：

```json
{
  "requestId": "client-generated-stable-id",
  "command": {
    "kind": "propose",
    "change": {
      "scope": "project",
      "action": "add",
      "content": "提交前运行 cargo test。",
      "oldText": null,
      "reason": "仓库验证规则",
      "expectedRevision": null,
      "confidence": 1.0
    }
  }
}
```

命令为类型化的 `read`、`read_entries`、`candidates`、`candidate`、`propose`、`apply`、`reject`、
`edit`、`undo`、`policy`、`set_policy`、`record_evidence` 和 `forget`。未知字段拒绝，
请求上限 64 KiB。候选查询最多返回 512 条；条目查询支持范围、文字和 1–128 的条数限制，
共用 32 KiB 输出预算。

`MemoryResponse.result` 为携带类型化结果的 `Ok`，或携带 `denied`、`conflict`、
`unavailable`、`invalid_data`、可修正 `invalid` 的 `Err`。HTTP 鉴权和信封格式错误仍是传输错误。
相同主体和参数重用修改请求 ID 时返回已提交回执；参数改变则冲突。读取不使用旧回执，
重试与新请求均检查当前授权。

候选详情与修改结果返回不透明的 `stateDigest`。手动 `apply`、`edit`、`reject`、`undo`
须携带 `candidateId` 和用户实际审阅版本的 `expectedState`。事务拒绝审核后被修改的
候选；客户端应重新展示变更，不能自动读取新摘要后重试审批。

内部 Worker 协议 11 增加 `/internal/worker/v1/memory`，同时验证工作负载身份和当前
Job grant，仅开放作用域内读取和前台提案。`memory_read`、`memory_update` 复用个人
工具参数 schema。无法确认修改结果时保留不确定状态，不机械重放。

每次模型请求之前，包括其他 Worker 接管检查点之后，都会通过异步服务重新读取 Memory。
当前空结果会覆盖检查点中的旧内容。权限撤销或状态服务不可用保留类型化失败，不使用旧
快照继续请求模型；最终组装提示词及摘要仍由共享引擎持久记录。

## 配置、迁移与验证

控制面 `memory` 配置包含 `concurrentTransactions`（1–32，默认 4）、
`globalCharacters`（默认 2200）、`projectCharacters`（默认 3000）、
`transactionTimeoutMillis`（1000–30000，默认 10000）。字符上限范围为 1–131072。
省略整个配置块时使用默认值；用户生成授权保存在持久策略中，不由这些运维默认值开启。

PostgreSQL 预览格式 9 新增 Memory 表和强制 RLS。格式 1–8 原子前向迁移，最后写格式
标记。精确格式 8 fixture 验证故障回滚及成功升级前后，会话、消息、Job、等待、浏览器
记录和操作准入均得到保留；不会把个人 SQLite 数据库自动导入预览命名空间。

`scripts/check_enterprise_postgres.py` 覆盖隔离、CAS、去重、回滚、授权、租约、
证据重验和维护结算；`scripts/check_enterprise_docker.py` 还运行真实控制面、网关及
两个独立 Worker，验证私有 Memory 读写、提示刷新、命令审批等待期间撤销授权、
检查点接续与隔离执行。

后台提取调度、组织共享 Memory、学习管理 UI 和完整 P6 故障验收仍待后续实现。维护
存储契约已实现，不表示企业自动提取生产者已开放；Skill 评测及应用仍需独立审核。

参见 [English](MEMORY.md)、[PostgreSQL](POSTGRES.zh.md)、
[应用 API](APPLICATION.zh.md)及[部署](DEPLOYMENT.zh.md)。
