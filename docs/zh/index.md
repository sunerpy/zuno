---
layout: home

hero:
  name: Zuno
  text: 零代码，任何任务
  tagline: >
    用 Rust 编写的单二进制编码 Agent CLI。目标带预算和真实的终止条件，
    用专职 Agent 团队而不是一个全能提示词，编排结构模型无法在运行时改写。
  actions:
    - theme: brand
      text: 快速开始
      link: /zh/guide/quick-start
    - theme: alt
      text: Zuno 是什么
      link: /zh/guide/what-is-zuno
    - theme: alt
      text: GitHub
      link: https://github.com/sunerpy/zuno

features:
  - title: 会收敛而不是发散的目标
    details: >
      Goal 带三样东西：模型不能缩小的 objective、不能改写的 success_criteria、
      以及 token 上限。标记完成要拿授权证据，标记阻塞要给出连续三回合都存在的
      具体条件。
    link: /zh/guide/durable-state
    linkText: Goal、Plan 与 Todo

  - title: 专职 Agent 团队，而非一个提示词
    details: >
      10 个可选 Agent，能力边界各不相同。契约只能收窄权限、永远不能放宽，
      所以选只读 Agent 是一项保证，而不是可以被配置反转的默认值。
    link: /zh/guide/agents
    linkText: Agent

  - title: 编排由配置拥有
    details: >
      Council 的席位、法定人数、并发上限、重试策略与超时都由配置决定。
      模型只提供问题，无法在压力下放宽自己的约束。
    link: /zh/guide/orchestration
    linkText: 编排与委派

  - title: 有真实边界的委派
    details: >
      子 Agent 拿不到父级不具备的工具，delegates 精确限定它能调用谁，
      它的报告是父级需要验证的证据，而不是可以直接采信的结论。
    link: /zh/guide/orchestration
    linkText: 委派

  - title: 单个二进制，只有一个外部依赖
    details: >
      Linux 是静态 musl，其他平台是原生构建。不需要 Node 或 Python，
      也没有要与 Agent 版本对齐的运行时。唯一要求是 ripgrep 14+，
      因为 glob 与 grep 驱动的是真正的 ripgrep 而不是再实现一遍。
    link: /zh/guide/installation
    linkText: 安装

  - title: 原生 Component，而非插件 ABI
    details: >
      把 DeepSeek Harness 的"一切皆插件"具体化为 Rust Component：类型化服务、
      每个副作用对应一个精确 disposer、事务化的 profile 替换。不加载 Rust 动态库，
      因为卸载一个库什么也证明不了。
    link: /zh/guide/plugins
    linkText: 插件与扩展
---

## 安装

::: code-group

```sh [Linux / macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

```sh [Cargo]
cargo install --git https://github.com/sunerpy/zuno zuno-cli --locked
```

:::

然后启动终端应用：

```sh
zuno
```

## 选 Agent，而不是改提示词

在 Zuno 里，你配置的多数东西是**由谁来做**。Agent 的契约在回合开始前就固定了它的能力
上限，所以这个选择是本次运行的性质，而不是一句模型可以自行重新解读的请求。

```sh
# 只读调查。无论配置怎么写，写入类工具根本不会被注册。
zuno run --agent plan "为什么重试预算在首次尝试之前就开始计时"

# 端到端交付，以测试作为验收门槛。
zuno run "为 /users 接口增加分页并跑测试"

# 困难的跨领域改动，且不应再向下扩散委派。
zuno run --agent deep "让会话恢复能承受回合中途的 provider 故障"

# 续跑最近的会话，而不是新建一个。
zuno run --continue "再把每页上限设为 100"
```

底层上，一个回合是一个持久的工作单元：组装好的提示词在请求离开进程前写入 SQLite，
每个工具结果都作为事件记录，会话在进程结束后仍可重放或续跑。这是上面那些保证得以
成立的地基，而不是卖点本身。

## 接下来看什么

| 如果你想 | 阅读 |
| --- | --- |
| 先理解设计再安装 | [Zuno 是什么](/zh/guide/what-is-zuno) |
| 几分钟内跑起来 | [快速开始](/zh/guide/quick-start) |
| 接入一个 Provider | [Provider 与凭据](/zh/config/providers) |
| 查某个配置项 | [配置项参考](/zh/config/reference) |
| 查某条命令 | [CLI 参考](/zh/cli/) |
| 在编辑器里工作 | [编辑器与 ACP](/zh/guide/editors) |
| 排查一个故障 | [常见问题](/zh/operate/faq) |

## 关于中文文档

中文文档与英文文档来自同一份源文件仓库，随代码一起维护。绝大多数页面是完整翻译；
其中 [配置项参考](/zh/config/reference) 与 [Harness 运行时](/zh/operate/harness-runtime)
两页篇幅较长，中文版是导读，逐节的完整契约以英文版为权威来源，页首已标注。

发现译文与实际行为不一致时，以英文版和
[`schemas/zuno.json`](https://github.com/sunerpy/zuno/blob/main/schemas/zuno.json)
为准，并欢迎直接提 issue 或 pull request。
