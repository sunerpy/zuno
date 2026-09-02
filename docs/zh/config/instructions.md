# 指令与 AGENTS.md

指令文件会被注入每一条提示词，这正是 Zuno 的发现机制窄而有序、而不是图方便的原因。隐式文件名恰好只有两个，`AGENTS.md` 和 `AGENTS.local.md`，再加上一个显式的配置数组。Zuno 不读 `CLAUDE.md`、`CONTEXT.md`、`.opencode` 或任何其他产品的指令文件，并且永远不会隐式地读：一个静默进入每条提示词、却不出现在任何有文档记录的清单里的文件，是无法审计的。

## 三种机制，刻意分开

| 机制 | 来源 | 加载时机 |
| --- | --- | --- |
| 全局指令 | `$XDG_CONFIG_HOME/zuno/AGENTS.md` | 始终 |
| 项目指令 | 从 worktree 根到当前目录，每个目录的 `AGENTS.local.md` 或 `AGENTS.md` | 始终 |
| 配置的指令 | `zuno.json` 中的 `instructions` 数组 | 始终 |
| 就近指令 | 从会话中途读取的某个文件向上遍历 | 按需 |

就近发现是最容易让人意外的那一个。当模型在会话进行到一半时读取某个文件，Zuno 会从该文件向上遍历，并附加尚未计入的指令文件。每个规范化文件只计一次：在附加发生之前，系统集合、会话中已读取的路径，以及当前消息声明的内容都会被查阅。

## 顺序与优先级

Zuno 按这个顺序加载原生指令文件：

1. `$XDG_CONFIG_HOME/zuno/AGENTS.md`；
2. `ZUNO_CONFIG_DIR/AGENTS.md`，当某个 profile 目录提供了它时；
3. 从 worktree 根一路向下到当前目录的项目目录。

靠后的条目追加得更晚，因此更近的目录具有更高优先级。在同一个目录内，`AGENTS.local.md` 替换 `AGENTS.md`，而不是与之合并，这让 `AGENTS.local.md` 成为存放机器特有或不提交规则的正确位置。

由 `ZUNO_CONFIG_DIR` 选中的 profile 文件不会替换基础的全局文件。它追加更窄、优先级更高的指导。当某个 profile 切换 provider 或团队时这一点很重要：基础规则仍然生效，因此一个 profile 只需说明它的差异。

```text
~/.config/zuno/AGENTS.md          global, lowest priority
$ZUNO_CONFIG_DIR/AGENTS.md        profile overlay
<worktree>/AGENTS.md              project root
<worktree>/crates/AGENTS.md       narrower
<worktree>/crates/x/AGENTS.local.md   narrowest, replaces AGENTS.md here
```

## `instructions` 键

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `instructions` | `string[]` \| `null` | 无 | 额外的指令文件或 glob 模式 |

```json
{
  "instructions": [
    "docs/house-style.md",
    ".zuno/rules/*.md",
    "https://example.com/org-policy.md"
  ]
}
```

条目可以是路径或 glob 模式，也接受远端 URL。这个数组是一个独立的显式来源；它既不替换隐式的层叠机制，也不改变后者的优先级。

请记住，数组在各配置层之间是替换而不是合并。一个设置了 `instructions` 的项目 `zuno.json` 会完全丢弃全局数组。合并规则见[配置文件与优先级](/zh/config/files)。

一条远端指令如果挂起、返回 404 或 DNS 失败，会产生一条警告并从结果中被丢弃。它绝不会让加载失败，因为配置文件里一个不稳定的 URL 不该让 Agent 变得不可用。本地读取与远端抓取都是有界且并发的，并有按 URL 的超时。

## 什么情况下规则文件会中止本轮

规则文件要么整份进入 Prompt，要么完全不进入。Zuno 绝不截断：规则被截断后含义会改变，
“除非 Y，否则做 X”在“做 X”之后被切断就是另一条指令，而用户仍以为原始规则生效。

以下两种情况会在第一次 provider request 之前让本轮失败，并点名文件与修复方式，超预算时还给出字节数与预算：

- 文件存在但无法读取，例如权限错误或字节不是合法 UTF-8；
- 文件超出 instruction prompt 预算，该预算取 64 KB 与模型 context window 四分之一
  两者中较小的值。

两者都不是警告。继续发出请求等于让模型在从未收到的规则下工作，它的回答看起来很自信，
错误原因却在 transcript 里完全看不到。远端抓取失败是上文记录的例外：宿主报告哪些规则
本轮不生效，然后继续执行，因为网络不该决定 Agent 能否运行。

预算按模型计算，因此同一个文件可能被大窗口模型接受、被小窗口模型拒绝。拒绝信息会说明
字节数和预算，修复方式要么是缩短文件，要么换一个窗口更大的模型。

## 首次运行

在第一次常规发现时，Zuno 会用独占的新建文件语义，从它自带的起步指导创建缺失的全局 `AGENTS.md`。已存在的文件绝不会被覆盖。这份起步内容覆盖归属、验证、范围受限的 Git 操作，以及安全的 worktree 决策；详细流程留在内置的 `git-workflow` 与 `worktree` Skill 中，以便它们只在相关时加载。

以显式的 `ZUNO_CONFIG`、`ZUNO_CONFIG_DIR` 或 `ZUNO_CONFIG_CONTENT` 启动不会物化默认值。如果你想要那份起步文件，请先做一次常规启动。

## 什么内容适合放进指令文件

指令是无条件的：它们在每一次请求、每一个会话、每一个 Agent 上都要消耗提示词预算。只在某些时候适用的内容应当放进 Skill，Skill 会在匹配时才加载。这条边界见[编写 Skill](/zh/config/authoring-skills)。

好的候选是项目不变量 —— 构建命令、归属边界、评审要求、语言约定。不好的候选是长流程、参考表格，以及任何单个任务偶尔才需要的东西。

## 验证模型实际收到了什么

指令内容对模型可见，因此它作为提示词的一部分被持久记录：

```sh
zuno debug prompt --show-sensitive
```

`--show-sensitive` 会原样打印指令、AGENTS、skill 和记忆内容。在把这份输出粘进工单之前，请把它当作敏感信息对待。不带这个标志时，提示词分段仍会被列出，通常已足够回答「这个文件进去了吗」。

要先确认这个可执行文件解析出了哪些根目录：

```sh
zuno debug paths
```

## 参见

- [配置文件与优先级](/zh/config/files)
- [编写 Skill](/zh/config/authoring-skills)
- [诊断](/zh/operate/diagnostics)
- [配置项参考](/zh/config/reference)
