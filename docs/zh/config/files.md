# 配置文件与优先级

Zuno 的主配置只读两个文件名：`zuno.json` 和 `zuno.jsonc`。没有别的。它还会在对应的各层读取 `tui.json` 与 `tui.jsonc` 作为界面设置，以及 `AGENTS.md` 作为指令。

在推理优先级之前，先确认这个可执行文件实际解析出了哪些根目录：

```sh
zuno debug paths
```

## 解析出的根目录

```text
home       /config
data       /config/.local/share/zuno
bin        /config/.cache/zuno/bin
log        /config/.local/share/zuno/log
repos      /config/.local/share/zuno/repos
cache      /config/.cache/zuno
config     /config/.config/zuno
state      /config/.local/state/zuno
tmp        /tmp/zuno
```

| 根目录 | 它保存什么 |
| --- | --- |
| `config` | `zuno.json`、`tui.json`、`AGENTS.md`、Agent、Skill、命令、扩展、profile、主题 |
| `data` | 会话数据库、凭据存储、日志、repos |
| `log` | 结构化的运维日志存储 |
| `cache` | 模型目录缓存与下载的二进制文件 |
| `state` | 既非配置也非持久会话数据的运行时状态 |
| `tmp` | 临时工作文件 |

上面的路径来自一台真实宿主，所以你的会不同。它们遵循 XDG 变量：`config` 是 `$XDG_CONFIG_HOME/zuno`，`data` 是 `$XDG_DATA_HOME/zuno`，`cache` 是 `$XDG_CACHE_HOME/zuno`。

## 层级顺序

优先级最低的在前：

| 顺序 | 层 | 来源 |
| --- | --- | --- |
| 1 | 全局 | `$XDG_CONFIG_HOME/zuno/zuno.json[c]` |
| 2 | 项目遍历 | 从 worktree 根一路向下到当前目录的裸 `zuno.json[c]` |
| 3 | 项目 `.zuno` | 沿同一遍历路径的 `.zuno/` 下的文件 |
| 4 | `ZUNO_CONFIG` | 一个显式文件 |
| 5 | `ZUNO_CONFIG_DIR` | 一个包含 `zuno.json[c]` 的显式目录 |
| 6 | `ZUNO_CONFIG_CONTENT` | 最后一个环境层，以内联方式提供 |

离项目更近的目录排在后面，因此它们胜过 worktree 根。

对象从低优先级到高优先级递归合并。数组与标量值替换较低层的值。顶层拒绝未知键。

## 环境变量覆盖

| 变量 | 效果 |
| --- | --- |
| `ZUNO_CONFIG` | 把一个显式配置文件作为高优先级层加入 |
| `ZUNO_CONFIG_DIR` | 把一个包含 `zuno.json[c]` 的目录作为可切换覆盖层加入 |
| `ZUNO_CONFIG_CONTENT` | 以内联方式提供最后一层，用于受管或临时环境 |

```sh
ZUNO_CONFIG="$HOME/audit/zuno.json" zuno debug config
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

由于没有具名的 `--profile` 标志，`ZUNO_CONFIG_DIR` 就是切换整个团队或 provider 选择的方式。这个覆盖层做深度合并，所以它不必重复 provider 定义；它可以只包含顶层选择：

```json
{
  "model": "kiro-local/claude-opus-5",
  "small_model": "kiro-local/gpt-5.6-luna",
  "preset": "kiro-local"
}
```

## 首次运行的默认值

在第一次常规发现时，Zuno 会把缺失的全局 `zuno.json` 创建为 `{}`，并用它自带的起步指导创建缺失的全局 `AGENTS.md`。创建使用独占的新建文件语义，绝不覆盖这两个文件中的任何一个。

以显式的 `ZUNO_CONFIG`、`ZUNO_CONFIG_DIR` 或 `ZUNO_CONFIG_CONTENT` 启动不会物化默认值。因此，一次安装或一次常规启动应当先于一次仅使用 profile 的首次运行。

## 主配置与 TUI 配置

`theme`、`mouse`、快捷键、提示框尺寸、diff 布局和通知设置都不属于 `zuno.json`。它们属于对应层的 `tui.json` 或 `tui.jsonc`。

```json
{
  "theme": "system",
  "mouse": true,
  "leader_timeout": 5000
}
```

校验错误会指出每一个被拒绝的顶层键，因此把 `theme` 放进 `zuno.json` 会得到一次明确的拒绝，而不是一个莫名其妙毫无作用的设置。参见[主题与快捷键](/zh/config/theming)。

## 沙箱权限取决于来源

不是每一层都可以选择任意沙箱模式，这也是唯一一处优先级并非全部答案的地方。

| 层 | 沙箱权限 |
| --- | --- |
| 受信的全局、显式配置、受管、环境、CLI | 可以选择任意模式 |
| 项目 `zuno.json[c]` 与 `.zuno` | 只能收窄为 `read-only`、拒绝网络、添加受保护路径，或把 `onUnavailable` 设为 `deny` |

项目层无法选择更宽的模式、授予宿主网络、添加外部可写根目录，也不能启用
`run-unconfined`。因此一份被检入仓库的配置无法提升自己的约束级别，正是这个性质让克隆一个陌生仓库变得安全。

当确实想要更宽的模式时，使用一次受信的单次调用覆盖：

```sh
zuno --sandbox read-only
zuno --sandbox danger-full-access
zuno --sandbox workspace-write --sandbox-on-unavailable run-unconfined
```

仅在后端不可用时降级的环境变量写法是
`ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`。受管策略的优先级更靠后，仍可收窄这些覆盖，
或把不可用动作强制改回 `deny`。参见[权限与沙箱](/zh/guide/permissions)。

## 其他资产在哪里被发现

| 资产 | 位置 |
| --- | --- |
| Agent | 全局配置根与每个项目 `.zuno` 之下的 `agent/*.md` 与 `agents/*.md` |
| 命令 | 同样这些根目录下递归查找的 `command/**/*.md` 与 `commands/**/*.md` |
| Skill | 项目 `.zuno/skill(s)`、项目 `.agents/skills` 然后 `.claude/skills`、全局配置根、`~/.agents/skills` 然后 `~/.claude/skills`、`skills.paths`、远端索引 |
| 扩展 | 项目使用 `.zuno/extensions`；全局配置根之下的 `extensions` |

对以上任何一项，Zuno 都绝不扫描 `.opencode` 或 OpenCode 的配置目录。

## 这个构建打开哪个数据库

会话数据库的文件名取决于构建 channel，这是会话列表看起来为空最常见的原因：

| 条件 | 文件 |
| --- | --- |
| `ZUNO_DB` 为 `:memory:` | 内存中 |
| `ZUNO_DB` 是绝对路径 | 原样使用该路径 |
| `ZUNO_DB` 是相对路径 | 拼接到数据目录之下，而不是工作目录 |
| channel 为 `latest`、`beta` 或 `prod`，或 `ZUNO_DISABLE_CHANNEL_DB` 恰好为 `1` 或 `true` | `zuno.db` |
| 其他情况 | `zuno-<channel>.db` |

源码构建没有 channel define，因此它解析为 `zuno-local.db`。参见[数据库生命周期](/zh/operate/migration)。

## 参见

- [配置总览](/zh/config/)
- [变量与替换](/zh/config/variables)
- [指令与 AGENTS.md](/zh/config/instructions)
- [配置项参考](/zh/config/reference)
