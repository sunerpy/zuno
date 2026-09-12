# 经过鉴权的执行网关

控制面、Worker 传输和 Docker 网关使用独立的类型化协议。网关持有 Docker socket
和自己的回执账本，通过工作负载凭据访问控制面，不接收数据库连接。启动及验收进度见
[实施状态](STATUS.md)。

## 请求与执行权限

1. Worker 携带工作负载 token 和当前 Job grant 请求控制面。
2. 控制面检查数据库租约、当前组织权限，并解析 Job 固定的配置。
3. 短期网关凭证绑定已验证的 Worker、完整租约、指定网关及完整请求摘要。
4. 网关携带自身独立白名单中的工作负载身份核验凭证；控制面再次检查租约和环境分配。
5. 准备命令时创建或读取持久审批，请求凭证不能代替审批。
6. Docker 后端在执行前再次请求控制面检查操作审批；操作 ID、参数与回执继续遵守原有恢复规则。

`GatewayTicketAuthority` 仅存在于控制面，凭证有效期可配置为 1–30 秒，且不超过
已验证的 Worker grant。网关不持有签发密钥。Worker 状态 grant 与网关请求凭证具有
不同的类型和签名用途。

网关服务认证拒绝用户身份及非白名单主体／应用。服务身份决定 gateway ID，请求不能
选择该映射。签名验证后仍需检查租户和环境范围。

## 配置与持久环境

`ConfiguredGateways` 支持最多 128 个已安装的不可变配置引用，映射租户、网关、HTTPS
地址、镜像 digest 和资源边界。Job 的配置 ID、版本和 SHA 必须全部匹配，旧配置不可用
时拒绝，不用当前默认值替代。

环境在独立类型命名空间中使用会话的不透明 ID，Docker volume 名额外绑定所有者。
配置不能携带控制面的宿主目录。现有环境规格发生变化时由网关拒绝；镜像变更需要明确
的环境迁移，不能静默得到空工作区。

命令准备绑定当前环境版本。写操作完成后版本推进，因此下一次操作应重新读取环境；
旧审批或旧资源版本不能授权另一项操作。

## 私有协议

`GatewayRequest` 为版本 2，使用有界的 tagged enum：

| 命令 | 行为 |
| --- | --- |
| `acquire` | 获取数据所有者分配的会话环境 |
| `get` | 读取并验证该环境 |
| `prepare_child_workspace` | 仅准备控制面解析的暂存子任务工作区 |
| `prepare_command` | 解析环境并取得持久审批 |
| `submit_command` | 重新检查租约与审批后提交 |
| `inspect` | 在分配环境内查询原操作回执 |
| `output` | 使用 offset 和前缀摘要读取有界输出 |

它们属于 Worker／网关私有消息，公共 Web 活动使用独立投影。此版本的 Worker HTTP
不开放任意环境销毁、分支目标选择或管理级取消。子工作区准备绑定已接纳关系和回执，
不代替子命令审批。

控制面提供 `/internal/worker/v1/gateway-ticket`、
`/internal/gateway/v1/resolve`、`/prepare`、`/authorize`；执行网关提供
`/internal/execution/v1/request`。宿主装配真实 provider 后才注册这些处理器。
独立可执行文件已安装这些服务；有效子目标配置会增加受限工作区准备路径。

Worker→控制面携带 Worker token 和 Job grant；网关→控制面携带网关自身的工作负载
token；Worker→网关仅通过 `x-zuno-gateway-ticket` 携带绑定请求的短期凭证。
凭证不能进入命令容器或工具结果。

HTTP client 强制 HTTPS、禁止重定向和 POST 自动重试。网关 frame 上限 1 MiB，
单次输出请求最多 64 KiB。空输出页可以返回 offset 0 与空前缀 SHA-256 摘要，该游标
仍可用于继续查询。

查询回执／输出前检查操作属于已分配环境。控制面回调将网关观察到的环境与 Job 固定
配置核对，并继续使用现有组织审批存储。反序列化的租约或环境对象本身不是认证凭据。

## 验证

`python3 scripts/check_enterprise_postgres.py` 验证控制 API、身份、租约与审批，
使用临时 TLS 凭据及受限 PostgreSQL 角色。

`python3 scripts/check_enterprise_docker.py` 验证真实执行路径，并以
`ZUNO_GATEWAY_TEST_REQUIRED=1` 运行 PostgreSQL／HTTPS 测试，缺少 rootless Docker
socket 会使该 gate 失败。可以通过 `ZUNO_ROOTLESS_DOCKER_SOCKET` 指定任务专属守护进程。

测试覆盖审批前／后提交、重复逻辑操作、输出读取和独立审批后的文件验证，还覆盖服务
身份混用、请求改变、其他环境 ID、环境事实改变及租约到期。测试 token 不等于真实
企业 IdP 验收，Linux amd64／arm64 原生 CI 仍是认证前提。

另见 [English](GATEWAY.md)、[rootless 后端](ENVIRONMENTS.zh.md)、
[身份](AUTHENTICATION.zh.md)和[审批接续](WAITING.zh.md)。

`GatewayToolDispatcher` 已通过此传输实现 `environment_command`，审批等待发生在执行前，
操作等待发生在持久交接之后。可执行网关运行回执投递 supervisor；原生 runner 还会启动
控制面、网关和两个独立 Worker 二进制，详见[部署](DEPLOYMENT.zh.md)。

## 数据所有者取消

网关 supervisor 通过独立服务认证轮询 `internal/gateway/v1/cancellations`，领取已停止 Job 的不可变操作接纳记录。该入口在 Worker 撤权后仍有效，只允许停止原操作；完成事实保持原样，不确定状态继续核查。见[控制](CONTROL.zh.md)。

网关协议 3 允许从数据所有者验证过的 Workflow 协调工作区准备子工作区；源工作区必须已经存在，具体子 Job、源与网关绑定保持在签名请求内。普通命令仍要求执行会话一致。
