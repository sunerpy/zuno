---
layout: home

hero:
  name: Zuno
  text: 持久工作，明确边界
  tagline: >
    一个本地 Rust 编程 Agent。目标、工具结果、重试和委派状态都能在进程重启后继续使用。
  image:
    src: /zuno-logo.svg
    alt: Zuno 标志
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
  - title: 工作可从中断处继续
    details: >
      Prompt、工具结果、计划、重试和子 Agent 报告都属于持久会话状态。
      进程重启后可以从已记录的工作继续。
    link: /zh/guide/durable-state
    linkText: Goal 与工作状态

  - title: 角色有固定权限上限
    details: >
      构建、规划、深度实现和专职 Agent 暴露不同的工具边界。
      配置可以继续收窄边界，但不能扩大它。
    link: /zh/guide/agents
    linkText: Agent

  - title: 命令执行受独立控制
    details: >
      权限规则、命令风险检查和 OS 沙箱是三道独立门禁。
      受限模式默认失败即拒绝，只有受信策略能明确选择原生执行。
    link: /zh/guide/permissions
    linkText: 权限与沙箱

  - title: 一套原生运行时
    details: >
      TUI、headless、ACP 和 HTTP 客户端共用同一套 Rust 运行时、
      持久事件、工具和扩展生命周期。
    link: /zh/operate/harness-runtime
    linkText: Harness 运行时
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
cargo install zuno --locked
```

:::

如需安装尚未发布的当前 Git revision：

```sh
cargo install --git https://github.com/sunerpy/zuno zuno --locked
```

ripgrep 14 或更新版本只是 `glob` 与 `grep` 工具的后端，不是 Zuno 启动或核心运行
依赖。bubblewrap 0.8.0 或更新版本也只用于 Linux 上受约束的 `read-only` 与
`workspace-write` Shell。显式 `danger-full-access`，以及符合条件且受信的
`workspace-write` `run-unconfined` 降级，都走原生执行。macOS 与 Windows 当前没有
受约束后端。安装器会使用 `SHA256SUMS` 校验所选 release 的归档。

## 开始运行

配置 Provider 后，先验证配置与模型目录：

```sh
zuno debug config
zuno models myopenai --verbose
```

在具备可用约束后端的 Linux 上，再用只读的 `plan` Agent 验证完整模型与工具链路：

```sh
zuno run --agent plan "概述这个仓库的架构"
```

macOS、Windows，或没有约束后端的受信 Linux 宿主，应按
[快速开始](/zh/guide/quick-start)显式选择原生执行路径。需要交付改动时：

```sh
zuno run "为 users 接口增加分页并运行测试"
```

直接运行 `zuno` 会打开终端应用。Provider 配置、凭据和沙箱检查见
[快速开始](/zh/guide/quick-start)。

## 查找文档

| 需求 | 页面 |
| --- | --- |
| 理解执行模型 | [Zuno 是什么](/zh/guide/what-is-zuno) |
| 配置 Provider | [Provider 与凭据](/zh/config/providers) |
| 查询配置项 | [配置项参考](/zh/config/reference) |
| 启用或切换 History 与 Notes | [History 与 Notes 连续性配置](/zh/config/continuity) |
| 选择 Agent | [Agent](/zh/guide/agents) |
| 配置 Shell 权限 | [权限与沙箱](/zh/guide/permissions) |
| 在编辑器中使用 | [编辑器与 ACP](/zh/guide/editors) |
| 查询命令 | [CLI 参考](/zh/cli/) |
| 理解发布产物 | [发布流水线](/zh/operate/release-pipeline) |
| 排查故障 | [常见问题](/zh/operate/faq) |

## 中文文档范围

中文页面与代码位于同一仓库并随行为更新。[配置项参考](/zh/config/reference)和
[Harness 运行时](/zh/operate/harness-runtime)是导读页；完整字段和协议以页面所链接的
英文参考为准。
