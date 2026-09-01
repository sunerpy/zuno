# 配置总览

Zuno 读取 `zuno.json` 与 `zuno.jsonc`。所有可以告诉运行时的东西，都是这份文档中经过校验的字段，再加上少数几个各自负责自身关注点的文件：`tui.json` 负责界面设置，`AGENTS.md` 负责指令，以及扩展与 Skill 的包目录。

让其余部分变得可预测的心智模型是：**配置声明的是上限，而更窄的层只能削减它们。**Agent 契约收窄沙箱模式。项目层收窄沙箱权限。一条权限 `allow` 绝不会恢复被更窄层移除的能力。

## 各类内容的位置

| 内容 | 文件 | 页面 |
| --- | --- | --- |
| 运行时、provider、Agent、权限、沙箱 | `zuno.json` / `zuno.jsonc` | [配置文件与优先级](/zh/config/files) |
| 主题、快捷键、鼠标、提示框尺寸、diff 布局、通知 | `tui.json` / `tui.jsonc` | [主题与快捷键](/zh/config/theming) |
| 注入每条提示词的指令 | `AGENTS.md`、`AGENTS.local.md` | [指令与 AGENTS.md](/zh/config/instructions) |
| 带自身身份的可复用指导 | Skill 目录中的 `SKILL.md` | [编写 Skill](/zh/config/authoring-skills) |
| 提示词模板与参数宏 | `command/**/*.md` | [Workflow 与命令](/zh/config/workflows) |
| 以 Markdown 定义的 Agent | `agent/*.md`、`agents/*.md` | [自定义 Agent](/zh/config/custom-agents) |
| 扩展包 | 包目录中的 `extension.json` | [插件](/zh/guide/plugins) |

把 `theme` 这样的 TUI 键放进 `zuno.json` 会被拒绝，而不是被忽略，并且校验错误会指出那个被拒绝的键。`zuno.json` 的顶层完全不接受未知键，这是刻意的：一个静默无效的拼写错误比一次拒绝更糟。

## 合并语义

对象从低优先级到高优先级递归合并。数组与标量替换较低层的值。

这一条规则就解释了大多数意外。数组不是追加的，因此更高层中某个 Agent 的 `tools` 列表会替换较低层的列表，而不是扩展它，`delegates`、`requiredSkills`、`writableRoots` 和 `instructions` 同理。

## 顶层结构

一共有四十一个键。按它们决定什么来分组：

| 分组 | 键 |
| --- | --- |
| 模型路由 | `model`、`small_model`、`preset`、`presets`、`provider`、`enabled_providers`、`disabled_providers` |
| Agent 与委派 | `agents`、`default_agent`、`subagent_depth`、`subagent_model_selection`、`workflows`、`productAgent` |
| 权限 | `permission`、`sandbox`、`shell` |
| 指令、Skill 与学习 | `instructions`、`skills`、`command`、`memory`、`learning` |
| 上下文 | `compaction`、`tool_output`、`attachment`、`references` |
| 集成 | `mcp`、`lsp`、`formatter`、`web_search`、`watcher` |
| 运行时 | `concurrency`、`goal`、`snapshot`、`tools`、`logLevel` |
| 部署 | `server`、`share`、`autoupdate`、`enterprise`、`experimental` |
| 展示 | `username`、`$schema` |

逐键参考是[配置项参考](/zh/config/reference)。本页及其同级页面解释各个分组；对单个字段而言，那一页才是权威。

## 一份可用的最小配置

```json
{
  "$schema": "https://raw.githubusercontent.com/sunerpy/zuno/main/schemas/zuno.json",
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY"],
      "options": { "baseURL": "https://gateway.example.com/v1" },
      "models": {
        "primary-model": {
          "name": "Primary model",
          "reasoning": true,
          "tool_call": true,
          "limit": { "context": 200000, "output": 32000 }
        },
        "fast-model": {
          "name": "Fast model",
          "tool_call": true,
          "limit": { "context": 128000, "output": 16000 }
        }
      }
    }
  },
  "permission": {
    "mode": "standard",
    "rules": { "shell": "ask" }
  },
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "deny"
  }
}
```

没有默认模型 id。Zuno 不附带隐藏的 provider 默认值，因此一份没有可达路由的配置会产生一条可见的路由诊断，而不是静默地选一个。

## 选择无沙箱行为

默认仍然失败即拒绝：`workspace-write` 加 `onUnavailable: "deny"` 要求请求的约束后端
确实可部署。

如果要始终使用宿主原生进程后端，请选择显式模式：

```json
{
  "sandbox": {
    "mode": "danger-full-access"
  }
}
```

如果要优先使用沙箱，但允许具备写能力的 `workspace-write` Agent 仅在后端发生符合条件的
类型化不可用错误时继续：

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "run-unconfined"
  }
}
```

只有受信的全局、显式配置、环境、CLI 或受管层可以设置 `run-unconfined`。项目配置不能
启用它，只读 Agent 永远不会使用它，受管策略也可以把它强制改回 `deny`。精确的降级边界见
[权限与沙箱](/zh/guide/permissions)。

## 编辑时的 schema 校验

规范 schema 由反序列化配置的那套 Rust 类型生成，因此它无法与二进制实际接受的内容产生漂移：

```json
{
  "$schema": "./schemas/zuno.json"
}
```

当配置文件与 schema 不在同一棵目录树中时，请使用编辑器的 schema 关联或绝对文件 URI。

## 检查结果

绝不要推断合并结果。把它打印出来：

```sh
zuno debug paths
zuno debug config
zuno debug permissions
zuno debug agent build
zuno debug sandbox --mode workspace-write --check
```

`debug paths` 显示这个可执行文件解析出的各个根目录，当一次编辑看起来毫无效果时，这是第一件要检查的事。`debug config` 显示合并后的文档。`debug permissions` 同时报告配置的与生效的模式，当涉及 Agent 契约或 `danger-full-access` 时，两者会不同。

## 整套配置的切换

没有 `--profile` 标志。请把 `ZUNO_CONFIG_DIR` 当作最后一个更高优先级的覆盖目录使用：

```sh
zuno

ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

这个覆盖层是深度合并，因此在其中定义的 provider 是被添加进来，而不是替换全局目录。顶层 `model`、`small_model` 以及非 null 的 `preset` 会替换它们较低层的值。不要把 `"preset": null` 当作墓碑标记：可选的类型化字段把 JSON null 视为「没有更高层的值」，因此继承来的 preset 仍会保持被选中。请改为在覆盖层里显式选择一个 preset。

## 参见

- [配置文件与优先级](/zh/config/files)
- [配置项参考](/zh/config/reference)
- [变量与替换](/zh/config/variables)
- [模型路由](/zh/config/models)
