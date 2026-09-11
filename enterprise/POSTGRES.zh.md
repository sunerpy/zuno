# PostgreSQL 预览持久化

当前适配器实现 `SessionPersistence`：逻辑工作区、私有会话、游标分页、幂等文本接纳和持久事件。
`AgentApplication` 通过同一个接口使用 SQLite 或 PostgreSQL。PostgreSQL Memory、
远程引擎状态访问和 OAuth2／OIDC HTTP 入口仍在后续实施；这个库不单独注册企业服务器或 Worker。

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
也显式过滤归属。Scope 数据只是归属，不是认证凭证，宿主必须先认证和授权。

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
再处理同一会话的下一轮。成功结算要求输入已经消费；旧租约不能覆盖结果。租约过期
会记录 `uncertain` 并保留该会话的逻辑占用，其他会话仍可运行。在途外部操作的自动
接管还需要网关回执核查，这个适配器不会自行重放副作用。

PostgreSQL 预览格式 2 从已验证的格式 1 原子升级。迁移账号可为
NOSUPERUSER／NOBYPASSRLS。回填在事务和排他 DDL 锁内临时解除 FORCE RLS，
随后恢复 FORCE、验证延迟外键、更新权限，最后写格式标记。测试在 DDL 中途注入
失败，逐项比较原工作区、会话、输入、事件和请求回执，验证回滚和成功升级都保留
数据；不要求重建受支持数据库。

真实 TLS 用例还覆盖双 Worker 领取、独立会话并行、输入 CAS、检查点交接、拒绝旧
Worker、执行不确定性、闲置所有者分页、RLS 和审核写入失败的原子回滚。输入实体化
由测试模拟；远程内核、当前组织授权、子任务完成投递和外部操作恢复仍需接入，
不能据此注册完整企业运行时为可用功能。
