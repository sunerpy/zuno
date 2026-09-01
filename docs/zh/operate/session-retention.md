# Session 保留

保留是一项由 Zuno 拥有的能力，通过两个 `/api/session/prune` 操作以及对应的 CLI 命令暴露。

## 唯一必须弄对的一件事

`--archive` 是**可逆**的。`--delete` 是**不可逆**的。

- `--archive` 只写入一列：`session.time_archived`。什么都不会被移除。逆操作存在于库中
  （`zuno_db::prune::PruneRequest::restore_archive`，它把 `time_archived` 设回
  `NULL`），并由
  `crates/zuno-db/tests/prune.rs::prune_archive_is_reversible_without_deleting_session_data`
  覆盖。
- `--delete` 从下文列出的表中删除行、清扫孤立的 part，并把 artifact 收集切换为删除模式。
  没有撤销。只能从备份或数据来源恢复；这个二进制文件里没有任何东西能把它找回来。

大规模归档之前请注意一处不对称：**CLI 与 HTTP 界面能设置归档标记，但目前不能清除它。**
今天要撤销一次归档，意味着从 Rust 调用 `restore_archive` 或者自己清空该列。可逆性是真实
存在的，但它还不是一个命令行标志。

## 永远先预览

既不带 `--archive` 也不带 `--delete` 时，该命令是预览，不改动任何东西 ——
`crates/zuno-db/tests/prune.rs::prune_default_preview_is_inert_across_every_real_table`
断言了它在每张表上都是惰性的，而
`prune_preview_counts_exactly_match_the_subsequent_transactional_delete`
断言预览的计数就是随后删除产生的计数。

```sh
zuno session prune --older-than 90
zuno session prune --older-than 90 --format json
```

## 选项

| 选项 | 说明 |
|---|---|
| `--older-than DAYS` | 必填；保留窗口 |
| `--by updated\|created` | 该窗口作用于哪个时间戳；默认 `updated` |
| `--project PATH\|ID` | 限定到一个项目；默认是当前项目 |
| `--all-projects` | 所有项目；与 `--project` 互斥 |
| `--archive` | 设置可逆的归档标记；与 `--delete` 互斥 |
| `--delete` | 不可逆地移除；与 `--archive` 互斥 |
| `--include-shared` | 不把共享 session 从选择范围中排除 |
| `--include-recent` | 不把近期活跃的 session 从选择范围中排除 |
| `--force` | 当某个共享 session 的远端副本无法取消共享时仍然继续 |
| `--yes` | 预先确认一次删除；需要配合 `--delete` |
| `--format table\|json` | 输出形态；默认 `table` |

## 确认门禁

删除绝不会不经询问就执行。在 TTY 上你会看到一个提示。没有 TTY 且没有 `--yes` 时，命令会
拒绝：

```text
--delete requires --yes when stdin is not a TTY; nothing was changed
```

在提示处回答任何非 yes 的内容也是同一种拒绝：

```text
session deletion cancelled; nothing was changed
```

两者都在 `crates/zuno-cli/src/cmd/session_prune.rs` 中有断言。注意这个拒绝发生在**读取
stdin 之前**，因此管道中的一次删除无法被恰好到达的字节确认。

## 共享 session

一个共享 session 如果其远端副本无法取消共享，就会被拒绝，而不是在本地静默删除。`--force`
会继续执行，并在报告的 warnings 中原样说明：

```text
remote unshare failed for shared session <id>: <detail>; local rows were deleted because --force was supplied and the remote copy may survive
```

这是一句诚实的陈述：本地行已经没了，而远端副本可能还在。

## 一次删除会触及什么

由 `zuno_db::prune::DELETE_ORDER` 生成。该顺序由
`crates/zuno-db/tests/prune.rs::prune_delete_order_and_true_related_table_count_are_pinned`
固定，因为这个顺序正是在事务中途保持外键约束成立的关键。

**19 张表**，按此顺序：

<!-- generated:BEGIN prune-tables -->
| order | table |
|---:|---|
| 1 | `session_note_operation` |
| 2 | `session_note` |
| 3 | `memory_reflection_job` |
| 4 | `memory_reflection_delivery` |
| 5 | `learning_job` |
| 6 | `message_feedback` |
| 7 | `agent_job` |
| 8 | `work_item` |
| 9 | `work_plan` |
| 10 | `work_plan_archive` |
| 11 | `session_context_epoch` |
| 12 | `session_input` |
| 13 | `session_message` |
| 14 | `part` |
| 15 | `message` |
| 16 | `session_share` |
| 17 | `session` |
| 18 | `event_sequence` |
| 19 | `event` |
<!-- generated:END prune-tables -->

用以下命令重新生成：

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno --test docs
```

在表删除之后，没有存活 session 的 part 会被清扫，artifact 收集以删除模式运行。

## 读懂 artifact 警告

报告中可能带有：

```text
`<database>` contains <n> sessions; artifact reclamation is skipped because shared snapshot stores cannot be attributed and may belong to another channel's database.
```

这不是失败。它是说这次运行无法证明某个快照存储属于正在被清理的那些 session，因此没有动那些
字节。最常见的原因是用源码构建去访问某个发布版安装的数据目录 —— 两者选择的是不同的数据库
文件。参见 [migration.md](/zh/operate/migration#channel-数据库)。

如果你确知某个数据库里有 session，而这里的 `n` 是 `0`，那说明你看的是错误的数据库，而不是
你的 session 丢了。

## 删除单个 session 时的派生学习

交互式删除会明确要求选择：

- **保留学习**：保留派生 Experience；删除对话后，把可空的 session/message 来源指针置空；
- **清理学习**：先为已应用的 Memory 和 Skill 建立待审撤销候选，拒绝仍在等待审核且引用
  这些证据的候选，然后把 Experience 标记为 `forgotten`。

清理不会静默移除已应用的 Memory 或 Skill。TUI 会询问该选择；ACP 的
`session/delete` 必须传入 `cleanupDerivedExperiences: true|false`。独立 CLI
`zuno session delete <id>` 必须且只能提供
`--keep-derived-experiences` 或 `--cleanup-derived-experiences` 之一；如果确有派生
Experience，清理操作需要在已挂载学习 profile 的 TUI 或 ACP 中执行，以便创建待审撤销候选。

`zuno session prune --delete` 是批量保留策略路径。它删除 session 自有的反馈和学习 job，
但保留项目级 Experience、模式、评测、Memory 和 Skill 候选。这个行为属于删除确认的一部分，
不会把长期项目学习静默当成对话数据删除。

## 通过 HTTP

```sh
curl 'localhost:PORT/api/session/prune?olderThan=90&by=updated'
```

`GET` 是预览且是惰性的。`POST` 会产生变更，并且要求显式给出 `apply: true`：

```sh
curl -X POST localhost:PORT/api/session/prune \
  -H 'content-type: application/json' \
  -d '{"olderThan":90,"action":"archive","apply":true}'
```

没有它时：

```text
session prune mutation requires `apply: true`; nothing was changed
```

CLI 与 HTTP 的预览输出逐字节相同的 JSON ——
`crates/zuno-cli/src/cmd/session_prune.rs::session_prune_cli_and_http_preview_json_are_byte_identical`
—— 因此运维者可以基于其中一个构建策略，并用另一个做审计。
