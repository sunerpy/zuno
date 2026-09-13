# 组织共享 Memory

共享 Memory 通过 `SharedMemoryStore` 使用明确的租户所属命名空间。个人
Global／Project Memory 保留原有归属、证据和学习流程，不创建虚拟共享用户。

组织管理员为已安装的工作区创建空间，并向有效组织成员分配读取、提交候选或审核
权限。管理员可以审计空间，但模型召回仍要求明确的空间成员资格。提交者不能自行
批准内容；审核由另一名审核者通过获准应用完成，沿用已有 BFF 或 API 鉴权边界。

候选保存完整修改前后内容、文档版本、空间权限版本和状态摘要，复用个人 Memory
的威胁检测、唯一定位、去重及最终字符上限规则。文档、版本、候选状态、幂等回执
和审计在同一事务中提交。撤销要求当前文档仍等于原修改后的内容。版本或权限失效
时需要重新提交候选；配置变更会使未完成候选失效。

## API 与 SDK

| 路由 | 操作 |
| --- | --- |
| `GET /api/v1/workspaces/{workspace}/memory/spaces` | 有界的授权空间列表 |
| `GET /api/v1/memory/spaces/{space}` | 当前文档与调用者权限 |
| `PUT /api/v1/memory/spaces/{space}` | 管理员配置及版本 CAS |
| `POST /api/v1/memory/spaces/{space}/changes` | 提交有界候选 |
| `GET /api/v1/memory/spaces/{space}/changes/{change}` | 查看有权审核的完整差异 |
| `POST /api/v1/memory/spaces/{space}/review` | 按状态摘要批准、拒绝或撤销 |

SDK 提供 `sharedMemorySpaces`、`sharedMemorySpace`、`configureSharedMemory`、
`proposeSharedMemory`、`sharedMemoryChange`、`reviewSharedMemory`，版本保持
十进制字符串，并验证响应所属空间、工作区与候选身份。App/UI 交付仍暂停。

配置包含标题、工作区、启用状态、字符上限和明确的成员列表。新建时 expected
revision 为 `"0"`；更新时提交当前权限版本并替换成员列表。每工作区最多 32 个
空间，每空间最多 256 名成员和 256 个待审核候选，每候选最多 32 项编辑。
文档最多 32768 字符，候选请求最多 64 KiB，列表页最多 512 KiB。

## 召回与恢复

Worker 协议 12 在每次模型请求前单独读取共享快照。当前成员资格、空间启用状态
和用户继承的 `useMemories` 决定召回。权限撤销后替换旧共享上下文，检查点接续
不能继续注入已经撤回的文档。共享上下文最多 64 KiB，保留完整文档；超出容量的
空间 ID 会明确记入持久提示词，不截断条目冒充完整内容。

PostgreSQL 预览格式 25 添加空间、成员、候选、版本、请求与审计表，强制 RLS。
空间级事务锁协调并发写入，不锁住个人 Memory 或其他空间。精确格式 24 升级
保留个人会话、消息、Memory 和 MCP 操作数据。

本次交付支持明确提交并经过组织审核的共享笔记。个人自动提取仍保留在个人空间。
从个人证据自动提升为共享内容、共享学习维护尚未注册为可用能力；这些流程还需要
明确的共享授权、来源失效和审核集成。

共享空间权限不授予作者私人会话、凭证、附件或学习记录的访问权。关闭空间停止
召回，同时保留供有权管理员核查的历史。
