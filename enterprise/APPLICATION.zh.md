# 企业应用 API

`EnterpriseApplication` 在 `AgentApplication`、`JobDispatcher` 和 PostgreSQL
组织存储之上装配会话、Job 和审批处理器。宿主安装逻辑工作区，绑定不可变配置引用及
已校验的 Agent／模型选择。请求不能指定归属、宿主目录、Worker 租约或配置快照。

外部委派用户通过 `/api/v1` 提供本服务的 access token，该入口不接受 Cookie。
浏览器通过 `/app/api/v1` 和 `EnterpriseBrowser::authenticate_routes` 使用
HttpOnly Cookie，写请求必须携带精确 Origin 与 `x-zuno-csrf: 1`。
内部 Worker／网关身份仍然独立；ID token 和 ACP 权限回复不能替代应用授权。

登录成功不会自动加入组织。PostgreSQL 会话操作、输入／Job 接纳及公共 Job／版本读取
在数据事务内重查成员、策略版本、主体类型和调用应用；相关锁保留到提交，防止撤权穿过
权限检查与数据修改之间的间隙。组织初始化仍需显式使用 schema-owner 凭证。

| 相对于两种前缀的方法和路径 | 结果 |
| --- | --- |
| `GET /workspaces` | 已安装工作区 ID 和标题 |
| `POST /sessions` | 幂等创建归属会话 |
| `GET /sessions` | 有界归属分页 |
| `GET /sessions/{session}` | 归属会话摘要 |
| `GET /sessions/{session}/input-version` | 精确 CAS 版本 |
| `POST /sessions/{session}/turns` | 原子接纳输入和 Job |
| `GET /jobs/{job}` | 公共 Job 标识、阶段及输入版本 |
| `GET /approvals/{approval}` | 经过授权的审批展示 |
| `POST /approvals/{approval}/answer` | 幂等人工决定 |
| `POST /workspaces/{workspace}/memory` | 安装真实后端后提供类型化私有 Memory 操作 |

回合请求包含 `requestId`、`expectedInputVersion` 和 `text`。版本使用规范十进制
字符串，避免 JavaScript 精度损失。相同请求可取回原接纳结果；旧版本的新请求或改变
内容的重放均冲突。Agent／模型选择与输入、Job、事件一起提交并参与去重，SQLite 与
PostgreSQL 使用同一契约。省略选择项保留已有会话选择和旧请求摘要。接纳不等于模型
已经应用输入。

分页接受 `limit`（1–100）及完整的 `beforeUpdatedAt`／`beforeSessionId` 游标对。
跨用户资源返回 not-found。未知字段、归属覆盖和无效 ID 在资源修改前拒绝，响应不可缓存。

审批答案包含 `requestId`、`answer`（`approve`／`reject`）。已有原子服务检查当前
角色、发起人／指定审批人范围、过期时间、策略及受信任审批应用。普通 API 客户端和
ACP bridge 不应为了跳过 HITL 而加入审批应用白名单。审批决定不是工具完成或 Worker 凭证。

公共 Job DTO 提供类型化等待目标，客户端可据此发现审批 ID；不包含检查点、租约、凭证、
配置、私有续接块或任意存储结果。完整历史与
临时活动由独立活动协议承载；当前不注册占位的历史、取消或流式路由。

`python3 scripts/check_enterprise_postgres.py` 通过真实 TLS／PostgreSQL 验证
双用户、CAS／重放冲突、配置选择、跨归属拒绝、审批限制和撤权。BFF 用例跨两个副本
访问真实应用接口，包含 CSRF 和退出登录；SQLite 用例验证选择项和改变内容后的拒绝。

独立角色与网关命令装配见[部署](DEPLOYMENT.zh.md)，完整 P3–P6 故障验收仍需完成。预览发布保持关闭，
个人 HTTP／TUI／ACP 平台行为不变。

参见 [English](APPLICATION.md)、[授权](AUTHORIZATION.zh.md)、
[浏览器登录](BROWSER.zh.md)和[进度](STATUS.md)。
