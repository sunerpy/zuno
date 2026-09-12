# 原生子任务的持久派发

企业子任务复用原生 `agent_job`、`ChildTurnHost`、完成信封及共享驱动的类型化
等待／消费边界，不创建另一套 Agent 循环。

`ChildTurnHost::dispatch` 返回原生就绪结果或
`ChildTurnDispatch::Pending(WaitRef)`。`TaskTool::dispatch` 先验证原有委派契约、
目标、模型路由、深度和权限，再交付该结果。前台 `running` 文本会被拒绝；个人模式
继续返回已完成结果，远程装配通过 `ChildToolDispatcher` 把 Pending 直接交给共享驱动。

## 原子接纳与投递

`ChildDefinitionCatalog` 属于控制面，解析精确父／子配置、模型选择与委派上限。
网络意图只携带稳定调用、参数摘要、逻辑键和有界任务数据。内部请求同时验证工作负载
身份和当前父 Job grant，调用方不能授予自身其他作用域或提高上限。

前台派发先幂等存储意图，不启动子任务。父任务提交等待检查点时，在同一事务中创建
子会话、继承会话 Memory 限制、接纳原生 Job 与输入、登记等待并释放父 Worker 槽位。
审计失败会回滚整个边界。父任务检查点／终态不再保留的暂存意图会被淘汰，避免永久
占用逻辑键、阻塞用户后续明确委派。
另有部分唯一约束保护已有子会话：只能被一条暂存／活动续接占用，并发续接在接纳时即拒绝。

后台派发立即接纳子任务，返回不同的 Job 与会话标识。`nextStep` 使用持久 Outbox；
`quiet` 保存结果而不创建父输入。完成通知不能自动继续已取消或暂停的父任务。

子任务同时提交原生 Job 终态、实际 assistant 文本、类型化结果、完成信封和待投递标记，
结算时不持有父会话锁。发布阶段随后取得父锁，处理结果先于等待到达的情况，并通过
用户输入共用的根任务接纳代码创建下一回合报告。通知允许重复；完成消费、原工具结果
写入及检查点推进原子去重。

模型得到终态结果及可继续使用的子会话／Job ID，不注入 reasoning part。输入物化
会核对委派的子任务归属，以及自动报告的持久完成信封；来源保持可追溯，不会在 Memory
证据中把子 Agent 生成内容重新当成用户原始陈述。

## 存储与验证

PostgreSQL 预览格式 10 将 Job 父会话与执行会话分开，为 `runtime_child` 强制所有者
RLS，并保留格式 1–9。精确格式 9 fixture 含运行时和 Memory 数据，验证标记写入前回滚。

真实 PostgreSQL 测试覆盖暂存、重复／冲突调用、等待事务回滚、独立槽位、Worker 更换、
重复通知与消费、quiet／next-step 及已停止父任务。已认证 HTTPS 测试通过远程 Worker
运行原生 TaskTool，验证父等待、子执行、替换父 Worker、原调用只落一份结果且模型请求
不重复。

只有注入真实 catalog，`WorkerStateService::with_children` 才挂载处理器。独立可执行
定义配置有效 `delegation.targets` 后，现已安装 catalog 与原生 task dispatcher。
子模型、网关由引用定义固定，工作区准备控制执行准入。Job／任务树取消见[控制](CONTROL.zh.md)。审批合并、跨网关传输与
Workflow／Council 继续按 P4 实现，配置见[工作区](WORKSPACES.zh.md)。

参见 [English](CHILDREN.md)、[持久等待](WAITING.zh.md)、
[PostgreSQL](POSTGRES.zh.md)及[进度](STATUS.md)。

子定义使用 `agent.mode: "completion"` 时接收 `ModelOnly`，不分叉工作区，配置中
没有环境或委派能力；普通子定义仍使用已有 Docker 分支与审批路径。
