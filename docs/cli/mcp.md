# zuno mcp

Model Context Protocol servers extend Zuno with tools and resources it does not ship. `zuno mcp`
registers those servers, handles the ones that require authentication, and probes a server
that is misbehaving so you can see whether the fault is in the server or in the registration.

`add` accepts either a local command, given after `--`, or a remote URL, and can pass
environment variables and HTTP headers through to it.

## Synopsis

```sh
zuno mcp [OPTIONS] <COMMAND>
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Subcommands

| Subcommand | Description |
| --- | --- |
| [`add`](#zuno-mcp-add) | |
| [`list`](#zuno-mcp-list) | |
| [`auth`](#zuno-mcp-auth) | |
| [`logout`](#zuno-mcp-logout) | |
| [`debug`](#zuno-mcp-debug) | |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno mcp add

```sh
zuno mcp add [OPTIONS] [NAME] [-- <SERVER_COMMAND>...]
```

| Argument | Description |
| --- | --- |
| `[NAME]` | |
| `[SERVER_COMMAND]...` | |

| Option | Description | Default |
| --- | --- | --- |
| `--url <URL>` | | |
| `-v`, `--version` | Show the Zuno package version | |
| `--env <ENV>` | | |
| `--header <HEADER>` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno mcp list

```sh
zuno mcp list [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno mcp auth

```sh
zuno mcp auth [OPTIONS] [NAME] [COMMAND]
```

| Argument | Description |
| --- | --- |
| `[NAME]` | |

| Nested command | Description |
| --- | --- |
| `list` | |
| `help` | Print this message or the help of the given subcommand(s) |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

`zuno mcp auth list` accepts only the options shared by every command.

```sh
zuno mcp auth list [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno mcp logout

```sh
zuno mcp logout [OPTIONS] [NAME]
```

| Argument | Description |
| --- | --- |
| `[NAME]` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno mcp debug

```sh
zuno mcp debug [OPTIONS] <NAME>
```

| Argument | Description |
| --- | --- |
| `<NAME>` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

List the MCP servers this installation knows about.

```sh
zuno mcp list
```

Register a local server launched as a child process, passing its command after `--`.

```sh
zuno mcp add my-server -- npx -y @modelcontextprotocol/server-filesystem /srv/data
```

Register a remote server and send a bearer token with every request.

```sh
zuno mcp add remote-server --url https://mcp.example.com --header "Authorization: Bearer $MCP_TOKEN"
```

Probe one registered server when its tools do not appear.

```sh
zuno mcp debug my-server
```

## See also

- [Global options](/cli/global-options)
- [zuno plugin](/cli/plugin)
- [zuno providers](/cli/providers)
- [Configuration reference](/reference/configuration)
- [Plugins](/plugins)
