# 企业持久 Workflow

企业定义可安装命名 Workflow 模板。模型选择模板并提供任务，定义固定节点标识、依赖、
子 Agent／模型引用和逻辑并发。继续使用原生 `WorkflowTool`／`WorkflowHost`；
前台调用返回类型化等待，直到能够持久消费原始调用结果。

## 定义

在父[定义](DEPLOYMENT.zh.md)中增加 `workflows`：

```json
{
  "workflows": [{
    "name": "inspection",
    "sourceId": "organization:inspection:v1",
    "maxParallel": 2,
    "maxAgents": 3,
    "nodes": [
      {"id": "scan", "agent": "helper", "prompt": "收集证据。", "description": null, "dependsOn": []},
      {"id": "review", "agent": "helper", "prompt": "检查约束。", "description": null, "dependsOn": []},
      {"id": "conclude", "agent": "helper", "prompt": "综合结论。", "description": null, "dependsOn": ["scan", "review"]}
    ]
  }]
}
```

节点 Agent 必须存在于 `delegation.targets`。当前实现需要至少两级委派深度，分别用于
协调 Job 和 Agent 节点；`maxAgents` 不得超出 `delegation.maximumChildren`，
最多 64 节点、每定义 32 模板。配置阶段拒绝环、重复标识、缺失依赖、未知 Agent 和扩大的
容量。图变化会改变父定义摘要，需使用新的定义版本。

## 归属、执行与恢复

Workflow 使用原生子 Job 和内部协调会话。协调 Job 不会被 Agent Worker 领取，也不
发送模型请求。控制面只在短事务内检查依赖结果、派发符合条件的原生子 Job；节点继续
复用 Worker 租约、有界驱动、审批及操作网关。等待释放 Worker 执行名额，同时保留逻辑
节点名额。一条分支完成后立即给可运行的后继补位，不等待无关慢分支。

原始父任务暂存固定计划，分叉协调工作区并准备各节点独立工作区。该有界准备过程仍
占用原始 Worker 名额，但不运行节点模型或工具；当前 Docker 适配器要求同一指定网关。
准备完成后，前台激活与原父任务的等待检查点原子提交。后台 Workflow 通过准备提交激活，
保留 `nextStep`／`quiet` 行为。

节点从协调工作区分支启动，写入留在各自分支；依赖排序不会自动合并文件。补丁／提交
的审批合并仍是计划中的独立能力。

只有成功完成的依赖才能给后继提供输入。有界结果文本、稳定标识和完成摘要与实际接纳
的节点输入一起记录，明确作为数据，不扩大指令权限。本地与企业宿主共用依赖输入渲染和
DAG 决策；子结果不包含提供商私有推理。

Worker 更换不改变节点身份或输入。Workflow 完成、原生 Job 结果和父完成信封原子提交，
原调用结果通过已有完成标识与检查点事务去重消费。不确定子结果也会保存并消费一次，
随后共享驱动要求核查，不能继续工具或模型调用。失败／取消使用同一持久任务树取消与
网关停止机制处理其他节点。

协调会话不进入会话列表，不接受用户输入或任意 `task_id` 续接。每次派发节点继续检查
当前组织授权，固定定义不能恢复已撤销权限。

## 客户端与协议

已有 API／BFF 前缀下的 `GET /jobs/{job}/workflow` 返回 `WorkflowRunView`：
运行标识和状态、稳定顺序的类型化节点、依赖 ID、Job ID 及公共等待目标。不包含私有
计划、提示词、配置快照、租约或凭证；读取重查当前账号归属。
`UiAction::ViewWorkflow` 来自数据所有者记录的原消息 Job／调用关系，不从工具名称推断。

类型化视图供后续 App 使用。App 设计与 UI 交付按用户补充留到 Penpot 设计阶段；
实验性 UI 不属于当前后端交付。审批资格和精确操作绑定仍由服务端决定。

PostgreSQL 预览格式 15 增加 Workflow／节点协调和固定依赖输入。精确格式 14 fixture
验证会话、消息、Memory、持久 frame 和临时行在迁移及回滚中保留；更旧的受支持格式
仍在同一受保护事务内前进。Worker 协议 9 承载 Workflow 命令，检查点 schema 4 保持不变，
网关协议 3 校验 Workflow 工作区准备。控制面与 Worker 使用匹配版本，公共 SDK 和 Web 包一起生成。

## 验收范围

数据库用例覆盖激活回滚、并发领取、等待和补位、依赖输入、Worker 接管、终态消费、
失败、不确定性、取消与归属边界。原生可执行用例启动控制面、网关及两个 Worker，
验证三节点 DAG、四次明确命令审批、独立工作区分支，以及慢节点仍等审批时后继补位。
身份和模型服务为 fixture，Linux amd64／arm64 原生 CI 提供平台证据。

分布式 Council 综合／quorum／期限执行、审批合并、跨网关传输及剩余 P5–P6 运维验收
继续按[计划](PLAN.zh.md)实施，不能通过一个 Workflow 模板自动开启。独立预览发布
仍等待验收完成。

另见 [English](WORKFLOW.md)、[子任务](CHILDREN.zh.md)、[工作区](WORKSPACES.zh.md)、
[Web](WEB.zh.md)和[进度](STATUS.md)。
