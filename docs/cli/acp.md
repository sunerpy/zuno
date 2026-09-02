# zuno acp

`zuno acp` speaks Agent Client Protocol over stdin and stdout. Editors that support ACP
launch the executable as a child process and exchange framed messages on the pipes, so
there is no port to bind and no HTTP surface to secure. This is the integration path for
Zed and other ACP clients.

Because the protocol owns stdout, do not read that stream as human output. Use `--check`
when you only want to confirm the adapter is present, and `--print-logs` to route
diagnostics to stderr where they will not corrupt the protocol stream.

## Goal continuation

`/goal <objective>` is a native control followed by autonomous execution. Zuno
persists the typed command result, then immediately advances the active Goal
through the shared driver. On a fresh session the objective is admitted through
the durable inbox as the first user turn; the literal slash command is never
sent to the provider.

`session/load` and `session/resume` rebuild the session runtime and automatically
resume an active root Goal. No extra prompt is required, including for a session
written by 0.6.0 that contains an active Goal but no user message.

## Session MCP servers

Zuno advertises standard ACP MCP support for stdio and Streamable HTTP;
legacy SSE remains unsupported. `session/new`, `session/load`, and
`session/resume` must provide the complete `mcpServers` list for that session.
Load and resume never reuse process resources from an earlier request.

Declarations are validated before the session is published:

- names must match `[A-Za-z0-9_-]{1,32}` or are normalized to a stable slug
  with an eight-character digest; duplicate normalized names are rejected;
- a stdio command must be absolute and runs with the session directory as cwd;
- HTTP endpoints must be absolute HTTP(S) URLs;
- environment and header entries are strictly validated, including
  case-insensitive duplicate header names.

Every ACP session owns an isolated profile bundle. All requested servers must
connect and complete tool discovery before any of their tools are published.
Partial startup is shut down in reverse order. Session close, load failure,
process exit, and profile replacement use the same exact disposer path.

Client MCP commands, environment values, and HTTP headers remain process-local:
they are not written to the session database or diagnostics. Tool schemas and
actual tool attempts continue through the ordinary durable tool rules.

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
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
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
