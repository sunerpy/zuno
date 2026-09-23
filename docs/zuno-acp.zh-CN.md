# 原生 ACP

Zuno 将 Agent Client Protocol（ACP）作为原生 stdio 前端提供：

```sh
zuno acp
```

ACP 只是 Zuno 进程内 App Server 的协议投影，不会再启动第二套 agent loop。
ACP session 映射到 App Server 使用的同一份持久化 thread 和 turn，因此取消、
审批、沙箱策略、历史记录与持久化状态仍由原有运行时统一管理。

## 配置

该命令使用普通运行时配置分层。例如：

```sh
# 选择用户 profile。
zuno --profile kiro acp

# 为未由客户端指定模型或目录的 session 设置默认值。
zuno acp --model gpt-5.6-sol -c model_provider='"kiro-local"' --cd /work/project

# 设置权限默认值并增加可写目录。
zuno acp --sandbox workspace-write --add-dir /work/shared

# 遇到未知配置字段时直接失败。
zuno acp --strict-config
```

任意配置覆盖继续使用全局 `-c key=value` 语法。Provider 定义与凭证应放在
配置文件中；`zuno acp` 会明确拒绝 `--oss` 和 `--local-provider`。如果 ACP
客户端支持对应操作，也可以通过协议选择模型、mode 或 session 工作目录。

ACP 传输独占 stdout；日志与诊断信息不得写入协议流。

## 协议范围

当前适配器实现稳定 ACP v1 的初始化、session 新建/加载/恢复/分叉/列表、
prompt、同进程 steer、模型/模式/配置更新（`session/set_mode`、`session/set_model`
与 `session/set_config_option` 落到同一份线程设置）、取消、关闭与删除，并在 ACP
权限请求、session/turn 更新和 App Server 事件之间进行转换。

prompt 内容块到 App Server 输入的映射与参考实现 `codex-acp` 一致，因此无论客户端
连接哪一个 Codex ACP agent，同一段 prompt 对模型的含义相同：`text` 与 `image`
直接透传（`http(s)` 或 `data:` 图片 URI 原样使用）；`resource_link` 变成
`[@name](uri)` 链接；内嵌文本 `resource` 变成该链接加一个
`<context ref="uri">` 块；内嵌 `image/*` blob 变成图片；其他 blob 变成 base64 的
`<context>` 块。`initialize` 未宣告的块类型（例如 `audio`）以 `-32602` 拒绝。

桥接层自有的两种结果使用实现自定义的 JSON-RPC 错误码，且刻意避开两侧已有含义的
码：`session/prompt` 被并入当前活动 turn 作为 steer 时返回 `-32010`（`data` 携带
持久化的并入信息）；`session/steer` 无法命中活动 turn 时返回 `-32011`。ACP 的
`-32000`（需要认证）、`-32002`（资源不存在）与 App Server 的 `-32001`（过载）
原样透传。

## 协议版本与客户端兼容

桥接层使用 ACP 线协议 **1**（稳定版）。ACP 有三条互相独立的版本轴：`initialize` 中协商的
`protocolVersion` 整数（1 稳定、2 草案）、JSON schema 发布版本（1.x）、以及 SDK 包版本
（Rust crate `agent-client-protocol` 已到 2.x，但仍说线协议 1）。客户端升级 SDK——例如
Zed 1.21 升到 `agent-client-protocol` 2.1 与 schema 1.7——发送的仍是 `protocolVersion: 1`，
Zuno 无需改动。协议 2 走出草案后会在显式协商之后追加，同时继续服务协议 1。

在协议 1 之内，桥接层跟进客户端依赖的 schema 增量：

- `tool_call` 更新携带一等 `name`（schema 1.8），与 `title`、`kind` 并列：`shell`、
  `apply_patch`、`web_search`、`spawn_agent`（Zed 用它识别子代理）、MCP 或动态工具自身
  的名字、`view_image`、`image_generation`、`sleep`。
- App Server 的生命周期条目（`contextCompaction`、review 模式标记、`functionCallOutput`）
  不再被投影成工具调用。
- 客户端宣告 `clientCapabilities.elicitation.form`（schema 1.7）时，模型的提问
  （`item/tool/requestUserInput`）通过一次 `elicitation/create` 表单收集：选择题变成
  `enum` 属性，自由文本变成 `string` 属性；机密问题拒绝，因为表单模式不得承载凭据。
  没有表单能力的客户端保持原行为：选择题走 `session/request_permission`，自由文本拒绝。

Zuno 专属能力必须通过显式协议元数据逐步演进。仅凭 ACP SDK 包版本号，不能
视为连接已经启用不稳定的 wire protocol。
