# Memory 与学习

Zuno 用三种不同形式保存可复用信息。它们的复核和删除规则不同；把它们当成一个存储，容易
批准错误的内容。

| 类型 | 保存什么 | 出现在哪里 | 谁能应用 |
| --- | --- | --- | --- |
| 常驻 Memory | 一条简短的全局偏好或项目规则 | 稳定的 `memory.global`、`memory.project` Prompt section | 经过复核或 policy 批准的 `MemoryCandidate` |
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

HTTP client 通过 `GET|PUT /api/session/{sessionID}/memory-policy` 读写 policy。更新必须带
`expectedRevision`；revision 过期返回 `409`。Client 只能请求 `enabled` 或 `disabled`，
`excluded` 由宿主设置。

启用 `learning.post_turn.disable_on_external_context` 后，如果已完成回合使用了成功的 Web 或
MCP 结果，该 Session 会进入 `excluded`。Zuno 按持久工具元数据判断，不扫描 transcript 文本。

## 提议并复核常驻 Memory

模型可见的 mutation 工具是 `memory_propose`。它接受：

- `target`：`global` 或 `project`；
- `action`：`add`、`replace` 或 `remove`；
- add/replace 使用的完整 `content`；
- replace/remove 使用的唯一 `old_text` locator；
- 可长期复用的 reason 与 confidence。

工具只创建可审计的 `MemoryCandidate`，不能直接写常驻文件。校验会拒绝格式错误或有歧义的
操作、超出容量的结果、Prompt injection 模式、已知凭据字面量、不可读文件，以及与已复核版本
不一致的外部漂移。临时环境故障、未解决猜测、秘密和任务过程叙述都不应写入 Memory。

默认 promotion policy 是 `review`。另外两种 policy 改变有效提案的应用时机：

| `memory.promotion` | 行为 |
| --- | --- |
| `review` | 所有提案留给用户复核 |
| `high_confidence` | 应用达到 `memory.auto_confidence` 的提案，其余保留 |
| `automatic` | 应用所有通过同一校验与安全检查的提案 |

`memory.auto_confidence` 默认 `0.9`。Learning 生成的提案始终使用更窄规则：只有 confidence
`>= 0.9` 的 project Memory 可以自动应用。Global 或低 confidence 提案仍保持 pending。

### Apply 与 undo 恢复

修改文件前，Zuno 先保存精确的 before/after 条目列表，再把 candidate 设为 `applying`。
常驻文件原子替换后，状态变为 `applied`。Undo 同样经过 `undoing` 与 `undone`。

进程中断后，启动恢复会把当前文件与两个 snapshot 比较：

- 与 after 相同，证明 apply 已完成；
- 与 before 相同，证明没有完成；
- 任何第三种状态都标记为 `uncertain`。

恢复分支不会机械重放文件 mutation。先检查 `uncertain` candidate 与常驻文件，再决定保留哪一版。
外部修改不会被覆盖。

## 记录与检索 Experience

Learning 默认启用。`learning.enabled` 是总上限；其下的 `learning.use` 与
`learning.generate` 可以独立控制。因此项目可以在不启动 extractor 的情况下检索旧证据，
也可以记录新证据但不放进前台 Prompt。

已完成回合至少包含一项工具调用、产物、错误恢复、显式纠正或反馈时，才符合自动提取条件。
默认 scheduler 等待六小时空闲，每 60 秒轮询一次，每次唤醒最多领取两个 job。Job identity 是
`(session_id, source_message_id, extractor_version)`，重试和重启不会产生第二批记录。

`/reflect turn` 选择最近完成的 assistant 回合；`/reflect session` 使用持久 Session transcript。
手工 reflection 会立即到期，但仍遵守 Session generation policy 与 external-context 规则。

Extractor：

- 优先使用 `learning.extractor_model`；未配置时使用当前 Provider 可达的 `small_model`，再使用
  Session model；
- 接收脱敏重放与 structured response schema；
- 没有工具、网络、文件系统 authority 或前台 Session identity；
- 持久化精确请求与终态；
- 一个持久 job 最多尝试三次。

Settlement 在一个 transaction 中保存可接受的 Experience 与 evidence。某一项含有无法解析成
模型可见文本的编码时，只拒绝该项，不丢弃同批干净条目；job 结果用 `refusedItems` 记录原因。

自动检索优先当前 project，默认最多五条，渲染后 context budget 为 1,200 token。每条插入项都
携带持久 id、source、内容与 digest，精确 section 会写入 Prompt receipt。最小匹配也装不下时，
Zuno 不插入记录，并发送 `learning.retrieval_skipped`，说明所需与配置 token 数。

需要更深的显式搜索时，使用只读 `experience_search` 工具：

```text
experience_search(query: "sqlite migration preserved messages", limit: 10)
```

进入 FTS5 前，搜索输入会被引用并限长，因此标点和 FTS operator 仍是数据，不会成为查询语法。

## 把重复证据变成 Skill

后台 pattern miner 对可 promotion 的 project Experience 分组。默认至少需要三条新记录和三个
独立 Session，才会自动创建 project Skill candidate。跨项目聚合要求同一已 promotion 模式至少
出现在两个项目中。显式 `/learn promote <experience-id>` 可以创建单证据 project pattern，
但不能绕过 review 或 evaluation。

Skill candidate 包含完整提议文件、unified diff、learned rule、Experience id、source identity
与 digest、目标路径及操作。内置或只读 Skill 永远不会被覆盖；Zuno 会提议一个名称不同的
project companion。

安全流程刻意拆开：

1. `/learn skill-review <candidate-id>` 启动不可变的离线 cassette suite。
2. Baseline 与 candidate 使用相同模型、toolset digest、budget、temperature、seed 和已记录工具响应。
3. 只有引用的失败得到修复、保护用例没有 critical regression、加权指标不下降时才通过。
4. 通过只把 candidate 设为 `approved`，不会写文件。
5. `/learn skill-apply <candidate-id>` 另行执行带 digest 检查的 apply。

Source 漂移会把 candidate 设为 `stale`。Apply 与 undo 保存 before/after snapshot；重启时只分类
文件系统现状，不重放不确定副作用。已应用 candidate 用 `skill-undo`，不需要的用
`skill-reject`。

## 反馈、遗忘与保留

反馈只指向已持久化的 assistant Message，并要求 expected revision。Revision `0` 表示此前不能
已有反馈；之后每次写入必须匹配当前 revision。过期写入返回 conflict，不覆盖较新的意见。

`/learn forget <experience-id>` 把证据标记为 forgotten。移除 source evidence 不会静默删除已应用
Memory 或 Skill。Zuno 创建待复核的 inverse/revocation candidate，并保留审计所需证据。

Transcript retention 删除对话时，会移除 Session-owned feedback 与待处理 learning job。
Project Experience、Memory、pattern、evaluation result 与 Skill candidate 默认继续保留，除非用户
显式选择清理派生学习。执行破坏性维护前阅读 [Session 保留](/zh/operate/session-retention)。

## 配置入口

精确字段与默认值见[配置项参考](/zh/config/reference#memory-与用户学习)。下面的最小覆盖把使用与
生成分开：

```json
{
  "memory": {
    "promotion": "review"
  },
  "learning": {
    "use": true,
    "generate": false
  }
}
```

已退役的 `memory.reflection` 与 `memory.nudge_interval` 会被拒绝。Post-turn 提取属于
`learning`。

## 相关页面

- [Goal、Plan 与 Todo](/zh/guide/durable-state)
- [工具](/zh/guide/tools)
- [Session 与回合](/zh/guide/sessions)
- [常驻 Memory 设计（英文）](/design/memory-learning)
- [用户学习闭环设计（英文）](/design/user-learning-flywheel)
