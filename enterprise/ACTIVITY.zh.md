# 公共活动协议与客户端恢复

`zuno-types::activity` 与 Worker／网关内部协议分开，保留六组独立类型：

| 类型 | 职责 |
| --- | --- |
| `SessionItem` | 消息、可见思考、调用、审批、Plan／Goal、压缩、产物和后台工作 |
| `InvocationAction` | 文件、进程、Web、Memory、Agent、Workflow、Council 或通用工具 |
| `InvocationSource` | 内置、MCP、扩展、提供商、外部 Agent 或明确未知来源 |
| `InvocationState` | 排队、等待、运行、成功、失败、拒绝、取消或不确定 |
| `ContentBlock` | 文本、代码、Diff、终端、结构化数据、图片或资源引用 |
| `UiAction` | 查看、批准／拒绝、回答、中断或请求经过授权的恢复 |

动态名称是经过校验的数据。名为 `read` 的工具不会因此获得内置来源或自动批准。
真实适配器声明动作和来源，共享内核将它们随工具定义冻结，并记录到每次调用。
展示信息与副作用、重放及权限策略互相独立。旧检查点缺少展示信息时可由同一已安装
适配器补齐，但不能借此替换工具 schema 或明确记录的来源。

## 持久记录

PostgreSQL 格式 13 增加所有者作用域的 `activity_session`、`activity_item` 和
`activity_frame`。消息／part 原始写入、公共 item 更新及不可变 frame 一起提交；
投影失败会回滚源写入。公共内容未变化时不分配新序号。
序号属于会话内逻辑游标，不是物理 rowid，也不暴露内部事件游标。

item 保留首次位置和最新 revision，frame 同时携带原位置。因此只加载了近期历史的
客户端，也能正确放置一个旧 item 的更新。消息与 part 的 parent ID 保留分组关系。
数据所有者将消息绑定到原始 Job，后续 turn 复用 provider call ID 时不会串用审批、等待或操作。
旧格式仅回填能由输入／请求来源唯一证明的关系，缺少证据时采用保守状态。
输入排队时立即显示，消费／取消后仍使用同一个公共消息 ID，不因物化而重复展示文本。
子任务自动报告具有不同的消息来源标记。

共享投影器逐字段选择公开内容，不复制提示词回执、模型凭证、Worker 租约、配置快照、
加密推理、续接签名或任意工具 metadata。提供商允许展示的思考默认折叠；胶囊中的摘要
不会重复已有同文思考。不确定操作保持不确定。类型化等待和执行事实由数据所有者提供，
不能从模型写出的标签推断。

用量复用内核规范化逻辑并使用精确十进制字符串；未知计量不伪造为零。
过长文本／参数预览有明确截断或省略标记，客户端预览不替代原始持久数据。

## HTTP 与 BFF

`/api/v1` 与 `/app/api/v1` 均提供：

| 接口 | 查询参数 |
| --- | --- |
| `GET /sessions/{session}/history` | `limit` 1–100，可选 `before`、`through` |
| `GET /sessions/{session}/frames` | `limit` 1–100，`after` 默认 `"0"` |

计数器使用规范十进制字符串。每一页重新检查成员、调用应用、当前策略和归属；
跨用户会话返回 not-found。响应不可缓存，大小限制为 1 MiB。

首次历史分页固定 `through` 快照，后续旧页沿用该值和返回的 `before`。
即使工具随后完成，旧快照中的内容仍不变化。新事件从 `through` 之后连续补读，
每页只推进到实际返回的最后一个 frame。慢客户端可以停止并补读，不需要服务器无限积压队列。

`CommittedFrame` 是已提交的公共历史；`LiveFrame` 使用独立 generation 与序号，
不携带权威计量或加密数据。实时进度通过独立、经过认证的 `/sessions/{session}/live` 快照读取。流式传输、
Plan／Goal／产物投影及相关操作处理器，在真实适配器完成前不宣称可用。

## TypeScript SDK

`enterprise/sdk` 是私有源码包。Rust 生成 JSON Schema、TypeScript 判别联合类型和
已打包的独立校验器；浏览器不需要运行时编译 schema 或 `eval`。生成物漂移进入预览 CI gate。

```sh
cd enterprise/sdk
npm ci --ignore-scripts
npm run check:generated
npm test
```

`ActivityClient` 使用同源 BFF，或显式配置 HTTPS bearer API；不会携带凭证跟随重定向、
把 HTTP 错误正文抄进异常，或接受 Worker 入口。`watch` 通过有界、可中断轮询补读
frame，断线后从最后提交游标继续。

`ActivityState` 原子应用整页，识别断档及冲突重复，不让迟到旧分页覆盖较新 revision。
历史窗口有上限，重新加载旧页不会倒退订阅游标；合并旧页必须沿用原快照边界。

原生用例覆盖真实 Worker 最终输出、排队输入、跨用户及撤权、更新期间快照分页、
投影事务回滚、可见／私有推理分离，以及格式 12 迁移失败回滚。
SDK 用例覆盖协议拒绝、精确计数、去重、断档、快照竞争、大小限制、凭证路由和订阅中断。

参见 [English](ACTIVITY.md)、[应用 API](APPLICATION.zh.md)、
[SDK](sdk/README.md)、[平台](PLATFORMS.zh.md)和[进度](STATUS.md)。

## 可替换的实时快照

Worker 通过 `/internal/worker/v1/live` 发送合并后的可见文本／思考和调用标签，
同时验证工作负载身份与当前 Job 凭证。数据所有者在接纳、提交时验证租约，绑定尚未完成的
原始 assistant 消息，并拒绝改变内容的重复序号及倒序更新。每个 Job 只保留最新有界快照，
不进入模型历史或用量计费。

Worker 的 `liveMillis` 默认 500，允许 100–5000，设为 null 可关闭。
本地最多保留 16 项、32 KiB 原始文本，网络合并发送不阻塞模型执行。
重试回滚替换草稿；消息提交后草稿立即隐藏。签名、加密提供商数据不参与实时投影。

PostgreSQL 格式 14 增加作用域临时行，并在 Job 阶段或执行尝试切换时原子清除。
公共读取重新检查当前策略、执行租约及尚未完成的来源消息，超过 30 秒未更新的快照不显示。
SDK 的 `ActivityClient.live` 和 `LiveActivity` 与持久状态分开，先补齐所需历史，
再接受当前 generation；过期 generation、倒序更新不会覆盖新草稿。
前端应分别命名文本／思考草稿键和调用 ID，无当前快照时清除实时内容。

固定格式 13 迁移夹具在 DDL 失败时保留消息、Memory 和持久 frame。
HTTPS 用例实际观察模型完成前的草稿，并验证完成后消失；SDK 验证 generation 替换、
重试清理和静默之后的新快照。

`UiAction::ViewWorkflow` 指向持久编排 Job，数据所有者从原消息执行 Job 与调用关系生成，完成后的调用仍可查看。模型提供的工具名或结果文本不能创建该关联。
