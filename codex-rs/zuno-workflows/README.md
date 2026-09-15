# Zuno workflows

`zuno-workflows` is the engine-neutral contract and registry for user-owned workflows.
Zuno ships workflow engines and loading infrastructure, but **does not ship an
application-owned business workflow**. In particular, workflows such as a frontend review
or consensus pipeline belong in a user file or an optional plugin/example, not in the binary.
No such name is reserved by the DSL or control plane, and a client must not inject a hidden
workflow catalog. CLI, TUI, ACP, App Server clients, and external automation all operate on the
same discovered resources.

A host supplies explicit roots for three source scopes:

1. plugin
2. user
3. project

Resolution is deterministic: project overrides user, and user overrides plugin. Two workflows
with the same `metadata.name` at the same scope are not activated; the registry reports a
`duplicate-name` diagnostic instead of silently choosing one. Invalid files are isolated to
their source and do not prevent independent valid workflows from loading.

Plugins declare workflow files or directories with the `workflows` field in
`.codex-plugin/plugin.json`. The plugin loader preserves these roots and their plugin identity;
it does not turn them into built-in workflows.

This follows the DSH-style composition idea without embedding DSH or copying its wire/config
surface: the workflow is a replaceable resource, while Zuno owns the typed `zuno.workflow/v1`
schema, validation, lifecycle, durable ledger, permissions, and execution engines.

The repository contains optional, non-installed templates under
`examples/zuno-workflows/`. Copying one into an explicit discovery root is a
user action; the runtime does not special-case its name or content.

Local discovery is bounded by root, file, depth, document-size, and script-size limits. It does
not follow symlinks. A relative `scriptFile` is resolved beneath the declared root, read as a
bounded immutable input, and included in `RegisteredWorkflow::executable_digest`. Hosts must
persist this executable digest when binding a run so script changes cannot be replayed as the
old workflow.

Run admission also persists a non-secret `zuno.workflow-bindings/v1` snapshot for every route.
It covers the exact backend factory/plugin revision and the resolved execution-profile model,
provider, reasoning, approval, permission, sandbox, and workspace policy. The host resolves the
same binding immediately before dispatch and fails closed on drift; credentials and environment
variable values are never stored in the ledger.

## 中文说明

`zuno-workflows` 只提供工作流协议、加载器和执行引擎接口，不在应用内固化任何业务工作流。
前端评审、共识收敛等流程应由用户 YAML/JSON 或可选插件/示例提供，而不是编译进 Zuno。
DSL 与控制面不会保留 `frontend-consensus` 之类的特殊名称；CLI、TUI、ACP、App Server
客户端及外部自动化都只能使用同一套外部发现资源，不得由某个前端注入隐藏流程。

加载来源分为插件、用户和项目三层，优先级为“项目 > 用户 > 插件”。同一层出现同名工作流时
拒绝激活并返回诊断，避免静默抢占。插件通过 `.codex-plugin/plugin.json` 的 `workflows`
字段声明文件或目录，加载后仍保留插件归属和来源证明。

这里借鉴 DSH 的组合理念，但不会嵌入 DSH 或复制其配置/协议兼容面。工作流是可替换资源，
Zuno 负责自身的 `zuno.workflow/v1` 类型定义、校验、生命周期、持久化 ledger、权限和执行引擎。

本地发现具有目录数、文件数、递归深度和文件大小上限，并拒绝符号链接。`scriptFile`
必须位于声明根目录内，其内容会进入可执行摘要；持久化运行记录时应使用该摘要。

运行准入还会为每条路由持久化不含秘密的 `zuno.workflow-bindings/v1` 快照，覆盖精确的
backend factory/插件修订，以及 execution profile 解析后的模型、provider、推理等级、审批、
权限、沙箱和工作区策略。真正分派前必须重新解析并校验；发生漂移时 fail closed，凭据和环境
变量值不会写入 ledger。

### `graph/v1` execution contract

A graph node is dispatched as one typed Agent host call. The call payload contains the stable
node `id`, logical `route`, rendered `prompt`, original node `input`, immutable run `args`, the
optional `outputSchema`, and a `needs` object keyed by dependency node ID. If `input` is a non-empty string it becomes the
instruction; if it is an object with a non-empty `prompt`, that field becomes the instruction.
The engine always appends the JSON run/dependency context and performs no implicit template or
model expansion.

Ready nodes are scheduled in document order, with at most `maxConcurrentAgents` calls in flight.
The scheduler is work-conserving: whenever a node settles, any newly ready dependants may start
immediately if capacity is available; unrelated slow nodes do not impose a global stage barrier. A
single terminal node becomes the workflow
result; multiple terminal nodes become an object keyed by terminal node ID. `maxWallTimeMs` and
explicit cancellation are enforced across the whole graph. Calls receive a cancellation token and
are allowed a bounded settlement window so the ledger can record their outcome; failure to settle
is reported as `uncertain`, never mechanically replayed.

### `graph/v1` 执行约定

每个图节点会被分派为一次带类型的 Agent host call。调用载荷包含稳定节点 `id`、逻辑
`route`、渲染后的 `prompt`、原始节点 `input`、不可变的运行参数 `args`、可选
`outputSchema`，以及按依赖节点 ID 索引的 `needs` 对象。非空字符串 `input` 会作为指令；对象中的非空 `prompt` 字段也可作为
指令。引擎只会附加 JSON 运行/依赖上下文，不会隐式展开模板、供应商或模型。

就绪节点按文档顺序调度，同时运行数不超过 `maxConcurrentAgents`。调度器采用事件驱动、
work-conserving 语义：任一节点结算后，只要有可用并发槽，新就绪的依赖节点即可立即开始；
无关的慢节点不会形成全局阶段屏障。只有一个终端节点时直接返回该节点结果；多个终端节点时返回以节点 ID
为键的对象。`maxWallTimeMs` 和显式取消覆盖整个图；取消后的调用会获得有限结算窗口，
以便 ledger 写入确定结果。无法结算时结果为 `uncertain`，不会机械重放。
