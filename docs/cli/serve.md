# zuno serve

`zuno serve` starts the headless server so external clients drive the harness over HTTP
instead of through a terminal. Use it when the consumer is an editor, a GUI, another
service, or a script that speaks the server API rather than the CLI.

The server owner wraps `zuno_server::ServerBuilder` in-process. It does not spawn a
separate `zuno-server` executable and does not duplicate listener behavior.

Binding to `0.0.0.0` or advertising the listener over mDNS exposes it beyond the local
host. The server does not add authentication on your behalf, so restrict the bind address
and CORS origins to what the deployment actually needs.

## Synopsis

```sh
zuno serve [OPTIONS]
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--port <PORT>` | | `0` |
| `-v`, `--version` | Show the Zuno package version | |
| `--hostname <HOSTNAME>` | | `127.0.0.1` |
| `--mdns` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--mdns-domain <MDNS_DOMAIN>` | | `zuno.local` |
| `--cors <CORS>` | | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Start the server on the loopback interface with an operating-system assigned port.

```sh
zuno serve
```

Pin a port so a client configuration can point at a stable address.

```sh
zuno serve --port 4096
```

Watch the server's own log stream on stderr while diagnosing a client that fails to connect.

```sh
zuno serve --port 4096 --print-logs --log-level DEBUG
```

Advertise the listener over mDNS under a custom domain for discovery on a trusted network.

```sh
zuno serve --port 4096 --mdns --mdns-domain zuno.local
```

## See also

- [Global options](/cli/global-options)
- [zuno acp](/cli/acp)
- [Excluded commands](/cli/excluded)
- [Configuration reference](/reference/configuration)
- [Zed ACP integration](/reference/zed-acp)
- [Logging](/logging)
