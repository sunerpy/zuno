# PostgreSQL 预览持久化

当前适配器实现 `SessionPersistence`：逻辑工作区、私有会话、游标分页、幂等文本接纳和持久事件。
`AgentApplication` 通过同一个接口使用 SQLite 或 PostgreSQL。PostgreSQL Job／Memory、
远程引擎状态访问和 Entra HTTP 入口仍在后续实施；这个库不单独注册企业服务器或 Worker。

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
