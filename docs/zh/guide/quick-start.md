# 快速开始

从零到第一次成功运行。一共五步，其中最常失败的两步是 provider 配置和沙箱探测，所以把它们放在前面。

## 1. 确认二进制文件及其路径

```sh
zuno --version
zuno debug paths
```

`debug paths` 打印这个可执行文件解析出的各个根目录。`config` 那一行就是 `zuno.json` 应该放的位置；下面的内容都基于这一点。

## 2. 在依赖沙箱之前先验证它

```sh
zuno debug sandbox --mode workspace-write --check
```

它运行的是 Shell 所用的同一个后端：先检查启动器的归属与可信性，然后通过真实的 bubblewrap、能力丢弃和 seccomp 路径执行一次探测。当策略无法部署时，`--check` 以失败退出。

如果失败，默认行为是拒绝 Shell，而不是用高于配置请求的权限继续运行。在 Linux 上常见
原因是 bubblewrap 版本低于 0.8.0，或者策略禁止非特权用户命名空间。

如果这台宿主本来就无法提供沙箱，请做出以下一种受信选择：

```sh
# 始终使用宿主原生后端。
zuno run --sandbox danger-full-access "run the local build"

# 优先使用 workspace-write 约束，仅在符合条件的不可用错误下降级。
zuno run \
  --sandbox workspace-write \
  --sandbox-on-unavailable run-unconfined \
  "run the local build"
```

降级形式只适用于具备写能力的 `workspace-write` Agent。没有受限后端时，只读 Agent 仍会
拒绝。在尚未实现约束后端的 macOS 与 Windows 上，这两种受信选择都可以让具备写能力的
Agent 原生运行；`danger-full-access` 会完全跳过沙箱探测。参见
[权限与沙箱](/zh/guide/permissions)。

## 3. 配置一个 provider

Zuno 不自带任何默认模型 id。在配置根目录下的 `zuno.json` 中声明 provider、它的传输方式及其模型：

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
```

```json
{
  "$schema": "https://raw.githubusercontent.com/sunerpy/zuno/main/schemas/zuno.json",
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY"],
      "options": {
        "baseURL": "https://gateway.example.com/v1"
      },
      "models": {
        "primary-model": {
          "name": "Primary model",
          "reasoning": true,
          "tool_call": true,
          "limit": { "context": 200000, "output": 32000 }
        },
        "fast-model": {
          "name": "Fast model",
          "tool_call": true,
          "limit": { "context": 128000, "output": 16000 }
        }
      }
    }
  }
}
```

`transport` 指定原生 Rust 线协议实现，`surface` 选择 `responses`、`chat` 或 `messages`。两者都不会加载 npm 包，也不会启动 Node。`myopenai` 只是一个普通的 provider id，不是保留名。

## 4. 保存凭据

```sh
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
```

管道登录从标准输入读取；交互式登录会关闭终端回显。无论哪种方式，密钥都不会进入 shell 历史。凭据落在 `$XDG_DATA_HOME/zuno/auth.json`，Unix 上权限为 `0600`。

对于内置的 `openai` provider，先问清楚有哪些方法再做选择：

```sh
zuno providers methods openai
zuno providers login openai --method api-key
```

在 `provider.<id>.env` 下声明的环境变量会被直接使用，绝不会复制进凭据存储，所以一个 provider 可以在不执行任何登录命令的情况下就已完成认证。参见 [Provider 与凭据](/zh/config/providers)和[认证](/zh/config/authentication)。

## 5. 确认模型目录，然后运行

```sh
zuno debug config
zuno models myopenai --verbose
```

`debug config` 打印合并后的配置，并指出任何被拒绝的键，这是发现某个值放错文件的最快方式。`models` 确认 `run` 与 `tui` 期望的那个确切 `provider/model` 标识符。

先跑一个只读的：

```sh
zuno run --agent plan "summarize how configuration precedence works in this repository"
```

`plan` 是只读的：不注册任何写入类工具，并且它的契约会把沙箱钉在 `read-only`，与配置
无关。在约束后端可用的宿主上，它是端到端确认整条路径能走通的最安全方式。只读 Agent
刻意不会使用 `run-unconfined`。

现在做真正的工作：

```sh
zuno run "add pagination to the /users endpoint and run the tests"
```

或者启动终端应用，这也是不带参数的 `zuno` 所做的事：

```sh
zuno
```

## 首次运行常见故障

| 现象 | 原因 | 修复 |
| --- | --- | --- |
| `no trusted system bubblewrap executable was found` | 没有约束后端 | 安装 bubblewrap 0.8.0 或更新版本、显式使用 `danger-full-access`，或为具备写能力的 Agent 启用受信的不可用降级 |
| `OS sandbox is not implemented for platform` | 在 macOS 或 Windows 上使用受约束模式 | 显式使用 `danger-full-access`、为具备写能力的 Agent 启用受信的 `run-unconfined` 降级，或在 Linux 上运行 |
| 校验错误指出某个被拒绝的顶层键 | 仅 TUI 使用的键（如 `theme`）写进了 `zuno.json` | 把它移到 `tui.json`。参见[配置文件与优先级](/zh/config/files) |
| 切换构建后会话列表为空 | 源码构建与发布构建打开的是不同的数据库文件 | 参见[数据库生命周期](/zh/operate/migration) |
| 找不到某个模型 id | 目录在该 provider 添加之前就已缓存 | `zuno models --refresh` |

## 参见

- [你的第一个会话](/zh/guide/first-session)
- [配置总览](/zh/config/)
- [Provider 与凭据](/zh/config/providers)
- [权限与沙箱](/zh/guide/permissions)
