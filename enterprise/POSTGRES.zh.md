# PostgreSQL 预览持久化

当前适配器提供作用域化会话、运行时 Job、组织审批及共享内核的 `TurnPersistence`。
`AgentApplication` 和普通有界驱动通过相同契约使用 SQLite 或 PostgreSQL，并支持持久等待消费。
Worker 认证传输和[浏览器登录／会话存储](BROWSER.zh.md)已使用此适配器；
PostgreSQL Memory 和完整运行时装配仍在后续实施。
这个库不单独注册企业服务器或 Worker。

## 数据库边界

固定使用 `zuno_enterprise_preview` schema，不使用正式通道数据库或 PostgreSQL `public` schema。

迁移与运行使用不同凭证。运行角色不得拥有 superuser／BYPASSRLS、对象所有者成员资格、
schema CREATE、表 TRUNCATE／TRIGGER 或格式 marker 修改权限。数据库管理员可以先创建角色，
再交互设置密码：

```sql
CREATE ROLE zuno_preview_runtime LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE;
\password zuno_preview_runtime
```

将迁移连接池和运行角色名交给 `zuno_postgres::migrate`。迁移使用事务级 advisory lock，
在一个事务中创建表、约束、策略和权限，最后写入格式 marker。有效迁移可重复执行；
无 marker、未来版本或结构改变时拒绝，不猜测修复或降级。

`PostgresOptions` 强制 `VerifyFull` TLS，URL 中的 `sslmode=disable` 不能覆盖它。私有 CA
需要提供可信根证书。连接 URL 留在控制面的密钥配置中，配置类型不实现 Debug／Serialize。

每个应用事务使用事务局部设置绑定租户和主体，连接复用前清除。数据表启用并强制 RLS，应用查询
也显式过滤归属。Scope 数据只是归属，不是认证凭证，宿主必须先认证。用户事务同时检查
组织成员、主体／应用和策略版本，相关锁保留到提交；资源使用前必须完成组织初始化。
公共处理器见[应用 API](APPLICATION.zh.md)。

创建回执阻止同一幂等键被不同内容复用；输入、接纳事件和调用者归属原子提交。事件游标是会话内
逻辑位置，会话分页同时使用更新时间和 ID。

## 验证

以非 root 开发用户运行，需安装 PostgreSQL 服务端、`pg_config`、OpenSSL、Python 和 Cargo：

```sh
python3 scripts/check_enterprise_postgres.py
```

脚本创建独立 loopback 数据库、临时 CA／服务器证书和受限角色，通过校验证书的 TLS 执行真实测试，
随后停止并删除该测试集群，不访问已有数据库。可用 `ZUNO_POSTGRES_BINDIR` 指定服务端目录。

测试覆盖角色限制、TLS 不可降级、RLS、连接池身份清理、跨用户／租户隔离、相同时间戳分页、
重复请求、审计失败回滚和格式漂移／未来版本拒绝。独立预览 CI 配置 PostgreSQL 16 的 Linux
amd64／arm64 验证，本机还验证 PostgreSQL 18。

SQLx Core 与 PostgreSQL driver 使用精确配对版本。SQLx 总入口的可选 SQLite 依赖与 Zuno
已发布的 `rusqlite` 链接范围冲突，因此适配器不降级或替换本地 SQLite。应用持久化接口不暴露
SQLx 专属类型。

完整能力进度见 [STATUS.md](STATUS.md)。

## 内核状态与执行权

`PostgresBackend::turn_state` 只在状态服务端构造。宿主先认证 Worker，绑定确切租约，
再解析执行环境内可见的目录；不接受公共客户端提交的路径，也不暴露控制面私有路径。
分布式部署中的 Worker 使用状态 API 客户端，不持有此连接池。

每次操作在事务内检查会话归属、当前租约、组织成员和调用应用策略。普通状态操作在提交前
再次校验租约，防止慢写入延长已经过期的执行权；策略和成员共享锁将写入与权限撤销串行化。

消息／part 标识、工具参数及已结算回执在相应提交边界保持不可变。用量复用本地归一化逻辑，
按消息差量累计；并行工具结果按批次原子提交。提示词、模型尝试和重试状态均持久记录。
历史读取先选择已提交的压缩尾部，再解码工具／思考内容，避免读取已经丢弃的旧头部；
精确的 developer 回执继续参与模型续接。

驱动准入与检查点校验复用同一状态机。驱动检查点、RuntimeCheckpoint／版本及 Worker 槽位释放
同事务提交；驱动终态与原生 Job 结算同事务提交。即使成功响应丢失，下次领取也能从 Job
读取检查点，保留步骤、工具次数和用量。无有效检查点的在途任务继续要求核查。

当前输入物化覆盖根任务文本输入。有界驱动也支持[持久调用等待](WAITING.zh.md)，完成事实、
原工具结果和下一检查点同事务消费。远程 steering、附件、人工／子任务 producer 及网关到
工具的装配仍未完成；本地人工请求结果不能冒充远程等待检查点。

预览格式 4 增加消息／parts、重试、父会话／上下文和用量状态。格式 1–3 原子迁移，不重建数据库。
格式 3 fixture 固定原始授权 DDL 和 source digest；注入迁移失败后，组织策略、成员、审计、
Job 预算与租约状态仍保留。

## Runtime Job 与检查点

`PostgresBackend::runtime` 提供租户绑定的 `RuntimeStore`，复用原生 `agent_job`
标识和 root-turn subject。输入、输入 CAS、Job、调度状态及事件在一个事务中接纳。
使用逻辑 ID 和事件游标，不将 SQLite 物理 rowid 作为 PostgreSQL 游标。

领取任务时通过 `FOR UPDATE SKIP LOCKED` 锁定会话，原子提交会话 epoch、Worker
实例和执行尝试。租约携带归属用于路由，但不是身份凭证。每次更新都会在事务内核对
实际所有者、Job／会话、Worker、attempt、epoch、检查点版本和数据库时间期限。
续租不会缩短已经提交的有效期。

控制面账号可以调用固定只读函数 `dispatch_owners`，每次最多返回指定租户的 64 条
所有者／顺序元数据。函数由 schema 所有者执行，不能返回输入、Job 内容、检查点或
任意查询；撤销 PUBLIC 执行权限。私有读取和写入仍使用精确的所有者 RLS。空队列
探测也推进顺序，避免大量闲置所有者阻塞后续任务。Worker 和最终用户不持有数据库
账号。

提交检查点会释放执行容量，同时保留当前逻辑 Job。下一位 Worker 先续接该 Job，
再处理同一会话的下一轮。成功结算要求输入已经消费；旧租约不能覆盖结果。租约过期时，
只有未变化且能够解释全部未完成调用的确切驱动检查点可以重新入队。更新的在途推进或
无法解释的操作仍记录 `uncertain` 并保留该会话的逻辑占用；其他会话可继续运行。
这个适配器不会自行重放外部副作用。

PostgreSQL 预览格式 2 从已验证的格式 1 原子升级。迁移账号可为
NOSUPERUSER／NOBYPASSRLS。回填在事务和排他 DDL 锁内临时解除 FORCE RLS，
随后恢复 FORCE、验证延迟外键、更新权限，最后写格式标记。测试在 DDL 中途注入
失败，逐项比较原工作区、会话、输入、事件和请求回执，验证回滚和成功升级都保留
数据；不要求重建受支持数据库。

真实 TLS 用例还覆盖双 Worker 领取、独立会话并行、输入 CAS、检查点交接、拒绝旧
Worker、执行不确定性、闲置所有者分页、RLS 和审核写入失败的原子回滚。输入实体化
由测试模拟；远程内核、当前组织授权、子任务完成投递和外部操作恢复仍需接入，
不能据此注册完整企业运行时为可用功能。

格式 3 增加[组织授权与审批](AUTHORIZATION.zh.md)，支持从格式 1、2 前向迁移。

格式 5 增加所有者作用域的等待、定时器索引和 `waiting` Job 状态。登记时读取提前到达的
完成事实，发布结果仅唤醒等待中的 Job。消费、原工具结果、检查点／版本及租约释放原子提交。
固定的格式 4 fixture 验证回滚和前向迁移均保留消息、签名 metadata、用量、预算与租约。


## 规范化上下文与输入执行

格式 6 增加所有者作用域的上下文状态与输入执行回执。Tracker revision CAS、epoch／时间
单调性及重复写入复用 SQLite 校验规则。提供商请求将初始助手、请求事件、数据库分配的请求
序号和上下文状态原子提交；助手完成也将上下文快照与 parts、用量一起提交。
共享驱动和 Worker 协议不携带 SQLite 连接。

根任务输入区分收到／写入历史与实际用于提供商请求，在真实派发前标记 applied。
历史 consumed 输入只回填为 recorded，不推断模型已经执行。远程 steering 和任意旧 turn
重分配仍不属于已开放能力。

没有规范化 Tracker 的旧 PostgreSQL 会话保留权威累计用量，上下文占用保持未知，直到当前
请求取得可确认测量；当前 post-hook 请求估算仍保护上下文限额，不把不完整历史当成精确窗口。

固定格式 5 fixture 验证迁移失败／成功都保留等待、Job 预算、租约、消息和用量。
内部 Worker 协议版本为 7，与公共 UI DTO 分离。兼容任务领取、稳定输入时间和有界
续租详见 [Worker 宿主](WORKERS.zh.md)。

## 浏览器认证状态

当前格式 7 增加租户作用域的 `browser_login`、`browser_session` 和
`authentication_audit`。浏览器身份尚未确认时，只有部署固定的租户可查询加密登录事务
或凭据摘要；工厂属于宿主 BFF，公开请求不能选择租户或数据库主体。三张表均强制 RLS，
查询另外保留显式租户条件。

登录事务通过数据库时间下的一次 `DELETE ... RETURNING` 原子消费，并在 token POST
之前提交；错误绑定不消费其他浏览器的事务。事务级锁保护配置的容量上限，过期记录
按有界批次清理。会话创建／撤销与审计同时提交或回滚，存储记录还需与索引列的凭据、
主体和到期时间一致。

格式 1–6 均原子前向迁移。捕获的格式 6 fixture 在失败和成功迁移前后保留会话、
message／part、等待 Job 和输入回执。BFF 与 Worker 的网络验证使用同一临时 TLS 集群
中的独立数据库。完整接口见[浏览器认证](BROWSER.zh.md)。

## 操作准入与完成

格式 8 增加 `gateway_operation` 与 `gateway_operation_attempt`。网关执行授权在当前
审批／租约验证的同一事务中记录逻辑操作和准入 attempt。完成回执核对经过认证的网关
及这些不可变记录，允许真实结果在原租约失效后到达；回执、完成事件和对应等待就绪原子
提交，改变内容或未经准入的事实被拒绝。

精确格式 7 fixture 在 DDL 故障回滚和成功迁移时保留会话、message／part、Job、等待
及有效浏览器会话。网关确认、输出上限与独立父消费边界见[操作结果投递](OPERATION_RESULTS.zh.md)。

## 私有 Memory

当前格式 9 增加所有者作用域的 Memory 策略、会话覆盖、文档、候选、版本、证据、
来源归属、淘汰记录、学习 Job、维护水位、请求回执和审计，所有新表强制所有者 RLS。
精确格式 8 fixture 在写入新标记前验证故障回滚及既有运行时、浏览器与操作数据保留。

数据所有者在有界阻塞容量中执行共享 Memory 服务，每个请求共用一个事务。主体／工作区
验证、候选 CAS、来源重验、授权、学习租约与结果／审计保持同一边界。Worker 使用内部
协议 11，不接收数据库 provider 或 pool。已实现能力和待实现生产者见 [Memory](MEMORY.zh.md)。

## 子任务执行会话关联

格式 10 将原生 Job 的父会话与 Worker 实际执行的会话分开。`runtime_job` 独立引用
逻辑 `agent_job`，输入仍绑定真实执行会话；`runtime_session` 引用精确的 Job／执行
会话组合。已有根任务行保持原标识和值。

精确格式 9 fixture 除运行时历史外，还包含真实私有 Memory 和策略值。迁移失败恢复
原约束和格式标记，成功保留全部行；存储回归验证子任务占用子会话执行槽位，并保留
委派父会话关系。原子派发、前台持久等待及完成投递见[子任务](CHILDREN.zh.md)。

## 子工作区准入

格式 11 增加 `child_workspace_preparation`、工作区策略／就绪状态与继承深度上限。
工作区未准备时，父检查点不能激活子任务。只有已分配网关可以提交匹配的不可变准备回执；
真实迟到回执仍保存，但不恢复旧父租约。

精确格式 10 fixture 比较旧列并单独验证新默认值：根会话保留配置上限，旧子会话不推断
额外委派权。迁移在回填后恢复强制 RLS，再更新格式标记。详见[工作区](WORKSPACES.zh.md)。

## 格式 12：持久取消

格式 12 增加 `runtime_control_request`、`runtime_stop`、`runtime_continuation` 和 `gateway_cancellation_delivery`。停止意图、子树执行权撤销、逻辑完成及网关队列一起提交；受限目录函数只返回分配给网关的操作坐标，正文仍经过所有者 RLS。固定格式 11 夹具验证迁移成功及注入失败回滚时会话、消息、Job、Memory、子任务和操作接纳记录不丢失。见[控制](CONTROL.zh.md)。

## 格式 13：公共活动

格式 13 增加所有者作用域的公共计数、item、不可变 frame，以及消息到原始执行 Job 的稳定关联。投影、源写入及逻辑游标一起提交。固定格式 12 夹具验证迁移成功及 DDL 失败回滚时消息、part、Memory 与取消投递状态均被保留。快照分页与隐私边界见[活动协议](ACTIVITY.zh.md)。

## 格式 14：临时实时进度

格式 14 增加 `live_progress` 与执行状态变化时的原子清理触发器。验证原始消息／Job、租约、摘要和序号，仅显示当前有效租约下、来源消息仍未完成且未过期的快照。固定格式 13 迁移保留持久活动和 Memory 数据。

## 格式 15：持久 Workflow 协调

`runtime_workflow`、`runtime_workflow_node` 保存固定 DAG、原生 Job 关联、逻辑节点容量、有序结果及精确依赖输入。Agent Worker 不领取协调 Job；节点接纳在当前授权与短事务锁下完成。格式 14 fixture 验证会话、消息、Memory、持久 frame 和临时行保留，以及 marker 前故障回滚。详见 [Workflow](WORKFLOW.zh.md)。

## 格式 16：Council 协调和执行期限

作用域化 `runtime_council`、`runtime_council_seat` 和 `runtime_council_attempt`
保存固定策略执行、答案摘要、修正来源、quorum 和综合期限。普通 Job 的
`runtime_job.deadline_at` 为空；有期限的 Council Job 在领取和续租时截断租约。
固定格式 15 fixture 的来源摘要为
`412f668e781e311d6ddf76fd51a1314ace1b6423f1b33c67ceaac053ce40661c`。
迁移测试保留代表性 Job、消息、Memory、frame 和 Workflow／节点，检查 DDL
失败回滚、新 RLS 表及 marker。比较历史 Job 行时只排除新增的可空列，旧格式来源摘要
和结构 manifest 仍需通过校验。详见 [Council 执行](WORKFLOW.zh.md#持久-council)。

## 格式 17：工作区审批合并

`gateway_merge_operation`、`gateway_merge_attempt` 和
`gateway_merge_cancellation` 保存不可变提案、执行接纳身份、回执和取消投递。
批准检查与执行接纳同事务提交，命令与合并操作 ID 共用作用域化 advisory lock，
完成结果与原等待的唤醒原子提交。固定格式 16 fixture 的来源摘要为
`b539bf863f4192d81cc8189ec65f2900b4fd589e7e8e00e34d1ff2e77d1464f1`，
验证会话、消息、Memory、活动、Job 和 Council 期限在迁移成功及 DDL 失败回滚中保留。
新表全部使用所有者 RLS，取消目录函数只返回有界坐标。

## 格式 18：初始工作区导入

`workspace_import` 将空根会话绑定到唯一活动归档请求、固定部署和持久初始化回执。
会话锁与首轮输入仲裁，未完成或不匹配的初始化会拒绝接纳，不消费输入。部分唯一索引
只允许在旧上传取消后创建新上传。固定格式 17 fixture 来源摘要为
`0283f316583aeff2c9da642747f54430c9fd1035a5183355d9a2b86deb3aeebc`，
检查会话、消息、Memory、Job、合并提案在迁移成功及 marker 前失败回滚中保留。
