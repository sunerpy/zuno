# HTTP API 与 OpenAPI

`zuno serve` 暴露与 TUI、ACP adapter 相同的 Session、工具、持久 event 和运行时。
它是进程内的客户端入口，不是第二套 Agent 实现。

本页负责已经发布的 HTTP 契约。运行中进程的 OpenAPI 文档是机器可读清单；Session、附件、
保留策略和权限页面负责 schema 无法表达的行为。

## 启动 Server

另一个进程需要稳定地址时，固定端口：

```sh
zuno serve --hostname 127.0.0.1 --port 4096
curl http://127.0.0.1:4096/health
```

未传参数时先使用 `server.hostname` 与 `server.port`。这些配置也不存在时，监听
`127.0.0.1`，端口由操作系统分配。启动输出会给出实际地址。

`GET /health` 返回纯文本：

```text
ok
```

`GET /api/health` 返回 JSON：

```json
{"healthy":true}
```

参数优先级、日志和进程生命周期见 [`zuno serve`](/zh/cli/serve)。

## 认证与暴露边界

没有 `ZUNO_SERVER_PASSWORD` 的 Server 只能绑定到全部解析结果都是 loopback 的地址。
只要一个解析地址不是 loopback，Zuno 就拒绝启动。对外暴露前设置非空密码：

```sh
ZUNO_SERVER_USERNAME=zuno \
ZUNO_SERVER_PASSWORD='replace-with-a-secret' \
  zuno serve --hostname 192.0.2.10 --port 4096

curl --user 'zuno:replace-with-a-secret' \
  http://192.0.2.10:4096/api/location
```

只有在 `ZUNO_SERVER_USERNAME` 不存在时，用户名才默认为 `zuno`。Basic Auth 启用后，
每条路由都要求有效凭据。失败响应是 `401`，并带
`WWW-Authenticate: Basic realm="Secure Area"`。

`--browser-auth` 是另一种仅 loopback 可用的模式。启动时打印一个带单次 token 的 URI。
`GET /auth/browser` 是唯一可以在没有现有凭据时交换 token 的路由。交换成功后跳转到
`/health`，并设置绑定 authority、有效期 30 天的签名
`HttpOnly; SameSite=Strict; Path=/` Cookie。Cookie 授权的非安全方法还必须携带完全匹配的
`Origin`。请求日志记录前会移除 bootstrap query。

Basic 用户名与密码不会传给模型组装的 Shell 命令。HTTP 认证只保护 listener，不会扩大
Agent 的工具、权限或 sandbox authority。

## OpenAPI 文档

运行中的进程发布 OpenAPI 3.1：

- `GET /openapi.json`
- `GET /doc`
- `GET /api/doc`

三条路由返回相同 JSON，并遵守与其他路由相同的认证规则。文档版本来自当前 Zuno package，
因此客户端应从实际调用的进程获取，不要复制其他 release 的文件。

```sh
curl --silent http://127.0.0.1:4096/openapi.json > zuno-openapi.json
```

路由清单是精确的：只有真实 handler 存在时才加入 operation。当前文档没有完整标注每个 body。
其中有 32 个已复核的 body-schema 缺口，包括 SSE、WebSocket、原始文件、catalog、learning、
prompt 与 history 响应；另有 7 个 operation 刻意没有 body。生成的 client 必须保留未知 JSON
字段，并为缺少 request 或 `200` response schema 的 operation 手工建模。

Zuno 不发布旧的源码树 `generate` 命令。请从运行中的 `/openapi.json` 使用自选工具生成 client，
再对同一 Zuno 版本测试。

## Endpoint 清单

`{sessionID}`、`{requestID}`、`{providerID}`、`{integrationID}` 与 `{ptyID}` 是路径参数。
`*path` 是当前项目目录下的 wildcard 路径。

### 进程与 Catalog

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `GET` | `/health` | 纯文本进程健康状态 |
| `GET` | `/api/health` | JSON 进程健康状态 |
| `GET` | `/api/location` | 当前目录与项目 identity |
| `GET` | `/api/event` | 进程级实时 SSE 通知 |
| `GET` | `/api/agent` | 解析后的 Agent catalog |
| `GET` | `/api/command` | 解析后的 command catalog |
| `GET` | `/api/skill` | 解析后的 Skill catalog |
| `GET` | `/api/reference` | 解析后的 reference catalog |
| `GET` | `/api/model` | 解析后的 model catalog |
| `GET` | `/api/provider` | Provider 列表 |
| `GET` | `/api/provider/{providerID}` | 单个 Provider |
| `GET` | `/api/integration` | Integration 列表 |
| `GET` | `/api/integration/{integrationID}` | 存在时返回一个 Integration |

Catalog 与 location 响应携带当前目录上下文。可选 backend 不可用时可能返回
`503 backend_unavailable`；Zuno 不会为了固定返回该错误而注册 placeholder operation。

### 文件系统

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `GET` | `/api/fs/read/{*path}` | 按 content type 读取一个文件的字节 |
| `GET` | `/api/fs/list` | 列出当前项目下的目录 |
| `GET` | `/api/fs/find` | 在当前项目下查找条目 |

路径不能离开当前 Session 目录。`read` 对超过 32 MiB 的文件返回
`413 file_too_large`。`find` 响应包含 `truncated`；它为 `true` 时，没有匹配只表示有界遍历
在停止前没有找到，不代表路径不存在。遍历与并发上限见[无界面运行](/zh/guide/headless)。

### Session 与回合

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `GET` | `/api/session` | 列出 Session |
| `POST` | `/api/session` | 创建 Session |
| `GET` | `/api/session/active` | 列出当前进程中的活跃 Session |
| `GET` | `/api/session/{sessionID}` | 读取一个 Session |
| `GET` | `/api/session/{sessionID}/context` | 读取当前 context item |
| `GET` | `/api/session/{sessionID}/history` | 读取持久 history |
| `GET` | `/api/session/{sessionID}/message` | 分页读取 Message |
| `GET` | `/api/session/{sessionID}/learning` | 读取持久 learning projection |
| `GET` | `/api/session/{sessionID}/memory-policy` | 读取 Session Memory policy |
| `PUT` | `/api/session/{sessionID}/memory-policy` | 按 revision 更新 Memory policy |
| `GET` | `/api/session/{sessionID}/event` | 通过 SSE 重放并跟随持久 Session event |
| `POST` | `/api/session/{sessionID}/agent` | 在安全宿主边界切换 Agent |
| `POST` | `/api/session/{sessionID}/model` | 在安全宿主边界切换模型 |
| `POST` | `/api/session/{sessionID}/prompt` | 准入 Prompt 与可选文件 |
| `POST` | `/api/session/{sessionID}/compact` | 请求持久压缩 |
| `POST` | `/api/session/{sessionID}/wait` | 等待 Session host 结算 |
| `POST` | `/api/session/{sessionID}/interrupt` | 中断活跃回合；空闲 Session 不变 |
| `POST` | `/api/session/{sessionID}/revert/stage` | 暂存一次 snapshot 恢复 |
| `POST` | `/api/session/{sessionID}/revert/clear` | 清除已暂存恢复 |
| `POST` | `/api/session/{sessionID}/revert/commit` | 提交已暂存恢复 |

Memory policy 更新接受 `useMemories`、`generation`（`enabled` 或 `disabled`）与
`expectedRevision`。`excluded` 由宿主管理，客户端不能请求。Revision 过期、Session 已被排除，
或活跃回合持有 Session 时返回 `409`，不会覆盖当前状态。

`POST .../prompt` 使用与 TUI、ACP 相同的持久 inbox。HTTP 响应不是客户端私有回合；event 与
projection 仍可被其他已连接客户端看到。

### 人工问题与权限

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `GET` | `/api/permission/request` | 列出当前位置内待处理的持久权限请求 |
| `GET` | `/api/session/{sessionID}/permission` | 列出一个 Session 的待处理权限请求 |
| `POST` | `/api/session/{sessionID}/permission/{requestID}/reply` | 结算一条权限请求 |
| `GET` | `/api/question/request` | 列出当前位置内待处理的持久问题 |
| `GET` | `/api/session/{sessionID}/question` | 列出一个 Session 的待处理问题 |
| `POST` | `/api/session/{sessionID}/question/{requestID}/reply` | 原子结算问题并准入模型可见答案 |
| `POST` | `/api/session/{sessionID}/question/{requestID}/reject` | 拒绝待处理问题 |

Pending 行能在进程重启后继续存在，实时 channel 只通知消费者。已经结算的请求不能通过第二次
答复准入重复输入。

### Session 维护

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `GET` | `/api/session/prune` | 预览 archive 或 delete 选择 |
| `POST` | `/api/session/prune` | 应用已确认的 archive 或 delete mutation |

变更前先预览。删除需要显式确认，还要选择如何处理派生学习；完整契约见
[Session 保留](/zh/operate/session-retention)。

### 伪终端

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `GET` | `/api/pty` | 列出 PTY Session |
| `POST` | `/api/pty` | 创建 PTY Session |
| `GET` | `/api/pty/{ptyID}` | 读取 PTY 元数据 |
| `PUT` | `/api/pty/{ptyID}` | 更新 PTY 状态，包括尺寸 |
| `DELETE` | `/api/pty/{ptyID}` | 停止并移除 PTY |
| `POST` | `/api/pty/{ptyID}/connect-token` | 签发有作用域、单次使用的连接 token |
| `GET` | `/api/pty/{ptyID}/connect` | Upgrade 到 PTY WebSocket stream |

连接 token 不能替代 HTTP 认证。请求先通过 Server 认证边界，token 再把一个 WebSocket
attachment 限定到一个 PTY。

## Server-sent event

`GET /api/event` 是进程级实时流。第一条 event 是 `server.connected`，不重放持久 history。
Subscriber 落后时，Zuno 发送带 `action: "reconnect"` 的 `server.stream.lagged`。

`GET /api/session/{sessionID}/event` 是持久流。连接后先重放已存 Session event，再无重复地
跨过捕获的实时边界，随后跟随新 event。普通 SSE message 格式如下：

```text
event: message
id: ses_...:42
data: {"id":"evt_...","type":"...","durable":{"aggregateID":"ses_...","seq":42,"version":1},"data":{...}}
```

保存 SSE `id`，重连时通过 `Last-Event-ID` 送回。Cursor 格式为
`<sessionID>:<非负序号>`，且只对签发它的 Session 有效。无效或跨 Session cursor 返回
`400 invalid_event_request`；不存在的 Session 返回 `404 not_found`，而不是打开空流。

实时 Session subscriber 落后时，终态 `server.stream.lagged` event 带 `lastCursor`，随后
关闭该流。使用最后确认的 cursor 重连，从 SQLite 继续。两个流都会发送 `heartbeat`
keep-alive，并设置 `Cache-Control: no-cache` 与 `X-Accel-Buffering: no`。

```sh
curl -N \
  -H 'Accept: text/event-stream' \
  -H 'Last-Event-ID: ses_01abc:42' \
  http://127.0.0.1:4096/api/session/ses_01abc/event
```

## JSON 与错误约定

许多成功 JSON 响应使用 `{ "data": ... }` envelope。列表和 location endpoint 可能添加分页或
目录字段；存在绑定 schema 时以运行中的 OpenAPI 为准，并保留未知字段。

大多数失败使用：

```json
{
  "error": {
    "code": "invalid_request",
    "message": "the actionable detail"
  }
}
```

常见 code 包括 `invalid_request`、`forbidden`、`not_found`、`conflict`、
`backend_unavailable`、`database_error`、`filesystem_error`、`file_too_large` 与
`event_stream_failed`。客户端不要按 message 文本分支。

两个兼容场景使用 tagged body：Provider 不存在时返回 `_tag: "ProviderNotFoundError"`；
缺少必填 query key 时返回 `_tag: "InvalidRequestError"`。调用这些 operation 的 client 必须
同时接受 tagged body 与普通 envelope。

## 刻意不存在的接口

- 不存在无认证的非 loopback 模式。
- CLI 不接受 `--cors`、`--mdns` 与 `--mdns-domain`。
- 不存在无 `/api` 前缀的 `/event` alias；使用 `/api/event`。
- 已保存的 `always` 权限属于进程/Session 状态，不是持久权限行，因此没有 saved-permission
  list 或 revoke endpoint。
- 真实 handler 不存在时，不注册 operation。

## 相关页面

- [`zuno serve`](/zh/cli/serve)
- [无界面运行](/zh/guide/headless)
- [Session 与回合](/zh/guide/sessions)
- [图像与文件引用](/zh/guide/attachments)
- [权限与沙箱](/zh/guide/permissions)
- [客户端接口架构（英文）](/design/client-interfaces)
