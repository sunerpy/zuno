# zuno acp

`zuno acp` speaks Agent Client Protocol over stdin and stdout. Editors that support ACP
launch the executable as a child process and exchange framed messages on the pipes, so
there is no port to bind and no HTTP surface to secure. This is the integration path for
Zed and other ACP clients.

Because the protocol owns stdout, do not read that stream as human output. Use `--check`
when you only want to confirm the adapter is present, and `--print-logs` to route
diagnostics to stderr where they will not corrupt the protocol stream.

## Synopsis

```sh
zuno acp [OPTIONS]
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--check` | Validate that the production ACP adapter is available, then exit | |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Confirm the production ACP adapter is available in this build, then exit.

```sh
zuno acp --check
```

Serve the protocol on stdin and stdout, the way an editor launches it.

```sh
zuno acp
```

Serve the protocol while mirroring diagnostics to stderr, so protocol framing on stdout
stays intact.

```sh
zuno acp --print-logs --log-level DEBUG
```

## See also

- [Global options](/cli/global-options)
- [zuno serve](/cli/serve)
- [Zed ACP integration](/reference/zed-acp)
- [Logging](/logging)
