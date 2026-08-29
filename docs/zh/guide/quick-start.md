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

如果失败，现在就修。一个无法被证明的受限沙箱模式会拒绝启动会话；它不会降级为无约束执行。在 Linux 上常见原因是 bubblewrap 版本低于 0.8.0，或者策略禁止非特权用户命名空间。参见[权限与沙箱](/zh/guide/permissions)。

在 macOS 与 Windows 上受约束后端尚未实现，因此受限模式会报告平台不受支持。这些宿主目前需要显式使用 `--sandbox danger-full-access`，这是一个刻意的信任决定，而不是绕过手段。

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

`plan` 是只读的：不注册任何写入类工具，并且它的契约会把沙箱钉在 `read-only`，与配置无关。它是端到端确认整条路径能走通的最安全方式。

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
| `no trusted system bubblewrap executable was found` | 没有约束后端 | 安装 bubblewrap 0.8.0 或更新版本，然后重新运行 `zuno debug sandbox --check` |
| `OS sandbox is not implemented for platform` | 在 macOS 或 Windows 上使用受约束模式 | 刻意使用 `--sandbox danger-full-access`，或者在 Linux 上运行 |
| 校验错误指出某个被拒绝的顶层键 | 仅 TUI 使用的键（如 `theme`）写进了 `zuno.json` | 把它移到 `tui.json`。参见[配置文件与优先级](/zh/config/files) |
| 切换构建后会话列表为空 | 源码构建与发布构建打开的是不同的数据库文件 | 参见[数据库生命周期](/zh/operate/migration) |
| 找不到某个模型 id | 目录在该 provider 添加之前就已缓存 | `zuno models --refresh` |

## 参见

- [你的第一个会话](/zh/guide/first-session)
- [配置总览](/zh/config/)
- [Provider 与凭据](/zh/config/providers)
- [权限与沙箱](/zh/guide/permissions)
