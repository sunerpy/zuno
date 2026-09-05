# Skill

Skill 是带身份的可复用指令：一个名字、一段用于触发匹配的描述、一段 Markdown 正文，以及可选的随包脚本、参考资料与素材。Skill 回答的是「这个项目希望这类工作怎么做」，而不必让这份指导出现在每一条提示词里。

Skill 不授予任何东西。它不会增加工具、权限、文件系统访问、网络访问或环境访问。运行时能力快照始终是权威，所以一个描述了某工作流的 Skill，并不因此授权该工作流。

## 为什么不直接写进提示词

两个原因。

全量加载代价高：只适用于某一类工作的指令会在每个回合都消耗上下文。渐进式披露解决了这一点。提示词只收到一份有界的初始索引，较大的可发现目录通过搜索按需提供，正文只会在选中之后加载。

第二个原因是身份。Skill 有来源，因此来自两个根目录的同名 Skill 仍可各自独立寻址，不会有一个隐藏的优先级胜出者被选中。这正是为什么一个项目 Skill 与一个同名的全局 Skill 会成为可见的歧义，而不是一个无声的意外。

## 发现顺序

Zuno 按这个作用域顺序发现 Skill：

1. 项目 `.zuno/skill` 根目录，从当前目录一路到 worktree；
2. 项目 `.agents/skills` 根目录，沿同一路径遍历；
3. `$XDG_CONFIG_HOME/zuno/skill`（通常是 `~/.config/zuno/skill`），以及显式设置
   `ZUNO_CONFIG_DIR` 时的 `ZUNO_CONFIG_DIR/skill`；
4. 全局 `~/.agents/skills`；
5. 显式的 `skills.paths`；
6. 已配置的远端索引。

因此项目作用域会先于用户全局作用域被公布。Zuno 不会隐式扫描 `.claude`、
`.opencode` 或其他产品的配置目录；确实需要共享时，仍可通过 `skills.paths`
显式选择该目录。同一个规范化来源路径会被去重，包括符号链接别名。

Zuno 不再隐式扫描 `~/.zuno`、`~/.config/zuno/skills` 或项目
`.zuno/skills`。任何非规范目录都应通过 `skills.paths` 显式添加。

```sh
ZUNO_DISABLE_EXTERNAL_SKILLS=1 zuno
```

这个开关禁用隐式 `.agents` 根目录；Zuno 原生 `.zuno` 根目录、已配置的 Zuno
根目录与显式 `skills.paths` 仍然启用。

## 运行中变更

一个会话会让 Prompt 发现、required Skill、`skill` 工具、斜杠命令、TUI 与 ACP
共同读取一份原子发布的 Skill catalog generation。在已经生效的根目录中安装、修改、
删除或重命名 Skill 后，无需重启会话即可识别。损坏的修改会保留上一份有效条目并公布
warning，直到文件修复。

规范用户根目录和显式配置路径即使在会话启动时尚不存在也受支持。Zuno 只以非递归
方式监听其最近的已有父目录，在目录出现后逐级收窄订阅，并且只在到达精确配置根目录
后开启递归监听。共享的 `~/.agents/skills` 只在启动时已经存在时自动监听；若需要运行中
新建，请在启动前创建，或通过 `skills.paths` 显式选择。

远端下载缓存属于私有状态。只有 `skills.urls` 确实下载文件时才会按需创建，而且它不会
成为文件系统监听根目录。

往这个缓存里放什么由 `skills.urls` 的索引决定，而索引是远端输入，所以 Zuno 会限制它能
点名的东西。索引条目的 `name` 必须是单个目录段：绝对路径的名字、含 `..` 的名字，以及含
路径分隔符的名字，都会被跳过并给出 warning，不会为它们下载任何东西。这条规则之所以重要，
是因为带 `version` 的条目在刷新时会把下载内容暂存到目标目录旁边，随后把该目录当前的内容
改名挪走并删掉 —— 若不校验名字，这套动作原本可能对准你自己的 Skill 目录，而不是缓存。
参见[远端 Skill 索引](/zh/config/authoring-skills#远端-skill-索引)。

若调用方在重命名后仍尝试加载旧的精确来源，Zuno 会强制刷新一次，然后返回
`CatalogStale` 和当前精确 locator；不会在同名来源之间猜测。

## 一个 Skill 如何到达模型

提示词拿到的是目录，不是正文：

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000,
    "config": [
      {
        "path": "~/.config/zuno/skill/powerapps",
        "recursive": true,
        "exposure": "search"
      }
    ]
  }
}
```

`maxContextTokens` 以近似 Token 显式限定紧凑目录的规模。未配置时，Zuno
使用等于已知模型窗口数值约 2% 的字符预算；上下文未知时回退为约 8,000 个
字符，而不是固定限制为 2,000 个字符。显式覆盖项会换算为字符，并最多按
10,000 个 Token 计算。
唯一名称不会在提示词索引中重复注入绝对来源路径；只有同名歧义项才携带
`source` 定位符。

每个已启用 Skill 都有一种目录暴露方式：

| 暴露方式 | 初始索引 | `skill search` / `list` | 精确加载、`$name`、`/<name>`、`requiredSkills` |
| --- | --- | --- | --- |
| `index` | 是 | 是 | 是 |
| `search` | 否 | 是 | 是 |
| `explicit` | 否 | 否 | 是 |

默认值是 `index`。大型供应商或领域包适合设为 `search`：仍然可以搜索，但不占用每次初始提示词。必须避免能力匹配自动选中的指导应设为 `explicit`。模型会知道存在多少个仅搜索来源，但显式来源的名称会刻意完全隐藏。

被完整选中的正文共用另一份聚合预算。它的默认值是已知上下文的百分之十，下限 2,000 个 Token，上限 32,000 个。`maxSelectedContextTokens` 可以覆盖它，但仍受 32,000 的上限约束。如果被选中的正文装不下，加载或恢复会话会在 provider 请求之前失败，而不是静默丢弃指令。

`includeInstructions: false` 会同时把触发策略和初始索引从提示词中移除。`skill` 工具仍然支持对 `index` 与 `search` 条目的分页 `list` 与 `search`。

因此即使个人 Skill 库很大，也不会把每个 `SKILL.md` 正文注入每次请求。若某个
供应商或领域包很少使用，可以让它继续留在普通发现根目录，只用一条递归
`skills.config` 规则把它设为仅搜索；不要仅为了减少数量而删除或合并语义不同的 Skill。

精确的 `skills.config` 条目还可以设置 `"enabled": false`。路径可以指向 Skill
目录或它的 `SKILL.md`；`"recursive": true` 会作用于所有后代。条目按顺序求值，
最后一个匹配项获胜，因此较后的精确条目可以重新启用或重新分类一个被更宽子树
规则覆盖的 Skill。已存在路径会规范化，包括符号链接别名。

## 加载是分页的，且必须读完

`load` 与 `read_resource` 返回与内容绑定的续读游标。调用方必须一路读到 `complete: true` 才能应用这些指令，因为一份不完整的 `SKILL.md` 不是可用的指导。这是刻意的：半套流程往往比没有更糟。

## 直接调用一个 Skill

一个不含歧义、且不与真实命令冲突的 Skill 可以用 `/<skill-name>` 调用。Zuno 会解析那个确切公布的来源，并在下一次 provider 请求之前加载它的正文。

来自多个来源的同名 Skill 会刻意禁用这种有歧义的斜杠形式。请使用 Skill 选择器，或者使用带确切来源的类型化 `skill` 工具。

原生会话命令在 Markdown command 和 Skill 之前解析，因此用户工作流无法遮蔽 `/compact` 或 `/plan` 这样的运行时控制命令。

## 内置 Skill

Zuno 把十一个第一方 Skill 编译进 `zuno-orchestration` 包：`customize-zuno`、`develop-zuno`、`deepwork`、`codemap`、`verification-planning`、`reflect`、`worktree`、`git-workflow`、`github-delivery`、`ui-design` 和 `bedrock-model-capability-review`。

每一个都有稳定的 `builtin://zuno-orchestration/...` 来源、内容哈希、来源溯源、允许的 Agent profile，以及所需工具声明。它们被编译进可执行文件，不会复制到你的配置目录，因此随二进制一起更新。把其中一个复制到用户 Skill 目录来「覆盖」它，只会造成同名来源歧义。

当前 profile 及其声明的工具可见性会过滤公布出来的集合。选择一个 Skill 永远无法扩大运行时能力快照。

内置 Skill 的触发描述刻意比正文更窄。`git-workflow` 只在 staging、提交、分支、worktree、交付移交或保护脏仓库等 Git 决策确实重要时加载；一次隔离的临时 fixture 或普通未提交编辑不会触发它。`verification-planning` 用于高风险、跨表面、发布、迁移、安全，或用户明确要求设计验证证据的工作；验收命令已经清楚的有界改动直接遵守运行时验证契约，不再加载第二套工作流。

## 委派回合中的 Skill

每个初始或恢复的子级宿主都独立执行发现。父级已加载的正文不会被复制进子级提示词。

当某个子级角色必须始终收到特定指令集时，请显式声明：

```json
{
  "agents": {
    "explorer": {
      "requiredSkills": ["codegraph"]
    }
  }
}
```

在 profile 与 Agent 过滤之后，每个名字都必须恰好解析到一个可见来源。名字缺失或同名来源存在歧义会让子级启动失败，而不是挑一个隐藏的胜出者。

请仔细注意这条边界：它保证子级收到 CodeGraph 的*指令*，而不是 CodeGraph 的*工具*。工具仍然需要父级 Attempt 的 schema、角色继承或一次确切授予、在 Agent 允许列表中存活，并且没有显式拒绝。

## 检查发现结果

```sh
zuno debug skill
zuno debug agent explorer
```

`debug skill` 报告原始发现结果：来自不同来源的同名条目会被保留，摘要会报告来源数、初始索引数、可搜索数、仅显式数、已禁用数与唯一数，以及存在歧义的名字。`debug agent` 报告经 Agent 过滤的视图，含预算与覆盖情况。

## 参见

- [编写 Skill](/zh/config/authoring-skills)
- [Workflow 与命令](/zh/config/workflows)
- [Agent](/zh/guide/agents)
- [配置项参考](/zh/config/reference)
