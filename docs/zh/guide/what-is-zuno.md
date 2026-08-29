# Zuno 是什么？

Zuno 是一个以单个 Rust 可执行文件运行的编码 Agent。你给它一个有界的目标，它会读代码、改文件、执行命令，并汇报自己验证过什么。它旁边不需要安装任何运行时，也不需要常驻任何服务。

它的与众不同之处不在于模型，而在于：任何可能改变模型请求的输入，都会在该请求发出之前写入 SQLite；命令执行由操作系统约束，而不是靠一段提示词请求模型守规矩。

## 设计承诺

四个决策决定了其余一切。每一个都以放弃一些便利，换取一项你可以依赖的性质。

### 单个二进制，无运行时依赖

Linux 发行版是静态 musl 产物，macOS 与 Windows 使用原生构建。执行路径中没有 Node、Python 或包管理器。只需要一个外部可执行文件：`rg`（ripgrep）主版本 14 或更新，因为 `glob` 与 `grep` 驱动的是真正的 ripgrep，而不是对它的目录遍历器再实现一遍。`rg` 缺失或版本不受支持时，工具运行时会直接报启动错误，绝不静默降级。

### 持久化是构造性的

会话不是一个滚动回看缓冲区。提示词、工具结果、重试通知和子 Agent 报告都是持久的会话事件，组装好的提示词会带着稳定的分段标识符和内容摘要，在 provider 请求之前落盘。回合执行中途死掉的进程可以恢复，重试截止时间会从 SQLite 重建，而不是随进程一起消失。

这也正是存储会不断增长、最终需要清理的原因。参见[会话与回合](/zh/guide/sessions)。

### 沙箱默认失败即拒绝

`read-only` 与 `workspace-write` 都要求一个已验证的 OS 约束后端。当没有可用后端时，
默认的 `sandbox.onUnavailable: "deny"` 会拒绝 Shell：

```text
no trusted system bubblewrap executable was found
```

无约束执行始终需要受信的显式选择。`danger-full-access` 直接指名无约束执行，跳过受限后端
发现并使用原生进程后端。另一种方式是设置
`sandbox.onUnavailable: "run-unconfined"`，只允许具备写能力的
`workspace-write` Agent 在符合条件的类型化不可用错误之后降级；只读 Agent、不安全错误与
内部错误绝不降级。约束后端目前只在 Linux 上实现，因此 macOS 与 Windows 默认失败即拒绝，
但具备写能力的执行可以使用以上任一受信选择。完整细节见
[权限与沙箱](/zh/guide/permissions)。

### 扩展是原生的，不是插件 ABI

一个扩展包要么是声明式的（Agent、workflow、Skill），要么是在显式 WASI 授权下运行的 WebAssembly 组件，要么是一个受限子进程 —— 后者必须声明 `host.full`，因为普通 OS 进程无法强制执行任何更窄的约束。Zuno 不加载 Rust 动态库：Rust 没有稳定的插件 ABI，而卸载一个库无法证明它的线程、回调和借用值都已消失。参见[插件](/zh/guide/plugins)。

## 与聊天形态的工具有何不同

一个能调工具的聊天界面，是为对话优化的。Zuno 为一个能在中断后存活的工作单元优化。

| 关注点 | 聊天形态的工具 | Zuno |
| --- | --- | --- |
| 历史 | 内存中的对话记录，或托管的线程 | 持久的 SQLite 事件，可重放、可续跑 |
| 提示词 | 每次请求现场组装，不保留 | Hook 之后落盘，带分段 id 与摘要 |
| 命令安全 | 请求模型不要造成破坏 | OS 约束加一道独立的权限门 |
| 重试 | 客户端循环，重启即丢失 | 落盘的指数退避截止时间 |
| 委派 | 同一上下文里的另一段提示词 | 拥有自身持久状态和能力上限的子会话 |
| 工具重复执行 | 失败即重试 | 默认至多一次；只有只读或幂等的工具才声明可重放 |

实际后果在副作用附近发生超时时最明显：Zuno 会把该结果记录为不确定，并要求检查权威状态。它不会机械地重跑这次调用，然后指望第一次什么都没做。

## Zuno 不做什么

把这一点讲清楚能省时间：

- 没有托管控制台、随包 Web 应用或托管的 GitHub Agent。那些命令名注册的唯一目的是说明用什么替代它们，并且会以失败退出。参见[被排除的命令](/zh/cli/excluded)。
- 没有自卸载。用当初安装它的方式移除这个二进制文件。
- 没有增量数据库迁移链。项目尚未发布，所以 schema 变更会提升格式版本，开发数据库直接重建。参见[数据库生命周期](/zh/operate/migration)。
- 与 OpenCode、Codex 或 Claude Code 之间没有配置、插件、hook 或工具参数兼容性。它们是设计参考，不是兼容目标。
- macOS 与 Windows 上还没有受约束的沙箱。默认失败即拒绝；受信的
  `danger-full-access` 或仅不可用时降级可以为具备写能力的 Agent 选择原生执行。

## 从哪里开始

想先跑起来，读[安装](/zh/guide/installation)，然后读[快速开始](/zh/guide/quick-start)。想先理解执行模型，[会话与回合](/zh/guide/sessions)和[Agent](/zh/guide/agents)这两页信息密度最高。

## 参见

- [安装](/zh/guide/installation)
- [快速开始](/zh/guide/quick-start)
- [权限与沙箱](/zh/guide/permissions)
- [Harness 运行时](/zh/operate/harness-runtime)
