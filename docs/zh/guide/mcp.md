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

## ACP session server

ACP 客户端可以在 new/load/resume 时提供完整的 session-local `mcpServers` 列表。这里支持 stdio 与 Streamable HTTP，不支持 SSE。这些 server 不会写进 `zuno.json`：它们是经过校验、只存在于进程内的 effect，全部连接并完成 discovery 后工具才会发布，部分启动按逆序清理。客户端 command、environment 值与 header 永不写入 session 数据库或日志。参见 [zuno acp](/zh/cli/acp)。

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

### 渐进式 schema 发现

通过上述四道门禁意味着 MCP 工具可执行，并不意味着 Zuno 必须在每次 provider 请求中
注入所有已连接 schema。对于没有 Agent 确切 `tools` 允许列表的根回合，匹配的 MCP
schema 默认隐藏在 `tool_search` 后。该工具搜索 id、显示名称与描述；匹配项会累积加入
下一次 provider step，不能在发现它们的同一批 assistant 工具调用中立即调用。

这只改变提示词暴露，不改变授权：搜索无法恢复被权限拒绝或被能力过滤移除的工具。Agent
确切 `tools` 允许列表被视为有意选择 schema，其中点名的 MCP 工具保持立即可见。被委派
的子级只接收父级 Attempt 已记录的确切 schema，不能通过搜索获得更大的目录。

ACP session-local `mcpServers` 同样属于客户端显式契约。严格连接门禁成功后，它们的
schema 会出现在第一次 provider 请求中；同一会话里的宿主配置 MCP server 仍采用渐进式
发现。该区分会随会话传递到子回合与后台续跑。

provider 请求快照记录搜索后的确切工具 schema；搜索结果也会把匹配 id 与单调递增的
目录 revision 写入持久工具结果。如果另一个已注册工具已经定义了 `tool_search`，Zuno
不会遮蔽它：该回合保持 schema 立即可见，并由宿主发出警告。

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

MCP 调用失败按「失败在哪」分类，而不是只看「失败了」。5xx、408、429 响应以及连接被断开属于可恢复失败，回合会带退避重试。其余 4xx —— 400、401、403、404、405 —— 以及 OAuth 失败（例如你设了 `"oauth": false` 的 server）则直接阻塞，因为在配置改变之前，同样的请求只会同样地失败。

客户端自己的超时被当作第三类，因为请求可能已经在 server 上生效了。远端工具代理声明自己不可重放，所以一次超时的调用会带回一条恢复提示：副作用可能已经发生，在权威外部状态证明它没有完成之前不得重放。两个资源列举工具声明自己可以安全重放，因为 `resources/list` 不改变任何状态。

stdio 分帧有上限。单个 JSON-RPC 帧最大 64 MiB，是一个 server 合法能发出的最大 base64 资源块的四倍以上。超长帧会让所有待响应的调用以协议错误失败并关闭该流，因为在帧中间被截断的流无法重新同步。server 的 stderr 按每行 8 KiB 截断，而不是当作致命错误，因为它没有待响应的调用方。

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
