# 模型路由

Zuno 不自带任何默认模型 id。一份没有可达路由的配置会产生一条可见的路由诊断，而不是静默地选一个 —— 在模型之间的成本与能力差异如此之大时，这是唯一诚实的行为。

路由有三层：一个会话默认模型、一个用于廉价副任务的模型，以及可选的具名团队 preset，用于路由单个 Agent 和 workflow 类别。

## 两个顶层模型键

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `model` | `string` \| `null` | 无 | 默认模型，`provider/model` 形式 |
| `small_model` | `string` \| `null` | 无 | 用于廉价副任务（例如生成标题）的模型 |

```json
{
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model"
}
```

两者都是带限定的 `provider/model` id。provider 段是 `provider` 映射中的键，model 段是该 provider 的 `models` 映射中的键 —— 不是厂商的市场名称。这就是为什么一个网关可以在一个 provider id 之下暴露多个上游厂商。

`small_model` 存在的原因是：标题生成、摘要和类似的副任务运行频繁，而且并不因用上前沿模型而受益。不设置它并不会禁用这些任务；只意味着它们使用与其他一切相同的路由。

两个键都是标量，因此更高优先级的配置层会直接替换较低层的值。

## Preset：为整个团队做路由

Preset 是一条类型化的团队级模型路由。它为某个 Agent 或某个语义 workflow 类别选择一个模型和一个可选的 provider 中性推理级别。它不创建 Agent、不授予工具、不改变权限，也不授权委派。

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `preset` | `string` \| `null` | 无 | 当前激活的团队模型路由 preset |
| `presets` | map of preset \| `null` | 无 | 具名的团队模型路由 preset |

每个 preset 体恰好有两个可选字段：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `agents` | map of model choice | `{}` | 逐 Agent 的模型选择 |
| `categories` | map of model choice | `{}` | 语义类别的模型选择 |

Agent 路由驱动直接与被委派的 Agent 选择。类别是给那些不应硬编码 Agent 名称的 workflow 节点使用的语义简写。

一个模型选择要么是一个裸的 `provider/model` 字符串，要么是一个对象：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `model` | `string` | 必填 | `provider/model` 形式的模型 |
| `reasoning` | `off` \| `low` \| `medium` \| `high` \| `xhigh` \| `max` \| `null` | 无 | 这条路由的 provider 中性推理级别 |

```json
{
  "preset": "house",
  "presets": {
    "house": {
      "agents": {
        "orchestrator": { "model": "myopenai/primary-model", "reasoning": "max" },
        "deep": { "model": "myopenai/primary-model", "reasoning": "high" },
        "explorer": "myopenai/fast-model"
      },
      "categories": {
        "cheap": "myopenai/fast-model"
      }
    }
  }
}
```

每个 preset 体都必须使用显式的 `agents` 与 `categories` 对象。扁平的兼容写法，以及 preset 内部 provider 特有的 `variant` 字段，都会被拒绝而不是被忽略。一个裸字符串不改变推理设置。

## 哪条路由胜出

对于指定了 Agent 的直接 `task` 委派，优先级是：

1. `agents.<target>.model`；
2. 当前激活 preset 的 Agent 路由；
3. 父会话模型。

对于宿主拥有的 workflow 与 Council 类别路由：

1. 当前激活 preset 的类别路由；
2. 父会话模型。

`general` Agent 的路由刻意不参与类别路由的解析，这样一条宽泛的 Agent 路由就无法悄悄接管 workflow 节点。面向模型的委派工具不接受 `model`、`effort` 或 `category` 覆盖；路由是一个配置决策，不是逐次调用的决策。

推理设置来自胜出的那条路由，然后是模型或 provider 的默认值。被选中的 preset 会随回合计划一起冻结，因此编辑配置无法改变一次正在进行的尝试。

## 推理级别与 variant

六个规范推理级别是 `off`、`low`、`medium`、`high`、`xhigh` 和 `max`。它们是 provider 中性的名称，由 Zuno 映射到所选模型实际暴露的能力上，并且只在该 Agent 已配置模型的情况下生效。

Variant 是模型自身的具名选项集，在 provider 目录中声明：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `provider.<id>.models.<id>.variants` | map of variant \| `null` | 无 | 具名 variant |
| `provider.<id>.models.<id>.variants.<name>.disabled` | `boolean` \| `null` | 无 | 为该模型禁用这个 variant |

对一次无界面调用，`zuno run --variant <name>` 会用模型声明的确切 variant 覆盖已配置的推理设置。规范名称只有在所选模型声明了它们、或该模型在没有具名 variant 目录的情况下暴露通用推理能力时才被接受。非规范名称会复制该 variant 完整的 provider 选项对象，因此一个只声明了 `deliberate` 的模型不会静默获得规范级别。未知名称会在 HTTP I/O 之前失败，并列出可用项。

`zuno run --thinking` 请求宿主在可用时选择 `high`，否则选择声明中最强的非 `off` 规范级别。对不具备推理能力的模型，以及对语义无法推断的仅具名自定义 variant 目录，它会失败。`--thinking` 与 `--variant` 互斥。当确切的推理强度很重要时，优先使用 `--variant max` 或 `--variant xhigh`。

## 逐模型的目录字段

一个模型条目描述能力，以便运行时在发出请求之前就拒绝一个不可能的请求。常设置的字段：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `name` | `string` \| `null` | 无 | 显示名 |
| `id` | `string` \| `null` | 映射的键 | 覆盖模型 id |
| `reasoning` | `boolean` \| `null` | 无 | 该模型是否进行推理 |
| `tool_call` | `boolean` \| `null` | 无 | 该模型是否能调用工具 |
| `attachment` | `boolean` \| `null` | 无 | 该模型是否接受附件 |
| `temperature` | `boolean` \| `null` | 无 | 该模型是否遵循 `temperature` |
| `limit` | object \| `null` | 无 | Token 上限 |
| `cost` | object \| `null` | 无 | 定价 |
| `modalities` | object \| `null` | 无 | 输入与输出模态 |
| `family` | `string` \| `null` | 无 | 模型家族 |
| `status` | enum \| `null` | 无 | 生命周期状态 |
| `experimental` | `boolean` \| `null` | 无 | 该模型是否为实验性 |
| `release_date` | `string` \| `null` | 无 | 发布日期 |
| `interleaved` | `boolean` \| `string` \| object \| `null` | 无 | 交错推理配置 |
| `headers` | object of `string` \| `null` | 无 | 向该模型发起请求时附加的额外头 |
| `options` | object \| `null` | 无 | 交给 provider SDK 的模型选项 |
| `provider` | object \| `null` | 无 | 支撑该模型的原生传输方式与 API 端点 |
| `variants` | map \| `null` | 无 | 具名 variant |

`limit.context` 是另外几项预算的推导依据 —— Skill 目录预算与被选中正文预算都是已知上下文的百分比。省略它意味着那些预算改用固定的近似值。

## 运行时切换团队

在 TUI 中，`/preset` 打开已配置的选择器，`/preset <name>` 直接选中一个。替换在当前 TUI 内部准备并应用；它不会重启界面，也不会中断正在进行的回合。

一次 preset 切换会清除先前手动设置的模型与推理覆盖，使所选团队的路由生效。之后一次显式的模型或推理选择会为顶层 Agent 覆盖那条团队路由，同时 preset 继续为各次委派做路由。那次选择是会话本地的运行时状态 —— 要让某个团队成为启动默认值，请设置顶层 `preset` 键。

不要在覆盖层里把 `"preset": null` 当作墓碑标记。可选的类型化字段把 JSON null 视为「没有更高层的值」，因此继承来的 preset 仍会保持被选中。请改为在覆盖层里指定一个显式的 preset。

## 验证一条路由

```sh
zuno models myopenai --verbose
zuno debug config
zuno debug agent build
```

`zuno models` 列出目录实际解析出的内容，包括某个模型是否声明了推理能力与 variant。`debug agent` 显示单个 Agent 的生效路由，一次意外的 preset 交互就是在这里变得可见的。

## 参见

- [认证与凭据](/zh/config/authentication)
- [Provider 与凭据](/zh/config/providers)
- [自定义 Agent](/zh/config/custom-agents)
- [Agent 编排](/zh/guide/orchestration)
