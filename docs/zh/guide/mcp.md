# MCP server

Model Context Protocol server 提供 Zuno 自身不附带的工具与资源。一个 server 要么作为子进程在本地启动，要么通过 HTTP 触达。无论哪种方式，它的工具都进入与原生工具相同的权限与授权路径；不存在第二套仅供插件使用的权限语言。

在注册任何东西之前，有一个默认值值得先知道：未知的 MCP 工具被视为有副作用。它失败即拒绝，而不是被假定安全。

## 注册一个 server

```sh
zuno mcp add my-server -- npx -y @modelcontextprotocol/server-filesystem /srv/data
zuno mcp add remote-server --url https://mcp.example.com --header "Authorization: Bearer $MCP_TOKEN"
zuno mcp list
```

本地 server 的命令写在 `--` 之后。`--env` 传递环境变量，`--header` 传递 HTTP 头。

## 或者直接配置

`mcp` 映射有三种形态：本地 server、远端 server，或者对另一配置层已定义的 server 的开关。

```json
{
  "mcp": {
    "filesystem": {
      "type": "local",
      "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/data"],
      "environment": { "LOG_LEVEL": "warn" },
      "cwd": "/srv/data",
      "enabled": true,
      "timeout": 5000
    },
    "docs": {
      "type": "remote",
      "url": "https://mcp.example.com",
      "headers": { "X-Tenant": "tenant-a" },
      "timeout": 5000
    },
    "inherited-server": {
      "enabled": false
    }
  }
}
```

| 字段 | 适用于 | 含义 |
| --- | --- | --- |
| `type` | 两者 | `local` 或 `remote` |
| `command` | local | 命令与参数，必填 |
| `cwd` | local | 工作目录；相对路径从工作区解析 |
| `environment` | local | server 进程的环境变量 |
| `url` | remote | server URL，必填 |
| `headers` | remote | 每次请求都发送的头 |
| `oauth` | remote | OAuth 设置，或用 `false` 抑制自动探测 |
| `enabled` | 两者 | 是否启动该 server |
| `timeout` | 两者 | 请求超时（毫秒）；运行时默认值是 5000 |

开关形态只接受 `enabled`，项目层就是这样在不重复定义的前提下禁用一个全局定义的 server。

## 认证

对于需要 OAuth 的远端 server：

```sh
zuno mcp auth remote-server
zuno mcp auth list
zuno mcp logout remote-server
```

OAuth 块控制整个流程：

```json
{
  "mcp": {
    "docs": {
      "type": "remote",
      "url": "https://mcp.example.com",
      "oauth": {
        "clientId": "zuno-local",
        "scope": "read:docs",
        "callbackPort": 19876
      }
    }
  }
}
```

| 字段 | 含义 |
| --- | --- |
| `clientId` | 客户端 id；缺省表示使用动态客户端注册（RFC 7591） |
| `clientSecret` | 客户端密钥，当授权服务器要求时使用 |
| `scope` | 要请求的 scope |
| `callbackPort` | 本地回调端口；运行时默认值是 19876 |
| `redirectUri` | 完整的 redirect URI，会覆盖 `callbackPort` |

对于一个公布了 OAuth 元数据但你不希望使用的 server，设置 `"oauth": false` 来抑制自动探测。对于静态 token，用一个头比走 OAuth 流程更简单，但请把值放在环境变量里，而不是 JSON 文件里。

## MCP 工具如何变为可用

注册不等于授权。要让模型调用一个 MCP 工具，四件事必须同时成立：

1. 该 server 已启用，并且连接成功。
2. 在配置了 Agent 确切 `tools` 允许列表的情况下，该工具的 wire id 在其中存活。
3. 没有显式权限规则拒绝它。
4. 对于被委派的回合，那个确切的 schema 在父级 Attempt 中可见。

由于未知的 MCP 操作默认为有副作用，授予一个经过审计的查询工具，并不会让某个 Agent 一并获得该 server 暴露的所有工具：

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "docs_search": "allow",
      "docs_write": "deny"
    }
  }
}
```

MCP 与扩展工具不会自动对每个只读 Agent 可用。具备工作能力的角色可以选择自动继承，只读 Agent 也可以在自己的规则中被显式授予一个经过审计的工具 id，但父级 schema 上限与确切 schema 校验仍然生效。参见 [Agent](/zh/guide/agents)。

## 并发与超时

```json
{
  "concurrency": {
    "mcp_connections": 8
  }
}
```

`mcp_connections` 限定跨*不同* server 的同时生命周期操作数。同一个 server 的操作保持串行，这避免了第二次连接与它自己的断开竞速。该字段接受 `1..=64`。

请求超时优先使用按 server 的 `timeout`。也存在一个实验性的全局 `experimental.mcp_timeout`。

## 工具没出现时

```sh
zuno mcp list
zuno mcp debug my-server
zuno debug agent build
zuno debug permissions
```

`mcp debug` 探测一个已注册的 server，从而把 server 故障与注册故障区分开。`debug agent` 显示该工具是否在 Agent 的能力过滤中存活，`debug permissions` 显示是否有规则拒绝了它。在怀疑 server 之前先检查这两项最省时间，因为被允许列表隐藏的工具，看起来和一个启动失败的 server 一模一样。

## MCP prompt

server 还可以暴露 prompt，它们会成为命令。Zuno 向 server 请求它的 prompt，并把每个声明的参数绑定为字面量 `"$1"`、`"$2"` 等，然后用你真实的参数填充这些占位符。参见 [Workflow 与命令](/zh/config/workflows)。

## 参见

- [zuno mcp](/zh/cli/mcp)
- [工具](/zh/guide/tools)
- [插件与扩展](/zh/guide/plugins)
- [配置项参考](/zh/config/reference)
