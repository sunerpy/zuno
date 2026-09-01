# 开发 Agent 与扩展

本指南说明应选择哪一种 Zuno 扩展接口，以及当前实现如何把它接入正在运行的
Agent。覆盖声明式 Agent、WASI Component Model 工具、受信进程工具和编译进
二进制的原生 Rust 组件。

最重要的区别是：

- **Agent 定义**选择提示词、模型路由、工具、权限、Skill 与委派目标；
- **可执行插件**在经过校验的包边界后实现一个或多个工具；
- **`AgentDriver`**拥有回合驱动算法本身。

声明式包或 WASI 包可以同时增加 Agent 和工具，但不能替换 Provider 循环、
凭据权限源、审批服务、持久 inbox 或子回合调度器。这些属于受信的原生 Rust
职责。

本页描述当前 `main` 分支实现的 `zuno.extension/v1`、`zuno.plugin/1` 和原生
`Component`/`HarnessProfile` 接口。Zuno 仍是 0.x 项目，开发时应使用与目标
二进制同一 revision 的接口定义和示例。

## 选择最小且正确的接口

| 需求 | 使用方式 | 是否运行代码 | 能否替换 Agent 循环 |
| --- | --- | --- | --- |
| 修改提示词、模型、工具允许列表、权限、Skill 或委派目标 | 配置或 Markdown Agent | 否 | 否 |
| 把 Agent、斜杠 workflow 与 Skill 作为一个可安装单元交付 | 声明式 `extension.json` 包 | 否 | 否 |
| 增加一个具有显式文件系统、环境和网络授权的可移植工具 | WASI 组件包 | 是，在 Wasmtime 中 | 否 |
| 增加一个依赖普通宿主可执行文件或已安装 SDK 的工具 | 受信进程包 | 是，作为子进程 | 否 |
| 增加 Provider、登录流程、凭据存储、审批服务、类型化服务、调度器或完整回合策略 | 原生 Rust `Component` 或 `AgentDriver` | 是，编译进二进制 | 通过 `AgentDriver` 可以 |
| 把已安装的 Codex 或 Claude Code 作为有界子 Agent 调用 | `productAgent` 配置 | 是，通过对应原生协议适配器 | 否 |

从能够表达行为的第一行开始。越往下，权限、生命周期工作、测试成本和错误实现
可能造成的损害都越大。

## 各接口如何组合

一个会话按以下顺序解析扩展与 Agent：

```text
静态包 + 进程本地声明式包
    -> 检查名称冲突后的扩展目录
    -> Agent / workflow / Skill 贡献
    -> 可执行插件工具代理
    -> HarnessProfile 激活
    -> 原生工具 registry
    -> AgentProfile 能力收窄
    -> Provider 可见的提示词与工具 schema
```

可执行部分和声明式部分共享包所有权，但消费者不同：

1. `zuno-extension` 发现并校验 `extension.json`。
2. `resolve_active` 合并静态包与进程本地包。重复包 id，或重复 Agent、
   workflow、Skill、工具名都会失败；Zuno 不会静默选择一个胜者。
3. Agent 贡献与配置、Markdown Agent 合并，然后冻结为 `AgentProfile`。
4. Workflow 贡献进入常规斜杠命令 registry。
5. Skill 贡献带着包来源进入常规 Skill 目录。
6. 运行时工具声明转换成原生 `Tool` 代理。
7. 运行时宿主作为延迟 profile effect 挂载；初始化成功前不会发布工具。
8. 最终工具 registry 继续应用来源优先级、Agent 收窄、权限、请求 hook 和
   Provider 能力过滤。

因此扩展 Agent 与内置 Agent 使用同一套 `task`、权限、沙箱、模型和子会话路径。
扩展包不会创建第二套 Agent 运行时。

## 声明式 Agent 是数据，不是 Driver

`extension.json` 提供的 Agent 接受与 `agents.<name>` 相同的字段：

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "release-review",
  "description": "Adds a bounded release reviewer.",
  "agents": {
    "release-reviewer": {
      "description": "Reviews release safety without editing.",
      "mode": "subagent",
      "model": "myopenai/reasoner",
      "tools": ["read", "glob", "grep", "lsp", "web_search"],
      "requiredSkills": ["release-safety"],
      "prompt": "Review immutable inputs, rollback evidence, and required gates. Do not delegate.",
      "permission": {
        "mode": "standard",
        "rules": {
          "*": "deny",
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "lsp": "allow",
          "web_search": "allow"
        }
      }
    }
  },
  "skills": [
    {
      "name": "release-safety",
      "description": "Use for release and deployment reviews.",
      "content": "Check exact commits, required jobs, immutable artifacts, rollback, and production evidence."
    }
  ]
}
```

需要牢记的边界：

- 映射键就是 Agent 身份；扩展 Agent 不能给自己改名；
- 扩展不能禁用内置 Agent；
- 只有 `mode: "subagent"` 或 `"all"` 才会进入 `task` 阵容；
- `tools` 是确切允许列表，不是追加；
- `requiredSkills` 只保证指令，不授予工具权限；
- 权限可以继续收窄生效工具面，但无法恢复父级 Attempt 中不存在的工具；
- 需要调用该 Agent 的 workflow 必须使用普通 `task` 工具，并给出显式的类型化
  委派契约。

全部字段见[自定义 Agent](/zh/config/custom-agents)，子回合语义见
[编排与委派](/zh/guide/orchestration)。完整声明式示例位于
[`examples/plugins/review-kit`](https://github.com/sunerpy/zuno/tree/main/examples/plugins/review-kit)。

## 实现一个 WASI 工具

当行为天然是一项工具，并且权限可以表示为显式授权时，使用 WASI。Zuno 托管的是
WebAssembly **组件**，不是旧式 core module，也不是 Rust 动态库。

### 包目录

```text
word-stats/
├── extension.json
├── plugin.wasm
└── guest/
    ├── Cargo.toml
    └── src/lib.rs
```

安装后的目录名必须等于包 `id`。组件产物必须使用相对路径，并且不能逃出包目录。

### Guest crate

Rust guest 是一个面向 `wasm32-wasip2` 构建的 `cdylib`：

```toml
[package]
name = "word-stats"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
serde_json = "1"
wit-bindgen = "0.60"

[workspace]
```

从规范 WIT world 生成绑定：

```rust
wit_bindgen::generate!({
    path: "../../../../wit/zuno-plugin",
    world: "plugin",
});

struct WordStats;

impl Guest for WordStats {
    fn initialize(
        _package_id: String,
        _workspace: String,
        _capabilities: Vec<String>,
    ) -> Result<String, String> {
        Ok("zuno.plugin/1".to_owned())
    }

    fn invoke(
        tool: String,
        arguments_json: String,
        _session_id: String,
        _message_id: String,
        _call_id: String,
        _agent: String,
    ) -> Result<(String, String, String), String> {
        if tool != "word_stats" {
            return Err(format!("unknown tool `{tool}`"));
        }
        let arguments: serde_json::Value =
            serde_json::from_str(&arguments_json).map_err(|error| error.to_string())?;
        let text = arguments
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "`text` must be a string".to_owned())?;
        let words = text.split_whitespace().count();
        Ok((
            "Word statistics".to_owned(),
            format!("{words} words"),
            serde_json::json!({ "words": words }).to_string(),
        ))
    }

    fn shutdown() -> Result<(), String> {
        Ok(())
    }
}

export!(WordStats);
```

ABI 刻意把 JSON Schema 留在 manifest，仅在 guest 边界使用 JSON 字符串：

```text
initialize(package-id, workspace, capabilities) -> result<protocol-version, error>
invoke(tool, arguments-json, session-id, message-id, call-id, agent)
    -> result<(title, output, metadata-json), error>
shutdown() -> result<_, error>
```

`initialize` 必须精确返回 `zuno.plugin/1`。`metadata-json` 必须能解析为对象或
`null`。`output` 对模型可见，因此不得放入凭据、Authorization header、私有上游
响应正文或无界二进制数据。

### Manifest 与权限

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "word-stats",
  "description": "Computes statistics without host access.",
  "runtime": {
    "kind": "wasi",
    "artifact": "plugin.wasm",
    "capabilities": [],
    "environment": [],
    "fuel": 10000000,
    "memoryMiB": 64,
    "timeoutMs": 30000
  },
  "tools": {
    "word_stats": {
      "description": "Count words in supplied text.",
      "parameters": {
        "type": "object",
        "properties": {
          "text": { "type": "string", "maxLength": 100000 }
        },
        "required": ["text"],
        "additionalProperties": false
      },
      "effect": "readOnly",
      "replay": "safe",
      "concurrency": "exclusive",
      "uiIntent": "generic"
    }
  }
}
```

授权信封与工具描述彼此独立地强制执行：

| 授权 | Guest 权限 |
| --- | --- |
| 无 | 没有工作区 preopen、继承环境、stdio 或网络 |
| `workspace.read` | 只读 `/workspace` preopen 与工作目录 |
| `workspace.write` | 可读写 `/workspace`；包含读权限 |
| `network` | WASI DNS 与 TCP/UDP socket |
| `environment: ["NAME"]` | 只复制指定的宿主环境变量 |

网络与代理配置是两回事。使用代理的 guest 通常同时需要 `network`，以及显式列出
`HTTP_PROXY`、`HTTPS_PROXY` 和 `NO_PROXY`。能够只授权一个变量时，不要授予整套
可能携带凭据的环境。

Manifest 校验器会强制：

- `timeoutMs` 范围为 1 到 300,000；
- WASI `fuel` 范围为 10,000 到 1,000,000,000；
- `memoryMiB` 范围为 8 到 1,024；
- capability 与环境变量名不得重复；
- 每个 runtime 至少有一个工具；
- 每个可执行工具必须有 runtime；
- 协议 v1 调用必须独占；
- 只有 `effect: readOnly` 才能使用 `replay: safe`；
- 只有授权信封排除了网络、`workspace.write` 与 `host.full` 时，才能声明只读；
- 运行时工具不能声明 `userMediated` 或 `delegating`。

最后一条是架构边界，不是格式限制：人机交互与子回合创建必须保持为 Zuno 原生、
持久的操作。

### 宿主生命周期

WASI 运行时通过以下路径接入原生组件运行时：

```text
扩展 manifest
    -> RuntimeSurface 工具代理
    -> ProfileBundle 中的 PluginRuntimeComponent
    -> Component::prepare 注册延迟 effect
    -> effect start 构建 Wasmtime instance
    -> initialize 协商 zuno.plugin/1
    -> profile 原子发布工具
    -> invoke 串行执行调用
    -> profile 撤销时移除路由
    -> shutdown 在逆序清理中执行
```

每次 exported call 前，Zuno 会补充 fuel、设置 epoch deadline、应用墙钟超时，并响应
用户中断。一个组件实例的调用会串行执行。Trap、join 失败、非法 metadata 响应或未能
收敛的中断都会 poison 该实例，并阻止之后的路由。

调用超时属于结果不确定，因为 guest 副作用可能已经开始。宿主会撤下该实例，并且绝不
机械重放。只有操作语义与可强制执行的授权信封都只读时，工具才能声明安全重放。原生
工具出于同样的至多一次理由默认使用 `ToolReplayPolicy::Never`。

### 构建与测试

```sh
CARGO_TARGET_DIR=target/plugin-examples/word-stats \
  cargo build \
  --manifest-path path/to/word-stats/guest/Cargo.toml \
  --target wasm32-wasip2 \
  --release

cp \
  target/plugin-examples/word-stats/wasm32-wasip2/release/word_stats.wasm \
  path/to/word-stats/plugin.wasm

zuno plugin add path/to/word-stats --project
zuno plugin list
```

在 Zuno 仓库中，规范验收路径是：

```sh
sh scripts/check-plugin-examples.sh
cargo test -p zuno-extension --test manifest
cargo test -p zuno-extension --test runtime_hosts
```

`runtime_hosts` 中被忽略的 WASI fixture，会在 guest 组件构建后由
`check-plugin-examples.sh` 真正执行。

## 实现原生 Rust 组件

当行为需要受信的类型化接口、拥有持久状态或凭据、参与 Provider 或审批生命周期，
或必须替换回合 Driver 时，使用原生 Rust。原生行为会编译进 Zuno；不存在运行时加载的
Rust ABI。

### 发布类型化服务与精确 disposer

`Component::prepare` 必须无副作用。它可以暂存服务并声明 effect，但不能直接 bind、
spawn、subscribe 或修改外部世界。

```rust
use std::sync::Arc;

use async_trait::async_trait;
use zuno_runtime::{Component, EffectError, PrepareContext, RuntimeError};

trait ReviewIndex: Send + Sync {
    fn revision(&self) -> u64;
}

struct ReviewIndexService {
    revision: u64,
}

impl ReviewIndex for ReviewIndexService {
    fn revision(&self) -> u64 {
        self.revision
    }
}

struct ReviewIndexComponent {
    service: Arc<dyn ReviewIndex>,
}

#[async_trait]
impl Component for ReviewIndexComponent {
    fn id(&self) -> &str {
        "review-index"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn ReviewIndex>(Arc::clone(&self.service))?;
        context.effect("watcher", || async {
            let watcher = start_watcher()
                .await
                .map_err(|error| EffectError::new(error.to_string()))?;
            Ok::<_, EffectError>(move || async move {
                watcher
                    .stop()
                    .await
                    .map_err(|error| EffectError::new(error.to_string()))
            })
        })
    }
}
```

`start_watcher` 背后的函数名取决于具体应用；关键是以下形状：

1. `prepare` 暂存类型化服务。
2. `effect` 记录 start closure。
3. 只有所有候选组件都 prepare 成功后，start closure 才获取资源。
4. Start 成功时必须返回该资源的精确异步 disposer。
5. Start 失败时不得留下活资源。
6. Disposer 在资源真正静默前不得报告成功。

消费者在 prepare 时解析并记录依赖：

```rust
let index = context.require::<dyn ReviewIndex>()?;
```

Provider 被替换时，依赖组件会先针对候选服务图重新 prepare，之后任何内容才会对外可见。

### 构建 Profile

部署和替换所有权相同的组件应放进同一个 `ProfileBundle`。`HarnessProfile` 是一个运行时
作用域的完整组合：

```rust
use zuno_runtime::{HarnessProfile, ProfileBundle};

let profile = HarnessProfile::new("review")
    .with_bundle(
        ProfileBundle::new("review.services")
            .with_component(ReviewIndexComponent { service }),
    )
    .with_bundle(zuno_harness::tool_contributions_bundle(
        "review.tools",
        "review.tool-contributions",
        contributions,
    ));

runtime.activate_profile(profile).await?;
```

Profile、bundle、component、effect、tool 与 capability id 都应稳定且唯一。Component
身份控制替换和诊断，不只是显示标签。

### 提供原生工具

原生工具实现 `zuno_tool::Tool` 或 `TypedTool`，然后进入 `ToolContributions` 快照：

```rust
let contributions = ToolContributions::new([
    zuno_tool::erase(ReviewStatusTool::new(service)),
])?;

let profile = zuno_harness::profile_with_tools(
    "review",
    Arc::new(DefaultAgentDriver),
    ToolManifest::standard(),
    contributions,
);
```

Profile 会同时发布可执行类型化服务，以及包含稳定接口 id、schema digest、owner、
provenance、generation 和 availability 的具名 capability descriptor。动态消费者使用
descriptor；原生 Rust 消费者使用类型化服务。

工具来源优先级依次是 built-in、harness contribution、MCP。后出现的同名来源会胜出，
并产生结构化 suppression diagnostic。除非替换是刻意且经过测试的，否则应避免碰撞。

### 替换 Agent Driver

`AgentDriver` 拥有一套完整回合驱动策略：

```rust
use futures::future::BoxFuture;
use zuno_engine::driver::AgentDriver;
use zuno_engine::r#loop::{
    RunTurnRequest, TurnContext, TurnError, TurnEventSender, TurnOutcome,
};

struct EvaluationDriver;

impl AgentDriver for EvaluationDriver {
    fn name(&self) -> &str {
        "evaluation"
    }

    fn drive<'a>(
        &'a self,
        request: RunTurnRequest,
        context: TurnContext<'a>,
        events: TurnEventSender,
    ) -> BoxFuture<'a, Result<TurnOutcome, TurnError>> {
        Box::pin(async move {
            run_evaluation_turn(request, context, events).await
        })
    }
}
```

使用 `AgentDriverComponent` 或 `zuno_harness::profile` 安装：

```rust
let profile = zuno_harness::profile(
    "evaluation",
    Arc::new(EvaluationDriver),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep])?,
);
```

替换 Driver 适用于拥有另一套完整回合策略的 benchmark、evaluation、workflow 或远程
harness。新增一个提示词定义的 Agent、专职角色或工具，不需要替换 Driver。

自定义 Driver 仍会收到原生 `RunTurnRequest`、`TurnContext`、`TurnEventSender` 与持久
存储。它必须保持所有客户端依赖的日志、中断、工具重放、人类等待和终态契约。修改默认
Driver 或循环时，必须在同一次变更中更新 [Harness 运行时](/zh/operate/harness-runtime)。

## 事务激活与恢复

`HarnessRuntime::activate_profile`、`mount`、`replace` 与 `unmount` 使用同一套切换算法：

1. 如果子作用域仍有活跃消费者，拒绝切换。
2. 针对 staging view prepare 完整候选；候选服务保持不可见。
3. Prepare 失败时丢弃所有未启动 effect，并恢复稳定 projection。
4. 停止旧 effect 前先撤销当前 capability 与服务。
5. 在有界超时内逆序停止旧组合。
6. 如果不能证明旧资源已清理，进入 `Uncertain` 并拒绝重叠。
7. 按顺序启动候选 effect。
8. 只有所有 start 都成功后，才原子发布全部候选服务与 capability。
9. 候选启动干净失败时，重新 prepare 并启动之前的定义。
10. 如果候选清理或恢复无法证明静默，带类型化生命周期诊断进入 `Failed` 或
    `Uncertain`。

绝不能隐藏清理错误，也不能把不确定副作用转换成成功。`RuntimeSnapshot` 是客户端使用的
生命周期、组件、effect、capability 和脱敏诊断清单。

## 受信进程工具

只有 WASI 无法暴露所需宿主 API 时才使用进程插件。它必须精确声明
`["host.full"]`，继承 Zuno 的进程环境与 OS 权限，并且永远是
`sideEffecting`、`replay: never`。

进程协议、containment 边界、取消、不确定结果、安全评审和完整 JavaScript 示例见
[受信进程插件开发](https://github.com/sunerpy/zuno/blob/main/docs/process-plugin-development.md)。

## 跨平台规则

Zuno 为多个操作系统和架构发布原生二进制。扩展必须说明自己的可移植边界：

- 声明式包只有在引用的 Skill 与 workflow 可移植时才可移植；
- WASI 组件是首选的可移植可执行边界，但每项已授权 WASI API 仍需在支持的宿主上测试；
- 进程包只有在 command、参数、运行时、文件系统假设与进程树清理都适配目标平台时
  才可移植；
- 原生 Rust 组件必须在每个支持目标上编译，OS 特有生命周期还需要原生执行证据。

不要把 Linux 进程语义、路径分隔符、可执行文件后缀、signal、权限位或 shell 语法当作
通用事实。交叉编译是有用证据，但不能替代 Windows 进程树、macOS 行为、终端集成或
OS 所有的凭据与文件系统边界上的原生执行。

## 完成标准

对于声明式 Agent：

- 校验包或配置；
- 检查 `zuno debug agent <name>`；
- 验证确切 `task` 阵容与权限上限；
- 通过一个真实客户端表面测试 workflow 或直接选择。

对于 WASI 或进程工具：

- 使用闭合 JSON Schema，并校验语义输入上限；
- 记录每项授权、环境变量、凭据来源与副作用；
- 测试初始化、调用、关停、取消、超时、非法输出和不确定清理；
- 验证 tool effect、replay、concurrency 与 UI intent 和实现一致；
- 安装并调用归档产物，而不只是构建目录里的二进制。

对于原生 Rust：

- 明确接口、Provider 与消费者所有权；
- 保持 `prepare` 无副作用；
- 每个 effect 返回一个精确 disposer；
- 测试替换、逆序清理、启动失败恢复、超时与不确定状态；
- 更新对应架构文档与客户端 projection；
- 先运行最小 crate 测试，再运行共享 workspace 门禁。

## 源码地图

当本指南与代码看起来不一致时，以这些实现点为准：

| 关注点 | 当前源码 |
| --- | --- |
| 包 schema 与校验 | `crates/zuno-extension/src/manifest.rs` |
| 静态发现与贡献合并 | `crates/zuno-extension/src/static_loading.rs`、`resolve.rs` |
| 扩展 revision 与 lease 事务 | `crates/zuno-extension/src/registry.rs` |
| 运行时工具代理与生命周期 bundle | `crates/zuno-extension/src/host.rs` |
| WASI 宿主 | `crates/zuno-extension/src/host/wasi.rs` |
| 进程宿主 | `crates/zuno-extension/src/host/process.rs` |
| 规范 WASI world | `wit/zuno-plugin/plugin.wit` |
| 原生组件运行时 | `crates/zuno-runtime/src/lib.rs` |
| Profile helper 与工具贡献 | `crates/zuno-harness/src/lib.rs` |
| 可替换 Agent Driver | `crates/zuno-engine/src/driver.rs` |
| Agent 目录与合并 | `crates/zuno-catalog/src/agent.rs` |
| 生效 Agent capability 快照 | `crates/zuno-agent/src/profile.rs` |
| CLI 组合根 | `crates/zuno-cli/src/cmd/turn.rs` |
| 可执行示例 | `examples/plugins/` |

其余公共 Zuno 能力由哪一份页面负责，见
[文档架构与覆盖地图](/zh/operate/documentation-coverage)。
