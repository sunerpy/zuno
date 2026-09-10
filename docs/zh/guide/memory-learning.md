# Memory 与学习

Zuno 自动提取并维护可复用记忆。普通 Memory 默认不需要逐条 approval；
会改变执行方法的 Skill 仍需复核、评估和显式应用。

| 类型 | 保存什么 | 出现在哪里 | 谁能应用 |
| --- | --- | --- | --- |
| 常驻 Memory | 一条简短的全局偏好或项目规则 | 带版本的 `memory.global`、`memory.project` Prompt section | 默认自动应用，保留可审计的 `MemoryCandidate` |
| Experience | 一次结果、纠正、失败或已验证流程的证据 | 检索得到的 `learning.experiences` section 与 `/learn` | Learning 服务写入证据，不直接修改 Memory |
| Skill candidate | 带完整 `SKILL.md`、diff 与证据的可复用方法提案 | `/learn` 复核状态 | 用户复核且离线 evaluation 通过后才能应用 |

未解决问题可以作为 Experience 保存，但不能成为 Memory、模式证据或 Skill evaluation 证据。

## 查看当前状态

TUI 提供四个原生命令：

| 命令 | 用途 |
| --- | --- |
| `/memory` | 查看常驻条目，批准、编辑、拒绝、移除或撤销 Memory candidate |
| `/memories` | 设置当前 Session 是否使用 Memory、是否允许生成学习 |
| `/learn` | 查看 Experience、反馈、模式、evaluation run 与 Skill candidate |
| `/reflect [turn\|session]` | 立即运行无工具权限的 extractor，不等待后台 eligibility |

运行 `/learn help` 查看完整 action：

```text
/learn
/learn list [offset]
/learn get <experience-id>
/learn inspect-memory|import-memory <global|project>
/learn remember <stable fact, preference, or project rule>
/learn issue <unresolved issue>
/learn solved <experience-id> <resolution>
/learn forget <experience-id>
/learn promote <experience-id>
/learn feedback <assistant-message-id> positive|negative <expected-revision> [note]
/learn pattern-promote|pattern-reject <pattern-id>
/learn skill-review|skill-apply|skill-reject|skill-undo <candidate-id>
```

HTTP projection 是 `GET /api/session/{sessionID}/learning`。ACP 在
`_meta.zuno.learning` 发布同一状态。它们只是持久存储的视图；关闭生成不会让已有记录不可读。

## Session policy

全局配置决定能力上限。每个 Session 在 materialize 时冻结一份带 revision 的 policy：

| 字段 | 含义 |
| --- | --- |
| `use_memories` | 在后续 Prompt 中加入常驻 Memory 与自动检索的 Experience |
| `generation=enabled` | 允许显式与自动学习 |
| `generation=disabled` | 停止新提取，保留已有 Memory 与 Experience |
| `generation=excluded` | 不符合条件时的 fail-closed 状态；同一 Session 不能重新启用生成 |

`/memories` 修改当前 Session policy。关闭使用只影响后续 Prompt 组装，不删除文件、Experience、
candidate 或审计记录。配置默认值之后发生变化，也不会重写已有 Session。

新子会话在创建 Session 和 job 的同一事务中读取父会话最新持久 policy。父策略 revision
为 1 或更大完全合法；没有 revision 的默认值是独立类型，仅在旧父会话没有 policy 行时使用。
子会话从自己的 revision 1 开始，继承 disabled／excluded 等选择；之后父策略改变也不会重写
已委派的子会话。不需要重建数据库或把父策略 revision 重置为零。

HTTP client 通过 `GET|PUT /api/session/{sessionID}/memory-policy` 读写 policy。更新必须带
`expectedRevision`；revision 过期返回 `409`。Client 只能请求 `enabled` 或 `disabled`，
`excluded` 由宿主设置。

启用 `learning.post_turn.disable_on_external_context` 后，如果已完成回合使用了成功的 Web 或
MCP 结果，该 Session 会进入 `excluded`。Zuno 按持久工具元数据判断，不扫描 transcript 文本。

## 自动维护常驻 Memory

旧工具名 `memory_propose` 已替换为 `memory_update`。权限规则、工具开关或 Agent 工具名单
仍使用旧名时，配置校验会要求改名，不会静默丢弃原来的 deny／禁用选择。

模型可见的 mutation 工具是 `memory_update`。它接受：

- `target`：`global` 或 `project`；
- `action`：`add`、`replace` 或 `remove`；
- add/replace 使用的完整 `content`；
- replace/remove 使用的唯一 `old_text` locator；
- 从 `memory_read` 或当前 Prompt 获取的 `expected_revision`；不传 revision 时，
  replace/remove 必须完整复制现有条目，不能只提供子串；
- 可长期复用的 reason 与 confidence。

工具通过受限 Memory 服务提交可审计的 `MemoryCandidate`，不获得任意文件写入权限。
只读 `memory_read` 返回当前 global/project 条目和 revision，支持 `target`、`query`、
`limit`，并报告因来源失效而隐藏的条目数。校验会拒绝格式错误或有歧义的
操作、超出容量的结果、Prompt injection 模式、已知凭据字面量、不可读文件，以及与已复核版本
不一致的外部漂移。临时环境故障、未解决猜测、秘密和任务过程叙述都不应写入 Memory。

默认 promotion policy 是 `automatic`。已有配置显式指定的 `review` 或 `high_confidence`
仍然生效：

| `memory.promotion` | 行为 |
| --- | --- |
| `review` | 所有提案留给用户复核 |
| `high_confidence` | 应用达到 `memory.auto_confidence` 的提案，其余保留 |
| `automatic` | 应用所有通过同一校验与安全检查的提案 |

`memory.auto_confidence` 默认 `0.9`，仅用于 `high_confidence`。前台和后台使用同一 promotion
策略。Global 自动记忆必须引用明确的用户证据，且只保存跨项目偏好；仓库知识留在 project。

Memory 是原生的受限数据能力，不触发 strict 模式通用的副作用审批；显式工具 `deny`/`ask`
和会话 generation policy 仍然生效。记忆不会授权 Shell、文件、MCP、Skill 或更改权限。
`build`、`deep`、`general`、`fixer`、orchestrator 可调用 `memory_update`；
只读角色只获得记忆读取与检索能力。

后台采用两阶段流程：先提取带来源的 Experience 与原始记忆建议，再由独立、无工具权限的
维护任务结合当前记忆和用户更正进行去重、更新与合并。每次最多选择 64 条近期有效经验，
仍受配置的输入预算限制；最多 32 项修改在一个事务中提交。无变化也保存处理水位，避免每次轮询
都重复付费调用。无效方案最多进行一次语义修复；版本冲突、租约失效或来源策略变化时，不提交旧方案。
任务只能由绑定相同规范记忆路径的 worker 领取；切换 worktree 不会消耗其他路径任务的重试次数，
也不会把它们误判为过期任务。

### Apply 与 undo 恢复

记忆文件及其直接管理目录必须是普通文件／目录，不能是符号链接或 Windows junction。
导入、读取以及发布文件投影前都会检查，避免仓库中的 `.zuno` 或 `RULES.md` 链接把免审批
记忆操作转向其他文件。记忆应放在受管理的位置；其他文件仍通过原有权限控制的文件工具操作。

常驻条目现在由 SQLite revision 保存权威状态。应用 candidate 时，精确的 before/after
快照、新条目、版本历史与 `applied` 终态在同一事务中提交。Undo 同样在一个事务中推进
revision 并记录 `undone`。两个写入者不能同时替换同一个旧版本。

Global 与 project Markdown 文件是可读投影。已有文件只导入一次；后续直接修改文件不会
静默替换已接受的 Memory。投影失败不会丢失已提交条目。启动时可以从记录的版本修复丢失的
投影，但会保留并报告内容不同的外部文件。协作写入者使用操作系统共享锁保护比较与原子替换。

每个前台回合捕获当前 Memory revision。另一个会话的修改在下一回合边界可见；已经持久化
的 Prompt receipt 仍能重建当时使用的原版本。

旧版本可能遗留 `applying` 或 `undoing` candidate。启动恢复继续按其精确快照分类：

- 与 after 相同，证明 apply 已完成；
- 与 before 相同，证明没有完成；
- 任何第三种状态都标记为 `uncertain`。

这些历史不确定 mutation 不会机械重放。先检查 `uncertain` candidate 和保留的文件，
再决定保留哪一版。修复投影不会重复逻辑 Memory 操作，也不会再次推进 revision。
重复添加已有条目同样不会推进文档版本。

`/learn inspect-memory global|project` 展示已接受版本、文件条目与投影错误。
检查外部修改后，使用 `/learn import-memory global|project` 显式导入新版本。
升级前遗留的不确定写入，在检查和导入前不会进入 Prompt，也不会静默选择或删除其中一边。

## 记录与检索 Experience

Learning 默认启用。`learning.enabled` 是总上限；其下的 `learning.use` 与
`learning.generate` 可以独立控制。因此项目可以在不启动 extractor 的情况下检索旧证据，
也可以记录新证据但不放进前台 Prompt。

已完成回合至少包含一项工具调用、产物、错误恢复、显式纠正或反馈时，才符合自动提取条件。
默认 scheduler 等待六小时空闲，每 60 秒轮询一次，每次唤醒最多领取两个 job。Job identity 是
`(session_id, source_message_id, extractor_version)`，重试和重启不会产生第二批记录。
项目学习由进程级 supervisor 持有，ACP 会话释放前台宿主后仍可继续，不需要保留整个前台运行时。
新进程会检查最近七天内最多 64 个尚未入队的完成回合，并从 SQLite 恢复待处理任务。
进程退出后不会继续在后台执行。

`/reflect turn` 选择最近完成的 assistant 回合；`/reflect session` 在整个持久 Session 中选择有上限的来源。
手工 reflection 会立即到期，但仍遵守 Session generation policy 与 external-context 规则。手工任务还包含精确来源输入的 digest，因此新增反馈或扩大到整个 Session 会成为新任务，
相同输入仍然幂等。新显式输入入队时，会撤销同一 source message 下旧的排队/运行中提取租约。
重试不能借用旧 Experience 的验证标记自动批准发生变化或未验证的 Memory。

Extractor：

- 优先使用 `learning.extractor_model`；未配置时使用当前 Provider 可达的 `small_model`，再使用
  Session model；
- 接收有上限的脱敏来源记录，带精确的 Part/Feedback 地址、source digest 与权威验证标记；
- 限制输入、输出与请求总时长；无效 JSON 最多修复一次，且共用原请求期限；
- 没有工具、网络、文件系统 authority 或前台 Session identity；
- 持久化精确请求与终态；
- 一个持久 job 最多尝试三次。

Settlement 在一个 transaction 中保存可接受的 Experience 与 evidence。某一项含有无法解析成
模型可见文本的编码时，只拒绝该项，不丢弃同批干净条目；job 结果用 `refusedItems` 记录原因。
引用必须匹配提供的来源地址与原文片段，存储前还会核验来源是否变化。模型自报高置信度不再足以
自动写入 Memory：维护任务重新验证来源字节，需要权威工具成功回执，或已验证的用户纠正／偏好，
包括用户显式记录。记忆工具的结果不能循环充当新证据。不受支持的引用保留为未验证观察。
每次领取任务都使用独立租约令牌和心跳；租约失效或会话被排除后，
自动 Memory 提交会被拒绝。

自动检索优先当前 project，默认最多五条，渲染后 context budget 为 1,200 token。每条插入项都
携带持久 id、source、内容与 digest，精确 section 会写入 Prompt receipt。最小匹配也装不下时，
Zuno 不插入记录，并发送 `learning.retrieval_skipped`，说明所需与配置 token 数。

需要更深的显式搜索时，使用只读 `experience_search` 工具：

```text
experience_search(query: "sqlite migration preserved messages", limit: 10, match: "any")
```

进入 FTS5 前，搜索输入会被引用并限长，因此标点和 FTS operator 仍是数据，不会成为查询语法。
自然语言默认按有效词召回并重排；`match: "all"` 要求全部词匹配。中日韩文本使用 trigram 索引，
短词采用有扫描上限的回退。索引由迁移创建、写入触发器维护，搜索是纯读取。结果会显示引用和验证状态。

`/learn` 展示队列数量、到期时间、近期错误以及最近一次召回的条目或跳过原因。
`/learn list [offset]` 按每页 100 条浏览 Experience。前台回合选用上下文时记录条目 id 与使用次数；
普通搜索不会改写这些次数。`/learn get <experience-id>` 可查看引用、验证状态与使用详情。

## 把重复证据变成 Skill

后台 pattern miner 对已验证、可 promotion 的 project Experience 做语义归并，允许同一规则的不同措辞。默认至少需要三条新记录和三个
独立 Session，才会自动创建 project Skill candidate。跨项目聚合要求同一已 promotion 模式至少
出现在两个项目中。模型给出的分组必须引用已知证据 id；证据和规则不变时，保留已经 promotion 的状态与版本。
显式 `/learn promote <experience-id>` 可以创建单证据 project pattern，
但不能绕过 review 或 evaluation。

Skill candidate 包含完整提议文件、unified diff、learned rule、Experience id、source identity
与 digest、目标路径及操作。内置或只读 Skill 永远不会被覆盖；Zuno 会提议一个名称不同的
project companion。

安全流程刻意拆开：

1. `/learn skill-review <candidate-id>` 显式启动不可变的离线 cassette suite。
2. Baseline 与 candidate 分别执行有上限的模型任务。工具只能返回参数精确匹配的录制结果，
   未知调用不会访问真实文件系统或网络。评分器依据实际回答和调用轨迹评分，任务执行阶段看不到标准答案。
   两个版本使用同一模型和执行预算。
3. 只有引用的失败得到修复、保护用例没有 critical regression、加权指标不下降时才通过。
4. 通过只把 candidate 设为 `approved`，不会写文件。
5. `/learn skill-apply <candidate-id>` 另行执行带 digest 检查的 apply。

打开会话只装配评测器，不会自动开始 Skill 评测，也不要求额外配置模型；审核复用上面解析出的学习模型。
显式跨 Provider 模型不可达时会报告不可用，不会静默替换。Skill 审核会产生额外模型请求。
这里的“离线”指工具使用录制结果；模型生成仍然调用已配置的 Provider API。

每次评测都有总期限与所有权令牌。其他会话启动不会中断仍有效的评测；取消或到期会保留终态诊断，
旧执行不能覆盖新一轮审核。Source 漂移会把 candidate 设为 `stale`。Apply 与 undo 保存 before/after snapshot；重启时只分类
文件系统现状，不重放不确定副作用。协作写入者与恢复器共用 OS 路径锁。已应用 candidate 用 `skill-undo`，不需要的用
`skill-reject`。

## 反馈、遗忘与保留

反馈只指向已持久化的 assistant Message，并要求 expected revision。Revision `0` 表示此前不能
已有反馈；之后每次写入必须匹配当前 revision。过期写入返回 conflict，不覆盖较新的意见。

`/learn forget <experience-id>` 把证据标记为 forgotten，并在同一事务中撤回失去全部来源支持的
派生 Memory，不再生成必须审批才能生效的记忆撤回。不会恢复更正前的旧文本；仍有独立来源的条目、
导入内容和用户主动再次确认的内容会保留。显式遗忘或 undo 的内容，不能从未变化的旧证据重新写回。

来源被修改／删除时，相关记忆在维护任务运行前就停止加载；关闭未来生成则不会遗忘已有记忆。
Skill 撤回仍单独复核，因为它改变可执行的方法。

Transcript retention 删除对话时，会移除 Session-owned feedback 与待处理 learning job。
Project Experience、Memory、pattern、evaluation result 与 Skill candidate 默认保留审计记录，
除非用户显式选择清理派生学习；自动记忆的原始来源已经丢失时，不再加载该记忆。
执行破坏性维护前阅读 [Session 保留](/zh/operate/session-retention)。

## 配置入口

精确字段与默认值见[配置项参考](/zh/config/reference#memory-与用户学习)。下面的最小覆盖把使用与
生成分开：

```json
{
  "memory": {
    "promotion": "automatic"
  },
  "learning": {
    "use": true,
    "generate": false
  }
}
```

已退役的 `memory.reflection` 与 `memory.nudge_interval` 会被拒绝。Post-turn 提取属于
`learning`。

## 常驻 Memory 的存放位置

| Scope | 文件 | 默认上限 |
| --- | --- | --- |
| 全局 agent 笔记 | `$CONFIG/memory/MEMORY.md` | 2200 字符 |
| 项目规则 | `<worktree>/.zuno/RULES.md` | 3000 字符 |

`zuno debug paths` 会打印两个解析后的路径，并给尚不存在的存储标记 `(absent)`。不存在是正常
状态而不是故障：存储由第一次通过复核的写入创建，而不是启动时创建，所以全新安装没有 `memory/`
目录，空 scope 也不会占用任何 prompt 字节。

两个上限都可配置，对应 `memory.global_char_limit` 与 `memory.project_char_limit`；写入被拒绝
时，错误信息会指出可以提高该上限的那个键。计数单位是 Unicode 标量值，既不是字节也不是 token
——按字节会让同一条规则仅因为用中文书写就贵三倍，而按 token 计数会在换模型时让已经落盘的内容
突然超限。

全局上限刻意小于项目上限：全局笔记会进入**每个** Session 的 prompt，包括与它们毫无关系的仓库；
项目规则只在为它付出预算的那个仓库里加载。

## 相关页面

- [Goal、Plan 与 Todo](/zh/guide/durable-state)
- [工具](/zh/guide/tools)
- [Session 与回合](/zh/guide/sessions)
- [常驻 Memory 设计（英文）](/design/memory-learning)
- [用户学习闭环设计（英文）](/design/user-learning-flywheel)
