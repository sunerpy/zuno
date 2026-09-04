# 无界面运行

`zuno run` 在没有终端界面的情况下驱动 harness。它从命令行或文件读取一条消息，把它执行到完成，然后把结果写到 stdout。这是脚本、CI job 和 git hook 使用的形态。

```sh
zuno run "explain what changed in the last commit"
zuno run --format json "list the failing tests" > result.json
zuno run --continue "now add tests for the new branch"
```

持久模型完全一致。无界面运行落盘的事件、提示词收据、plan 与 todo 状态以及 job 记录，与交互式会话相同，因此一个无界面会话可以在终端应用中续跑，反之亦然。

## 选择会话

| 选项 | 效果 |
| --- | --- |
| 不指定 | 启动一个新会话 |
| `-c`、`--continue` | 继续当前目录下最近的会话 |
| `-s`、`--session <SESSION>` | 在这个确切会话中对话 |

```sh
zuno run --session ses_1a2b3c --agent plan "what would a safe migration look like?"
```

使用 `--continue` 或 `--session` 时，本次运行会沿用会话上次使用的 Agent、模型与推理强度，除非下面某个参数另有指定；优先级表见[会话与回合](/zh/guide/sessions#续跑会话)。

这个二进制不提供会话分叉。脚本要探索一个替代方案，又不想让它混进某个人正在阅读的会话时，改为新起一个会话：同时省掉 `--continue` 与 `--session`，并用 `--title` 让这次运行事后好找。早期版本接受又拒绝的那些选项见 [zuno run](/zh/cli/run)。

## 选择 Agent、模型与推理强度

| 选项 | 效果 |
| --- | --- |
| `--agent <AGENT>` | 以这个 Agent 及其契约运行 |
| `-m`、`--model <MODEL>` | 使用这个 `provider/model` |
| `--variant <VARIANT>` | 用模型声明的确切 variant 覆盖已配置的推理设置 |
| `--thinking` | 在可用时请求 `high`，否则取声明中最强的非 `off` 级别 |

`--thinking` 与 `--variant` 互斥。规范的 variant 名称 `off`、`low`、`medium`、`high`、`xhigh` 和 `max` 只有在所选模型声明了它们、或该模型在没有命名目录的情况下暴露通用推理能力时才被接受。非规范名称会复制该 variant 完整的 provider 选项对象。未知名称会在 HTTP I/O 之前失败，并列出可用项。

当确切的推理强度很重要时，优先使用 `--variant max` 或 `--variant xhigh`；`--thinking` 刻意只是一个自动化的便利选项。

在续跑的会话上，这些参数都优先于会话上保存的值；没有给出的参数则先回退到保存的值，再回退到配置。`--agent` 指定一个与保存值不同的 Agent 时，模型会按配置重新路由；要保留会话的模型就再加上 `--model`。已不存在的已保存 Agent 或模型会以一条状态提示报告（`--format json` 下为 `status_detail`），运行继续使用下一级回退，而不会失败。

## 输出格式

```sh
zuno run --format default "summarize the diff"
zuno run --format json "summarize the diff"
```

`default` 是给人读的文本；`json` 是给解析用的。脚本中请用 `json`，而不是去抓取格式化输出，因为格式化后的形态属于展示层。

当回合没有完全按配置执行时，JSON 流会包含一条 `notice` 事件：
`{"type":"notice","severity":"warning","code":"budget.token_budget","detail":"…"}`。
`severity` 取 `info`、`warning` 或 `error`；`code` 稳定不变，来自 `instruction.*`
族（无法抓取的远程规则文件，其规则本轮不生效，但回合继续执行）或 `budget.*` 族（被额度
停下的回合，或预算策略要求的一次压缩）。本地无法读取或超预算的规则文件会在任何 provider
请求之前让本轮以错误失败，不会产生 `notice` 事件。同一事件在 server 事件流上以 `notice` 发布。

Provider 可见的推理进度需要显式启用：

```sh
zuno run --show-reasoning "summarize the failure" > answer.txt 2> reasoning.txt
```

最终答案只留在 stdout。stderr 只接收 provider 明确提供的 reasoning delta，并放在 `<<<zuno:reasoning>>>` 与 `<<<zuno:end-reasoning>>>` 之间；signed thinking 与 encrypted reasoning 永不显示。若 provider 缺少 start 事件，Zuno 会等首个 delta 再打开区块，并在 provider 错误或流结束时保证闭合。`--show-reasoning --format json` 会被拒绝，因为 JSON 模式已经输出结构化事件。

日志绝不会写到 stdout。诊断时把它们镜像到 stderr：

```sh
zuno run --print-logs --log-level DEBUG "summarize the build failure"
```

## 附加文件

`-f`/`--file` 可重复使用，而且它是唯一携带文件的选项。受支持的图像会在写入 inbox 前完成规范化，并接纳到当前数据库专属的持久对象存储；默认源上限是 20 MiB，规范化编码上限是 5 MiB。任何其他引用必须是 UTF-8 文本，且在 51,200 字节与 2,000 行以内，插入时带显式的起止标记。不受支持的二进制文件，包括 PDF，不会被静默转换。参见[图像与文件引用](/zh/guide/attachments)。

## 脚本中的约束

`--sandbox` 为本次调用选择约束方式：

```sh
zuno run --sandbox read-only --agent plan "audit the retry policy"
zuno run --sandbox workspace-write "fix the failing test and re-run it"
zuno run --sandbox danger-full-access "run in a deliberately unconfined container"
zuno run --sandbox workspace-write \
  --sandbox-on-unavailable run-unconfined \
  "prefer confinement, but allow eligible unavailable fallback"
zuno run --agent plan --sandbox-backend native \
  "run natively on a host without an OS sandbox, permission mode kept"
```

Agent 契约仍可能进一步收窄它。即使调用时请求了更宽的模式，只读 Agent 也只获得
`read-only`，并且绝不会使用不可用降级。`danger-full-access` 始终选择原生后端，并把生效
权限模式设为 `allow_all`。`run-unconfined` 会保留已配置的权限模式和硬拒绝，但降级期间
请求的文件系统与网络限制不会由 OS 强制执行。`--sandbox-backend native`（或 `ZUNO_SANDBOX_BACKEND=native`，或
受信层里的 `sandbox.backend: native`）为本次调用的每个 Agent（包括只读 Agent）选择原生后端，
权限模式保持不变；headless 运行永远不会询问，所以在 macOS 与 Windows 上，这个标志、
环境变量或受信配置层是只读 Agent 获得 Shell 的唯一方式。

在 CI 中依赖它之前先验证可部署性，并让退出状态成为 job 的门禁：

```sh
zuno debug sandbox --mode workspace-write --network deny --check
```

即使 `--sandbox-on-unavailable run-unconfined` 可以让运行时继续，只要请求的约束不可用，
`--check` 仍会失败。

## 没有人在场时的权限模式

这一部分决定了一次无界面运行到底能不能跑通。

| 模式 | 无界面下的行为 |
| --- | --- |
| `standard` | 应用配置的规则和常规风险门禁。一条 `ask` 规则没有人可问 |
| `strict` | 失败即拒绝。每一次有副作用的调用都需要一次新的人类决定，而此时没有在场用户 |
| `allow_all` | 不再询问。显式拒绝、灾难性 shell 拒绝、沙箱权限和参数校验仍然生效 |

对于无人值守的自动化，请配置你真正想要的规则，而不是依赖无人能回答的询问：

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "read": "allow",
      "glob": "allow",
      "grep": "allow",
      "edit": "deny",
      "shell": {
        "*": "deny",
        "cargo test*": "allow",
        "git push*": "deny"
      }
    }
  }
}
```

最后一条匹配的规则胜出，因此 catch-all `*` 写在最前面，从它当中划出例外的窄模式写在最后面。如果顺序反过来，写在末尾的 `"*": "deny"` 会覆盖 `cargo test*`，这份配置本来要跑的测试就永远跑不起来。`edit` 这个键同时管 `write`、`edit` 和 `apply_patch` 三个工具，不存在单独的 `write` 规则键。`--auto` 是为交互使用而存在的，在 strict 模式下它会让位给人类审批者；它不是策略的替代品。参见[权限与沙箱](/zh/guide/permissions)。

## 示例：一个 CI 门禁

```sh
#!/bin/sh
set -eu

zuno debug sandbox --mode workspace-write --check

result=$(
  zuno run --format json --agent plan \
    --sandbox read-only \
    "Review the staged diff for regressions. Report blocking findings only."
)

printf '%s\n' "$result" > review.json
```

在 CI 中读一份 plan Agent 的评审在构造上就是安全的：没有注册任何写入类工具，并且契约把约束钉在 `read-only`。

## Server 与编辑器界面

还有另外两个非交互入口。`zuno serve` 为外部客户端启动 HTTP server；`zuno acp` 通过 stdin 与 stdout 说 Agent Client Protocol，供编辑器使用。

```sh
zuno serve --port 4096
zuno acp --check
```

Server 支持通过 `ZUNO_SERVER_PASSWORD` 启用 Basic Auth，也支持显式的回环专用 `--browser-auth` bootstrap。浏览器认证只打印一次启动 URI，token 只能消费一次，随后签发绑定 authority 的签名 Cookie；它绝不会让非回环监听变得可接受。对 ACP 来说，stdout 承载协议分帧，因此请用 `--print-logs` 把诊断信息送到 stderr。参见[编辑器与 ACP](/zh/guide/editors)。

HTTP 界面有三处语义值得驱动会话的脚本了解。`POST /api/session/{id}/interrupt` 只取消正在运行的回合；对空闲会话它返回 `204` 且什么都不做，不会让下一个普通回合（包括对刻意排队输入的显式恢复）一开始就处于被中断状态。`GET /api/session/{id}/event` 对数据库中不存在的会话返回 `404`，错误码为 `not_found`，而不是打开一条永远不会产生事件的流。压缩持有会话租约期间接纳的 prompt 或 steer 输入会留在持久 inbox 中，并在压缩释放租约后立即被执行，而不是等到某个无关事件碰巧唤醒会话。

## 参见

- [zuno run](/zh/cli/run)
- [终端应用](/zh/guide/tui)
- [权限与沙箱](/zh/guide/permissions)
- [运维日志](/zh/operate/logging)
