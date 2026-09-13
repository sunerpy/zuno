# 企业 MCP 操作

远程 MCP 调用由网关使用明确的用户凭证执行。Worker 只持有不可变工具声明与操作
回执；命令容器不接收 MCP 凭证或连接会话。

## 配置

Agent 定义可配置最多 64 项 `mcpTools`，每项包含：

- `connection`：网关管理的连接名称。
- `server`、`tool`：经过验证的展示名称与上游工具名。
- `endpoint`：精确 HTTPS 地址，不含用户信息、查询参数或 fragment。
- `revision`：正整数资源／凭证分配版本。
- `definition`：完整审核过的 `tools/list` 声明，包含名称、描述、输入 Schema、
  可选输出 Schema 和 annotations。

声明进入原有配置摘要。修改地址、声明或分配版本需要新的配置。模型收到稳定派生的
工具名和原始参数 Schema，不能通过调用参数选择其他凭证、地址或工具。
只完成模型请求的 completion profile 不允许安装 MCP 工具。

网关配置提供 `mcpConnections` 和 `mcpParallelism`（默认 2，范围 1–16）。
连接字段为 `owner`（租户／主体）、`connection`、`revision`、`endpoint`、
`accessTokenFile`、可选 `rootCertificate`、`timeoutMillis`（100–120000）。
用户／连接组合必须唯一，地址和版本必须与不可变声明完全一致。
Token 文件使用绝对路径，只保存目标服务专用的 bearer token，不能填入 Zuno
API token 或 Worker 服务凭证。

每次建立连接前重新读取 token 文件。部署层负责凭证轮换，也可通过
`McpConnectionProvider` 接入外部凭证组件。本适配器不执行交互 OAuth、不刷新
token、不启动 stdio 服务，也不回退到旧 SSE。Zuno 的 Entra／OIDC 登录和 MCP
目标服务授权是独立边界。

## 审批与执行

所有外部 MCP 调用均需人工审批，包括声明 `readOnlyHint` 的工具。该字段只是
不可信元数据。审批绑定用户、Job、调用、参数、地址、版本和完整工具声明。
新准入验证当前组织授权与 Worker 租约。即将执行 `tools/call` 前，网关复核
原先获准的执行尝试、当前策略、审批有效期与取消状态；父任务释放 Worker 槽位
不撤销已经准入的操作。

`GET /api/v1/approvals/{id}/mcp` 向有权审批者展示完整声明、地址和原始参数。
SDK 的 `EnterpriseClient.mcpReview()` 验证响应身份。公共协议使用
`InvocationSource::Mcp`、`ExecutionLocation::External`、
`UiAction::ViewMcpCall`。本变更不交付 UI。

每项操作建立独立 MCP 会话，拒绝重定向，使用配置的 TLS 信任根，并在执行前检查
上游完整工具声明是否变化。目录最多 256 项／512 KiB，单项声明最多 32 KiB，
参数最多 64 KiB，结果最多 256 KiB。声明变化在 `tools/call` 前拒绝；准备失败
不会自动更换目标或重试外部调用。

## 恢复语义

网关独立的 `mcp.sqlite` 日志格式为 1，使用进程独占锁。PostgreSQL 预览格式 24
在原有运行时事务中记录审批准入、执行尝试、完成结果与等待唤醒；格式 23 的升级
保留原会话、消息、Memory 和编辑记录。网关协议 8 增加实际可执行的 MCP 准备、
提交与查询操作；Docker 日志仍为格式 5。

| 边界 | 处理 |
| --- | --- |
| 排队中，尚未调用外部工具 | 执行前重新验证授权 |
| 结果已落盘，投递响应丢失 | 重发相同回执，不再次调用工具 |
| 进入 `tools/call` 后网关重启 | 保留 `Uncertain`，不自动重放 |
| 响应丢失、不可解析或超过结果上限 | 保留 `Uncertain`，等待权威状态核查 |
| 调用前取消 | 落盘取消记录，阻止迟到提交 |
| 调用中取消 | 保存取消意图，并保留之后收到的真实有界结果 |

MCP 本身没有通用的权威操作查询或幂等保证。取消不能证明已经提交的远程副作用
停止。不确定结果需要核查外部状态；本适配器不注册自动解决命令。

恢复时需要同时保留 PostgreSQL 状态与网关日志。删除日志会丢失本地防重复执行
证据，不能作为恢复方式。企业网关支持 Linux amd64／arm64；个人 MCP、TUI、
ACP 的平台承诺不变。
