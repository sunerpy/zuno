# zuno serve

`zuno serve` 启动无头服务器，让外部客户端通过 HTTP 而不是终端来驱动 harness。当使用方是
编辑器、GUI、另一个服务，或者一个说服务器 API 而非 CLI 的脚本时，就用它。

服务器 owner 在进程内包装 `zuno_server::ServerBuilder`。它不会派生一个单独的 `zuno-server`
可执行文件，也不会重复实现监听器行为。

监管方只持有一个进程。它派生的那个进程就是正在监听的进程，因此停止这个进程就会释放端口并
关闭它的管道。参见[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)。

`ZUNO_SERVER_PASSWORD` 启用 HTTP Basic Auth；`ZUNO_SERVER_USERNAME` 默认是 `zuno`。没有非空密码时，只要 hostname 解析结果中包含非回环地址，Zuno 就拒绝监听。

这两个变量都不会交给 `shell` 工具，因此模型组装出来的命令读不到保护这个监听器的凭据。
参见[工具](/zh/guide/tools#一条-shell-命令继承什么)。

`--browser-auth` 是为本地浏览器显式启用的独立模式。即使同时配置了 Basic Auth，也只有全部解析地址都是回环时才接受。启动时会打印一个含 256-bit 单次 token 的 bootstrap URI；交换成功后设置 30 天有效、绑定 authority 的签名 `HttpOnly; SameSite=Strict; Path=/` Cookie，并 303 跳转到 `/health`。访问日志看不到 token query。Basic 凭据或浏览器 Cookie 任一有效即可授权；使用 Cookie 的非安全方法还必须携带与当前 authority 完全匹配的 `Origin`。

## 用法

```sh
zuno serve [OPTIONS]
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--port <PORT>` | 监听端口。不传时由 `server.port` 决定；该键也未设置时使用操作系统分配的端口 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--hostname <HOSTNAME>` | 绑定的 hostname。不传时由 `server.hostname` 决定；该键也未设置时使用 `127.0.0.1` | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--browser-auth` | 启用单次回环浏览器 bootstrap 与签名 session Cookie | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

`--port` 与 `--hostname` 都没有默认值。存在命令行参数时以它为准，其次是配置里的
`server.port` 与 `server.hostname`，最后才是内置的 `127.0.0.1` 与操作系统分配的端口。
`--port 0` 是显式要求一个由系统分配的端口，因此它与不传该参数是可区分的。配置里超出
`0`-`65535` 的端口会被指名拒绝，而不是被截断。

## 示例

在回环接口上启动服务器，端口由操作系统分配。

```sh
zuno serve
```

固定端口，好让客户端配置指向一个稳定地址。

```sh
zuno serve --port 4096
```

启动一个回环浏览器 session。打开输出中标为 `Browser authentication` 的 URI；它在本次进程启动中只能使用一次。

```sh
zuno serve --port 4096 --browser-auth
```

非回环部署使用 Basic Auth。

```sh
ZUNO_SERVER_USERNAME=zuno \
ZUNO_SERVER_PASSWORD='replace-with-a-secret' \
  zuno serve --hostname 192.0.2.10 --port 4096
```

在排查连接失败的客户端时，在 stderr 上观察服务器自身的日志流。

```sh
zuno serve --port 4096 --print-logs --log-level DEBUG
```

`--mdns`、`--mdns-domain` 与 `--cors` 在早先的版本里可以被接受，但每一次给出它们的调用都会
被拒绝 —— 因为它们背后没有任何实现。现在它们完全不再被接受：脚本传入其中之一会在解析阶段就
失败，而不是在进程启动之后。

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno acp](/zh/cli/acp)
- [已排除的命令](/zh/cli/excluded)
- [配置参考](/zh/config/reference)
- [Zed ACP 集成](/zh/guide/editors)
- [日志](/zh/operate/logging)
