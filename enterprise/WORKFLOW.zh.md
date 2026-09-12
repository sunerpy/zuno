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
占用原始 Worker 名额，但不运行节点模型或工具；节点可选择其他配置网关，通过认证的
快照传输准备其独立工作区。
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
仍在同一受保护事务内前进。Worker 协议 11 承载 Workflow 命令，检查点 schema 4 保持不变，
网关协议 5 校验 Workflow 工作区准备。控制面与 Worker 使用匹配版本，契约更新时重新生成公共 SDK。

## 持久 Council

Agent 定义可以安装 `councils`。每项包含 `preset`、综合模型的 `synthesis` 配置引用，
以及从席位 Agent 名称到配置引用的 `repairs` 映射。使用 `--definition-ref` 生成精确引用。
preset 复用原生字段：`name`、`sourceId`、`seats`、`quorum`、`maxParallel`、
`deadlineMs`、`seatOutputBytes`、`retryPolicy.maxRetries` 和
`synthesisPolicy.{timeoutMs,maxInputBytes}`。原生工具 ID 保持 `council_run`；
调用方选择 preset 并提供问题。

席位、修正与综合定义均需安装、保留父逻辑工作区，并在父 `delegation.targets` 中
明确授权。修正与综合使用 `agent.mode: "completion"`，不能带环境、委派、
Workflow 或 Council catalog。修正定义必须保持原席位的精确模型绑定，包括凭证引用
和提供商选项；配置阶段拒绝在格式修正时更换模型。子任务额度至少为
`席位数 × (maxRetries + 1) + 1`，委派至少两级。每定义最多 32 个 preset，
每次最多 12 席位、每席位 3 次格式修正、总期限最长 10 分钟。

Council 复用原生 Workflow 协调、子 Job、工作区分支、持久等待及完成消费。
原始调用方准备完工作区后释放 Worker 名额；席位等待仍占逻辑席位名额。
每个初始席位使用独立工作区分支。只有已完成的公开答案文本进入校验，私有推理
和工具 transcript 不进入修正或综合提示。无效答案可以建立有界的模型专用修正
Job，不重放原 Agent 的命令。修正次数和来源完成摘要持久保存，不随进程重启丢失。

工作区准备完成后，数据库在激活时确定总截止时间。席位时间包含排队、审批等待和
格式修正，综合时间在总期限内预留。领取和续租不能延长适用期限。席位超时通过同一
任务树取消和网关 Outbox 停止；在途操作等待真实回执，最多五秒且不超过剩余总期限。
外部效果未确认时 Council 保持 `uncertain`，合法迟到回执仍保存，但不擅自继续
已经不确定的父任务。

所有席位结算后才综合，保留原始顺序和不同意见。quorum 只计算截止时间前完成且
通过校验的答案；缺失、超时、无效和失败席位都会记录，但不能作为票数。综合 Job
没有工具和常驻 Memory，输入有界并固定；超过输入限制、票数不足或综合超时直接
失败，不伪造成功。取消撤销整棵任务树的执行权，Worker 更换继续检查当前授权。

PostgreSQL 格式 16 增加作用域化 Council、席位状态、尝试来源和可选 Job 截止时间。
精确格式 15 fixture 验证原有 Job、Workflow／节点、消息、Memory 和活动 frame
保留，并检查 marker 前故障原子回滚。Worker 协议 11 增加内部 Council 接纳；
网关协议 5 支持远端节点工作区，检查点 schema 4 保持不变。`WorkflowRunView.kind` 区分 `workflow`
与 `council`，可选 `council` 视图包含类型化阶段、席位状态、次数和十进制精确期限。
公开视图不包含私有配置、提示词、租约或凭证。

通用远程 Council 不暴露原生 review binding。Review 证据接纳和工作区审批合并
继续作为独立能力实现。

## 验收范围

数据库用例覆盖激活回滚、并发领取、等待和补位、依赖输入、Worker 接管、终态消费、
失败、不确定性、取消与归属边界。原生可执行用例启动控制面、两个网关及两个 Worker，
验证三节点 DAG、四次明确命令审批、独立工作区分支，以及慢节点仍等审批时后继补位。
身份和模型服务为 fixture，Linux amd64／arm64 原生 CI 提供平台证据。

Council 数据库用例覆盖模型专用修正、quorum、有界期限、等待取消、租约撤销、
迟到操作回执和不确定结果。原生用例增加两个独立且明确审批的席位命令、一次格式
修正及模型专用综合，随后消费原始父调用；这些后端证据不需要浏览器。

审批合并及跨网关传输使用已配置的工作区提供者。完整 P5–P6 运维验收
继续按[计划](PLAN.zh.md)实施。独立预览发布
仍等待验收完成。

另见 [English](WORKFLOW.md)、[子任务](CHILDREN.zh.md)、[工作区](WORKSPACES.zh.md)、
[Web](WEB.zh.md)和[进度](STATUS.md)。
