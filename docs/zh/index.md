---
layout: home

hero:
  name: Zuno
  text: 零代码，任何任务
  tagline: >
    用 Rust 编写的单二进制编码 Agent CLI。无运行时依赖，会话在进程重启后仍可恢复，
    OS 沙箱在不可用时拒绝执行而非降级运行。
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
  - title: 单个二进制，旁边不需要任何东西
    details: >
      Linux 提供静态 musl 构建，macOS 与 Windows 提供原生构建。不需要 Node、
      不需要 Python，也不必维护一个与 Agent 版本对齐的运行时。
    link: /zh/guide/installation
    linkText: 安装

  - title: 沙箱不可用时直接拒绝
    details: >
      read-only 与 workspace-write 都要求一个已验证的 OS 约束后端。后端缺失时
      Zuno 拒绝启动会话，而不是静默地以无约束方式运行你的代码。
    link: /zh/guide/permissions
    linkText: 权限与沙箱

  - title: 持久化是结构性保证
    details: >
      每个提示词分段、工具结果和子 Agent 报告都在 provider 请求发出前落盘。
      重启后可重建工作现场，包括从 SQLite 读回的重试截止时间。
    link: /zh/guide/durable-state
    linkText: Goal、Plan 与 Todo

  - title: 原生扩展，而非插件 ABI
    details: >
      要么是显式 WASI 授权下的 WebAssembly 组件，要么是使用行分隔 JSON-RPC 的
      受限子进程。能力经过声明与校验，不会被意外继承。
    link: /zh/guide/plugins
    linkText: 插件与扩展

  - title: 有真实边界的委派
    details: >
      把有界目标交给专职 Agent。子 Agent 的报告是父级需要验证的证据，
      并且子 Agent 永远无法获得父级不具备的工具。
    link: /zh/guide/orchestration
    linkText: 编排与委派

  - title: 自带 Provider
    details: >
      支持 Anthropic、OpenAI、Google、Bedrock 以及任何 OpenAI 兼容端点。
      凭据保存在你控制的存储中，模型路由由配置决定。
    link: /zh/config/providers
    linkText: Provider 与凭据
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

## 一个回合实际是什么

Zuno 不是一个恰好能执行命令的聊天窗口。一个回合是一个持久的工作单元：组装好的
提示词在请求离开进程前写入 SQLite，每个工具结果都作为事件记录，会话在进程结束后
仍可重放或续跑。

```sh
# 直接改代码，使用 workspace-write 沙箱，以测试作为验收门槛。
zuno run "为 /users 接口增加分页并跑测试"

# 续跑最近的会话，而不是新建一个。
zuno run --continue "再把每页上限设为 100"

# 只读调查。写入类工具根本不会被注册。
zuno run --agent plan "为什么重试预算在首次尝试之前就开始计时"
```

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
