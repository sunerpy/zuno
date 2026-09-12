# 企业可执行服务部署

开发版 `zuno-enterprise` 已有控制面、Worker、网关、迁移和身份检查的真实处理器，
支持 Linux amd64／arm64。其余企业验收完成前，预览发布保持关闭。个人 `zuno` 的安装、
配置和平台范围不变。

用 `cargo build -p zuno-enterprise` 构建，通过显式配置启动各角色：

```sh
target/debug/zuno-enterprise --config /etc/zuno-enterprise-preview/worker.json
```

JSON 包含 `stateDirectory` 和带 `kind` 的 `service`，不读取 Zuno 个人／项目配置和凭证存储。
未指定凭证绑定时，原生云提供商可以使用其标准工作负载凭证链。
路径必须绝对化；新状态目录以私有权限创建，已有共享目录会被拒绝。每个实例使用独立目录，
日志放在其中；网关 ledger 只允许一个实例持有。

## 定义与角色

[definition.json](examples/definition.json)、[worker.json](examples/worker.json) 和
[gateway.json](examples/gateway.json) 是经过类型校验的模板。使用前替换示例端点、
模型、证书及密钥引用。Alpine 测试镜像用于验证 argv 执行，实际任务需要有相应工具的
已批准镜像；初始工作区为空。

控制面和兼容 Worker 保持相同的不可变定义。ID、版本及规范化内容摘要绑定 Agent、
模型、预算和环境；修改定义时递增版本。物理凭证文件不进入摘要，Worker 重启读取轮换
后的模型凭证不会重置持久预算。升级期间保留尚有等待任务引用的旧定义。

Worker 通过 `HarnessProfile` 装配共享 `AgentDriver`，复用原生模型提供商并有界推进。
模型配置明确上下文和输出上限；token／时间／工具预算使用接续后的累计值，无法计量用量
时停止继续消费。原生云传输使用平台信任库，兼容传输另支持配置模型 CA 文件。

首个网关工具为 `environment_command`，接收 argv 数组，工作目录为分配的 `/workspace`。
Shell 语法需要显式调用 shell；它不替换个人 shell 的完整语义。准备阶段可以等待人工审批，
真正提交发生在持久交接之后，随后转为操作等待并释放 Worker。响应丢失时查询原操作，
不重复提交；无法确认的结果保留核查义务。

网关持有 rootless Docker socket 和回执 ledger，supervisor 以有界退避投递持久结果，
重启后从 ledger 重建工作。执行容器不持有运行数据库凭证、全局模型密钥或 Docker socket。
前置条件见[执行环境](ENVIRONMENTS.zh.md)。

## 控制面配置

`service.kind: "control_plane"` 包含：

| 字段 | 含义 |
| --- | --- |
| `tenantId` | 固定组织命名空间 |
| `tls` | 监听地址、PEM 证书／私钥路径及连接上限 |
| `database` | `urlFile`、可选 `rootCertificate`、`maxConnections` |
| `userIdentity`／`serviceIdentity` | 独立的用户和工作负载验证策略 |
| `workers` | 验证后的 `tenantId`、`principalId`、`clientId` |
| `gateways` | `subject` 及分配的 `gatewayId` |
| `jobKeys`／`gatewayKeys` | 独立的 `active` ID 和 `keys: [{id,path}]` |
| `definitions` | 所有保留的不可变定义文件 |
| `activeDefinitions` | 新会话明确选择的 `{id,version}` |
| `leaseMillis` | 数据库租期，1000–300000，默认 30000 |
| `browser` | 可选 OIDC BFF 配置 |
| `webAssetsDirectory` | 可选 Web 资源包绝对目录，必须同时配置 `browser` |

签名文件保存原始密钥字节，由 authority 校验长度和轮换集合；密钥值不能放入定义或请求 DTO。

身份配置的 `kind` 支持 `jwt`、`entra`、`introspection`。JWT 的 `config` 使用
[通用 access-token 策略](AUTHENTICATION.zh.md)，另可设置 `rootCertificate`。
Entra 使用对应的 `config`。Introspection 使用 `config`、`clientId` 和
`clientSecretFile`，通过已有认证 introspection 适配器及平台信任库访问。
主体类型声明、应用和 scope 必须符合实际 IdP，不能假设它们是通用 OAuth2 声明。

可选 browser 配置包含 `authority`、`clientId`、`clientSecretFile`、`redirectUri`、
`scopes`、可选 `rootCertificate` 及 `encryptionKeys`，通过 OIDC code／PKCE 和同一
用户 verifier 登录。详见 [BFF](BROWSER.zh.md)。
实验性 [Web 工作台](WEB.zh.md) 在另行提供资源包后可挂载到 `/app/`。App 设计和 UI
交付等待后续 Penpot 设计阶段，当前预览压缩包仅包含后端。客户端工作流继续检查 Rust
Schema 和 SDK；客户端与 Docker 工作流中的实验浏览器检查需要显式设置
`include-experimental-web: true`。默认 Docker 检查覆盖 Linux 两种架构的后端。
部署静态资源不会启用其他能力，也不会放宽认证。

## 初始化与身份检查

身份检查配置使用 `service.kind: "identity"`、`verifier`、`accessTokenFile`，验证后
输出不含 token 的身份坐标。服务白名单和成员使用这些坐标，不使用邮箱或显示名作为主键。

迁移配置使用 `service.kind: "migrate"`、`database`、`runtimeRole` 和可选 `bootstrap`。
迁移 URL 文件使用 schema-owner 凭证，与运行 URL 分离。Bootstrap 包含 `administrator`
（`PrincipalKey`）和组织 `policy`，只创建一次，不覆盖之后的撤权。数据库角色准备见
[PostgreSQL](POSTGRES.zh.md)。

服务 token 每次请求从私有文件读取，便于身份 sidecar 轮换；Worker 只接收状态 API token
和模型绑定。控制面提供[公共应用 API](APPLICATION.zh.md)，内部协议及执行凭证保持独立。

## 停止、升级及证据

SIGTERM 停止新接纳并排空有界工作，TLS 连接与回执投递的退出也有期限。停止网关不表示
外部命令已经完成，ledger 与容器仍按独立生命周期恢复。

Worker 协议 11 承载检查点 schema 4。旧 schema 3 只按未提交等待读取，不能被解释成已经
交给执行器。切换控制协议前排空不兼容 Worker，并保留定义和持久数据；完整滚动升级、
备份恢复验收仍属 P6。

`python3 scripts/check_enterprise_docker.py` 会启动真实可执行文件：一个控制面、
一个网关和两个独立 Worker。真实 TLS RSA issuer、原生兼容模型传输、PostgreSQL 和
rootless Docker 验证双用户、私有 Memory 读写及提示刷新、审批等待期间撤销 Memory 使用、
父／子工作区分支、Workflow／Council／合并执行和明确审批、两个 Worker 参与、每命令一次操作及 SIGTERM
退出。这是测试提供商证据，不是实际 Entra 租户验证。

工作区初始导入、后台／共享 Memory、剩余
Web／ACP 功能及完整故障矩阵仍需继续完成。构建此二进制不会启用预览发布。

参见 [English](DEPLOYMENT.md)、[平台](PLATFORMS.zh.md)及[进度](STATUS.md)。

控制面可选 `memory` 配置事务并发数、事务期限及字符预算；省略使用默认值。
这不会开启用户生成授权，完整字段和行为见 [Memory](MEMORY.zh.md)。

定义可选 `delegation` 固定子定义 `{id,version,sha256}`，并设置 `maximumDepth`、
`maximumChildren`。使用 `zuno-enterprise --definition-ref /absolute/child.json`
计算引用，将父子定义都安装到控制面和 Worker。引用改变时更新父版本，合法目录会安装
原生 `task`；工作区准备在子执行前完成。完整流程见[工作区](WORKSPACES.zh.md)。

Worker 的 `liveMillis` 默认 500 毫秒，允许 100–5000，设为 null 关闭。实时进度是有界、可替换的快照，不阻塞模型执行，不替代持久历史。见[活动协议](ACTIVITY.zh.md)。

可选 `workflows` 在已有子任务目录上安装有界模板，使用不运行模型的协调 Job、独立节点工作区及严格命令审批。配置与限制见 [Workflow](WORKFLOW.zh.md)。

Agent 的 `mode` 默认 `agent`，保留现有定义的规范化摘要。`mode: "completion"`
复用有界模型驱动，但不提供工具或驻留 Memory 上下文；该配置省略 `environment`、
`delegation`、`workflows`、`councils`。控制面不为其分配网关，并从 Worker Memory 授权中排除
该配置。它可作为自有根会话或显式配置的纯模型子任务运行，是内部完成请求的后端原语，
不代表分布式 Council 编排已完成。

可选 `councils` 安装原生 `council_run`，提供持久席位以及模型专用修正／综合。模型绑定、quorum、容量和期限由不可变定义控制，见 [Council 配置](WORKFLOW.zh.md#持久-council)。

网关 `mergeParallelism` 默认 2，允许 1–16 个后台合并任务；退出时有界排空，日志和回执可在重启后恢复。控制面的 `gatewayRootCertificate` 可为配置网关的审批内容下载设置私有 CA，省略时使用系统信任库。查看变更不需要活跃 Worker 租约，客户端也不会收到 Worker 凭证。

归档验证使用 `scripts/enterprise_artifact_smoke.py --archive <归档> --target
<本机Linux目标> --version <版本> --source-sha <SHA> --output <proof.json>`，
启动隔离 PostgreSQL／rootless Docker，并让已有原生角色 fixture 运行解包后的二进制。
fixture 默认采用 `release` profile，本地驱动检查可用 `--profile dev`。这不会发布
或安装二进制；库与 HTTP 契约仍由其他必需 gate 检查。
