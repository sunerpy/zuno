# 主题与快捷键

终端应用读取它自己的配置文件，与 Zuno 的主配置分开。本页讲这个文件：它在哪里、它的各层如何合并，以及它接受的每一个键。

## 为什么这是一个单独的文件

`theme`、`keybinds` 以及下面其他这些键刻意不出现在 `zuno.json` 中。它们配置的是一个客户端 —— 终端应用 —— 而不是 Agent 运行时，因此无界面运行、ACP server 和 HTTP server 永远不会读它们。把它们放在 `tui.json` 里，意味着一个快捷键设置无法影响脚本化的 `zuno run` 的行为。

值得记住的后果是：如果你把 `theme` 放进 `zuno.json`，这不是错误，但它也没有任何作用。

## 文件位置

这个文件是 `tui.json` 或 `tui.jsonc`，按这个顺序被发现。**靠后的路径胜出。**

| 层 | 路径 |
| --- | --- |
| 全局 | `~/.config/zuno/tui.json` |
| 项目，从工作目录向上遍历 | `.zuno/tui.json` |

`ZUNO_TUI_CONFIG` 会覆盖被发现的这组文件。

文件缺失不是错误；它只是一个什么都不贡献的层。

### 合并是按键进行的，不是按文件

这是让人意外的部分。靠后的层不会整体替换靠前的层 —— 它只覆盖它实际设置了的那些键：

```json
// ~/.config/zuno/tui.json
{
  "theme": "system",
  "keybinds": { "session_new": "ctrl+n" }
}
```

```json
// .zuno/tui.json in one project
{
  "theme": "gruvbox"
}
```

项目文件改变了主题，并且**让 `session_new` 的绑定保持不变**。嵌套对象以同样方式合并，因此一个只设置了 `prompt.max_height` 的文件不会抹掉 `prompt.max_width`。

## 键

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `$schema` | string | — | Schema 引用。接受但不使用。 |
| `theme` | string | 内置默认值 | 主题名，或用 `system` 从终端自身的配色推导一套调色板。 |
| `keybinds` | object | — | 逐动作的覆盖，以动作名为键。 |
| `leader_timeout` | number | — | Leader 键超时，单位毫秒。 |
| `prompt` | object | — | 提示框尺寸设置，包括 `max_height` 与 `max_width`。 |
| `scroll_speed` | number | — | 每个滚轮格滚动的行数。 |
| `scroll_acceleration` | object | — | 滚动加速设置。 |
| `diff_style` | string | — | Diff 渲染风格。 |
| `mouse` | boolean | `false` | 应用层鼠标处理。 |
| `attention` | object | 全部关闭 | 通知与声音提示设置。 |

### `theme`

任何主题层提供的名称，外加特殊值 `system`。

```json
{
  "theme": "system"
}
```

一个没有任何层提供的名称**不是**错误。主题注册表会回退到默认主题，并报告一条指出它找不到哪个主题的诊断，因此拼写错误表现为一条消息，而不是启动失败。

### `mouse`

默认禁用，其中的理由值得说明：在应用层鼠标处理关闭时，终端会在对话记录、侧边栏、提示框、对话框和通知上保留它自己的拖动选择与复制行为。打开它就是用这些换来分段的点击折叠与滚轮滚动。

```json
{
  "mouse": true
}
```

### `keybinds`

以动作名为键。取值要么是单个绑定，要么是一个携带多种写法的对象。

```json
{
  "keybinds": {
    "session_new": "ctrl+n",
    "input_paste": { "key": "ctrl+v", "alt": "cmd+v" }
  },
  "leader_timeout": 800
}
```

绑定内部无法识别的**按键名**会被接受并忽略。无法识别的**动作名**会被报告，因为一个你以为设置好、却静默毫无作用的快捷键比一条诊断更糟。

## 检查结果

```sh
zuno debug config
```

它打印解析后的配置，因此某一层没有按你预期合并时，这一点是可见的，而不需要靠推断。

## 参见

- [终端应用](/zh/guide/tui) —— 这些键所配置的界面
- [配置文件与优先级](/zh/config/files) —— 主配置栈
- [zuno tui](/zh/cli/tui) —— 命令行选项
- [zuno debug](/zh/cli/debug) —— `debug config` 与其他自省子命令
