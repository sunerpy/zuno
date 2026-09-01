# History 与 Notes 连续性配置

Zuno 可以向模型提供两个可选的连续性工具：`history` 用于恢复当前会话中的规范化证据，
`notes` 用于保存持久工作文档。两者默认都关闭。启用它们只会改变模型可见的工具面，不会
自动把全部内容塞进 prompt，也不会恢复被其他策略移除的权限。

## 选择要启用的能力

同时启用两个工具：

```json
{
  "continuity": true
}
```

只启用 History：

```json
{
  "continuity": {
    "history": true,
    "notes": false
  }
}
```

只启用 Notes：

```json
{
  "continuity": {
    "history": false,
    "notes": true
  }
}
```

显式关闭两者：

```json
{
  "continuity": false
}
```

完全省略 `continuity` 也会关闭两者。在对象形式中，只有完整合并结果里仍然缺省的字段才
默认为 `false`。

## 把开关放在正确的配置层

如果希望对当前用户长期生效，请编辑下列命令所显示配置根目录中的 `zuno.json`：

```sh
zuno debug paths
```

Unix 上通常是 `${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json`。项目中的
`zuno.json[c]` 与 `.zuno/zuno.json[c]` 也可以设置 `continuity`，并遵循正常的层级顺序。

如果连续性能力需要随着 provider 或团队 profile 一起切换，请使用 `ZUNO_CONFIG_DIR`。
例如在 `$HOME/.config/zuno/profiles/recovery/zuno.json` 中写入：

```json
{
  "continuity": {
    "history": true,
    "notes": true
  }
}
```

然后使用该覆盖层启动 Zuno：

```sh
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/recovery" zuno
```

目前没有单独的 `--continuity` 命令行参数。请使用配置文件、`ZUNO_CONFIG_DIR` 或
`ZUNO_CONFIG_CONTENT`，这样同一项设置才能一致作用于 TUI、headless、ACP 与 server
进程。

对象字段会递归合并。如果较低层已经启用 History，那么较高层只写
`{"continuity":{"notes":true}}` 时，History 仍然保持启用。一个 profile 如果必须关闭
它，就要显式写出 `false`：

```json
{
  "continuity": {
    "history": false,
    "notes": true
  }
}
```

如果只想影响一个进程，而不创建配置文件，可以使用最终的内联层：

```sh
ZUNO_CONFIG_CONTENT='{"continuity":{"history":true,"notes":false}}' \
  zuno debug config
```

PowerShell 写法：

```powershell
$env:ZUNO_CONFIG_CONTENT = '{"continuity":true}'
zuno debug config
Remove-Item Env:ZUNO_CONFIG_CONTENT
```

## ACP 与编辑器 profile

编辑器使用的是它所启动的 `zuno acp` 进程对应的配置。要在 Zed 或其他 ACP 客户端中使用
一个 profile，请通过 Agent server 环境传入同一个覆盖目录：

```json
{
  "command": "zuno",
  "args": ["acp"],
  "env": {
    "ZUNO_CONFIG_DIR": "/config/.config/zuno/profiles/kiro"
  }
}
```

把 `continuity` 对象写进该 profile 的 `zuno.json`。修改配置后，请先重启长期运行的 TUI、
ACP server 或 HTTP server 再验证。正在进行的 provider 请求会保留其不可变工具快照；重启
不会删除持久 session、History 证据、Notes、Plan 或其他工作状态。

## 启用连续性能力不等于最终授权

`continuity` 只贡献候选工具。最终模型可见工具面仍会依次被以下层收窄：

1. 当前 profile 与 Agent 契约；
2. 顶层 `tools` 映射；
3. Agent 的精确 `tools` allowlist；
4. request hook；
5. 生效的 `permission.rules`。

例如，下面会贡献两个连续性候选工具，但隐藏 Notes：

```json
{
  "continuity": true,
  "tools": {
    "notes": false
  }
}
```

下面会保留 Notes 候选工具，但拒绝所有 Notes 调用：

```json
{
  "continuity": true,
  "permission": {
    "rules": {
      "notes": "deny"
    }
  }
}
```

把 `tools.history` 或 `tools.notes` 设为 `true`，无法创建一个已经被 `continuity` 关闭的
provider。同样，`permission.mode: "allow_all"` 也无法恢复被 Agent allowlist 移除的工具，
或覆盖一条显式 `deny`。

Agent 的 `tools` 列表是精确列表，会替换继承结果。自定义 Agent 一旦声明它，就必须在保留
所需其他工具的同时明确加入 `history` 或 `notes`。

## 工具可以看到什么

### History

`history` 提供四个只读 action：

- `list_windows` 列出由成功压缩划分的范围；
- `list_items` 列出一个窗口内的规范化条目；
- `read_item` 读取一个已返回的条目；
- `search_contents` 搜索当前会话中的规范化内容。

它绝不会跨到另一个 session。返回内容不包含 reasoning、加密值、合成的内部提示正文或
二进制附件字节。返回文本是不受信数据，不能当作指令执行。

### Notes

`notes` 提供三个读取 action 与两个写入 action：

- `list_files_by_prefix`、`read_file` 和 `search_contents` 是只读操作；
- `append_to_file` 与 `write_file` 有副作用，绝不会被机械重放。

Notes 使用 `handoff/ci.md` 这样的逻辑斜杠名称，而不是宿主文件系统路径。每个文档都属于
当前 `session_id + Agent`，因此另一个 Agent 或被委派的子 session 不会隐式共享它。

每次写入都必须携带 `expected_revision`。只有新建时使用 `0`：

```json
{
  "action": "write_file",
  "name": "handoff/ci.md",
  "content": "Release candidate is waiting for Windows CI.",
  "expected_revision": 0
}
```

再次修改前先读取文档，并传回读取结果中的 revision。过期 revision 会被拒绝，而不是覆盖
并发工作。每个 session-Agent 作用域最多 100 个文档，单文档最多 256 KiB，总计最多
1 MiB。

以后关闭 Notes 只会隐藏工具，不会删除现有文档。同一 session 与 Agent 再次启用 Notes
时，它们会重新出现。删除 session 或执行破坏性 prune 会移除它们；export/import 会保留，
sanitize export 则会脱敏文档身份与正文。

## Plan 保持独立

连续性能力与宿主持久 Plan 相互独立。下面的配置：

```json
{
  "continuity": true,
  "tools": {
    "plan_update": false
  }
}
```

会隐藏模型侧的 Plan 修改工具，但宿主仍能创建、持久化、投影并恢复 Plan。History 与 Notes
也不会因此变成 Plan 的替代存储。

## 验证最终结果

请在与真实客户端相同的环境下运行：

```sh
zuno debug paths
zuno debug config
zuno debug agent build
zuno debug permissions
```

使用 profile 时：

```sh
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/recovery" zuno debug config
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/recovery" zuno debug agent build
```

`debug config` 用来确认合并后的 `continuity` 值；`debug agent` 显示选定 Agent 是否保留了
工具；`debug permissions` 显示最终拒绝。只有 `history` 或 `notes` 进入最终 provider 可见
工具快照时，prompt 中才会出现 `runtime.continuity` 分段。

如果工具仍然缺失，请依次检查解析出的配置根目录、继承的对象字段、顶层 `tools`、Agent
精确 allowlist、权限规则，以及长期运行的客户端是否已经重启。

## 参见

- [配置文件与优先级](/zh/config/files)
- [工具](/zh/guide/tools)
- [会话与回合](/zh/guide/sessions)
- [配置项参考](/zh/config/reference)
- [编辑器与 ACP](/zh/guide/editors)
