# ADK 参考设计与主线同步决策

本轮先将企业预览基线从 v0.10.31 同步到 v0.10.37，再以 ADK 的理念审视剩余架构。
Zuno 保持 Rust 共享内核，ADK 是设计参考，不是配置、数据库或协议兼容目标。
App/UI 仍暂停；后续 App 必须先在 Penpot 设计。

## 核验基线

- Zuno 稳定主线：v0.10.37，`4762790adaab0a9af8c93152c1b6136446e4dbc8`。
- ADK Go：v2.4.0，`4e57df40cbd35f055ba654b03ffb0e509fdd754a`，模块 `google.golang.org/adk/v2`。
- 文档通过 Zuno 已配置的 `adk-docs-mcp` 获取，先读取 sources 和 `https://adk.dev/llms.txt`，再读取运行时、恢复、状态、Memory、事件、Artifacts、压缩和 HITL 页面。
- 文档摘要校验值记录于 `adk-reference.json`；源代码留在任务验证目录，不复制为项目依赖。
- 文档的语言支持标记与 Go 源码并不总同步：Resume 指南目前标注 Python/Kotlin，Go 2.4.0 已有基于事件重建的 Workflow HITL 恢复；压缩页未列 Go，但 Go 源码已有独立 compaction 包。判断 Go 行为以固定源码和测试为准。

## 采用／适配／拒绝

| 参考 | 决策 | Zuno 的实施方式与验收 |
| --- | --- | --- |
| `runner.Config` 注入 Session、Memory、Artifact 服务；Agent 与 Runner 分离 | 采用 | 保留 `HarnessProfile`／`RuntimeBackendBundle` 装配和 `AgentDriver::advance`；客户端仅使用 `AgentApplication`，不新增客户端私有循环 |
| `session.Service` 的共享服务一致性测试 | 采用并扩展 | 将 SQLite／PostgreSQL／状态 API 的接纳、版本 CAS、消费、完成和压缩记录纳入同一行为契约；仅声明 trait 不算完成 |
| Runner 先持久化非 partial 事件再交给客户端 | 采用 | 继续保持 `CommittedFrame` 与 `LiveFrame`；事件、状态、Outbox 同事务提交后发布，临时流不进入权威历史和计量 |
| App/User/Session/Temporary 状态作用域 | 适配 | 使用类型化所有权和 `PrincipalScope`，保留租户／用户／会话／Job；不把 `app:`、`user:` 字符串前缀当鉴权边界，临时状态不承载审批或预算 |
| Invocation、Agent call、Step 分层及 Branch／IsolationScope | 适配 | 现有 Session/Turn/Job/ExecutionAttempt/ProviderAttempt/Invocation 保持明确含义；分支可见性与组织授权分开，不因同租户就读取同伴历史 |
| `workflow.ReconstructRunState` 以 InvocationID、节点、InterruptID 重建等待 | 适配 | 使用已有持久等待、检查点和完成消费；去重按稳定 ID，禁止空范围扫描全部历史后猜当前任务；恢复与新输入必须分开 |
| `NodeConfig.RerunOnResume` 的重入或交接模式 | 有条件采用 | 只允许明确只读／幂等节点重入。普通命令、编辑、MCP 等先查询原 Operation 回执；不确定副作用不得重跑 |
| `artifact.Service` 的命名、用户／会话作用域与版本 | 采用并加强 | 后续建立受权产物索引和可替换对象存储，使用内容摘要、稳定版本、流式读取、字节限额及保留租约；元数据提交与对象上传采用 staging/commit 协议 |
| `skill.Source` 的元数据、正文、资源独立读取 | 采用 | 下一阶段用统一来源接口替换仅嵌入正文的局限；资源按相对逻辑路径和版本摘要读取，禁止宿主目录泄露，包安装／执行继续经过审批与网关 |
| `memory.Service` 的会话摄取和用户检索 | 适配 | 保留独立提炼／维护模型、来源证据、撤销和共享审批。摄取不是自动许可；共享 Memory 不借用私人生成授权 |
| 插件 Before/After/OnEvent 生命周期 | 采用 | 继续使用 Component 与类型化 hook；记录实际 post-hook 模型请求，禁止 hook 静默删除压缩／审批／完成等控制事实，卸载必须精确移除注册 |
| 追加压缩事件、保留原历史；滚动摘要与原始尾部 | 采用并加强 | 压缩只影响模型投影，不删除事件。持久记下稳定事件游标覆盖区间和原始正文摘要，不以时间戳作为唯一定位；保留具体事实、未完成调用、授权与预算，测试长会话恢复及压缩失败后的已完成答案 |
| `authn.Authenticator` 与 `authz.Authorizer` 分离 | 采用并加强 | 保留通用 OAuth2/OIDC 与 Entra 适配器；严格匹配主体仅是第一层，继续检查调用应用、资源版本、组织策略、HITL 及 Worker 执行权 |
| 数据库 timestamp stale-check、GORM AutoMigrate | 不直接采用 | 使用独立版本、CAS、数据库时间租约和 epoch fencing；数据库格式必须经过精确旧样本、事务回滚和 marker-last 迁移，不用自动建表代替已发布格式迁移 |
| 内存 Session 先 append，再提交数据库 | 不直接采用 | 存储失败不能留下看似已成功的内存状态；先提交事实，再更新投影／通知。故障注入必须检查二者一致 |

## 对剩余计划的调整

1. **先完成同步门禁。** 本同步保留稳定 SQLite 核心格式 15 与预览 overlay 1；将主线输入门、批次回执和更丰富的重试诊断适配到远程持久化边界。Worker 协议 14 明确返回输入是否真正消费。通过新旧 SQLite 迁移、原生 Goal/Plan/输入/ACP 回归和企业五角色链路后合入预览。
2. **先建立统一契约测试，再扩充后端。** 提取同一业务操作的后端一致性测试，补充提交失败、重复 requestId、旧 epoch、乱序回执、撤权及重启。公开 API 与 Worker 协议分别验证。
3. **完成 Skill 资源与产物服务。** 元数据、正文、资源通过同一可替换来源接口；对象存储与工作区文件操作分开，版本／摘要／授权贯穿下载、安装、回退与保留。
4. **共享自动维护只产生可审阅变更。** 模型维护沿用现有 Job／预算／事件底座，输入限于明确共享且仍有效的来源；提交正文仍需独立组织审核。新状态不能重新引入 synthetic user 或第二套任务系统。
5. **按外部事实验收恢复。** 后续备份恢复、滚动升级、存储保留及累计执行资源限制，必须测试数据库快照落后于已发生外部操作时的核查，不能因为内部事件回放成功就认为可重放副作用。
6. **持续同步规则。** 每个阶段开始记录最新 origin/main 和预览 SHA。未发布功能分支更新到最新预览；稳定主线变化通过同步 PR 合入，已合并／已发布历史不重写。正式发布仍需另行评审。

下一可用预览核心版本基线变为 **0.10.38-preview.1**，发布依然关闭；该版本号只是纳入 v0.10.37 后的规则计算，不代表当前功能已经全部交付。

## 主要源码依据

ADK Go 固定提交中的：`runner/runner.go`、`runner/run_node.go`、`agent/context.go`、
`session/service.go`、`session/database/service.go`、`session/sessiontestsuite/service_suite.go`、
`memory/service.go`、`artifact/service.go`、`workflow/persistence.go`、`workflow/config.go`、
`plugin/plugin.go`、`session/compaction/compaction.go`、`tool/skilltoolset/skill/source.go`、
`server/authn/authn.go`、`server/authz/strict.go`。
