# zuno serve

`zuno serve` starts the headless server so external clients drive the harness over HTTP
instead of through a terminal. Use it when the consumer is an editor, a GUI, another
service, or a script that speaks the server API rather than the CLI.

The server owner wraps `zuno_server::ServerBuilder` in-process. It does not spawn a
separate `zuno-server` executable and does not duplicate listener behavior.

`ZUNO_SERVER_PASSWORD` enables HTTP Basic authentication;
`ZUNO_SERVER_USERNAME` defaults to `zuno`. Without a non-empty password, Zuno
refuses a hostname that resolves to any non-loopback address.

`--browser-auth` is a separate explicit opt-in for local browser use. It is
accepted only when every resolved listener address is loopback, even if Basic
Auth is also configured. Startup prints one bootstrap URI containing a
single-use 256-bit token. Successful exchange sets a 30-day, authority-bound,
signed `HttpOnly; SameSite=Strict; Path=/` cookie and redirects to `/health`.
The token query is removed from request logging. Basic credentials or the
browser cookie may authorize a request; unsafe cookie-authorized methods also
require an exact matching `Origin`.

## Synopsis

```sh
zuno serve [OPTIONS]
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--port <PORT>` | Port to listen on. Absent means `server.port` decides, and an unset key means an operating-system assigned port | |
| `-v`, `--version` | Show the Zuno package version | |
| `--hostname <HOSTNAME>` | Hostname to bind. Absent means `server.hostname` decides, and an unset key means `127.0.0.1` | |
| `--mdns` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--mdns-domain <MDNS_DOMAIN>` | | `zuno.local` |
| `--cors <CORS>` | | |
| `--browser-auth` | Enable one-time loopback browser bootstrap and signed session cookies | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

Neither `--port` nor `--hostname` has a default. The flag wins when it is present, then
`server.port` and `server.hostname` from configuration, and only then the built-in
`127.0.0.1` and an operating-system assigned port. `--port 0` is an explicit request for
an assigned port, which is why it is distinguishable from omitting the flag. A configured
port outside `0`-`65535` is refused by name rather than truncated.

## Examples

Start the server on the loopback interface with an operating-system assigned port.

```sh
zuno serve
```

Pin a port so a client configuration can point at a stable address.

```sh
zuno serve --port 4096
```

Start a loopback browser session. Open the one URI printed as `Browser authentication`;
it works only once for that process launch.

```sh
zuno serve --port 4096 --browser-auth
```

Use Basic Auth for a non-loopback deployment.

```sh
ZUNO_SERVER_USERNAME=zuno \
ZUNO_SERVER_PASSWORD='replace-with-a-secret' \
  zuno serve --hostname 192.0.2.10 --port 4096
```

Watch the server's own log stream on stderr while diagnosing a client that fails to connect.

```sh
zuno serve --port 4096 --print-logs --log-level DEBUG
```

`--mdns`, `--mdns-domain`, and `--cors` are reserved but are not implemented by
the current Rust server runtime.

## See also

- [Global options](/cli/global-options)
- [zuno acp](/cli/acp)
- [Excluded commands](/cli/excluded)
- [Configuration reference](/reference/configuration)
- [Zed ACP integration](/reference/zed-acp)
- [Logging](/logging)
