# zuno acp

`zuno acp` speaks Agent Client Protocol over stdin and stdout. Editors that support ACP
launch the executable as a child process and exchange framed messages on the pipes, so
there is no port to bind and no HTTP surface to secure. This is the integration path for
Zed and other ACP clients.

Because the protocol owns stdout, do not read that stream as human output. Use `--check`
when you only want to confirm the adapter is present, and `--print-logs` to route
diagnostics to stderr where they will not corrupt the protocol stream.

## Agent, Mode, Plan, and file projection

The Agent selector includes `plan`. `active_agent` is the authoritative state:
selecting `plan` switches to Plan mode, while selecting `build`,
`orchestrator`, `deep`, or another implementation Agent switches back to Build
mode. The inverse Mode change selects the corresponding Agent and publishes
both `current_mode_update` and `config_option_update`.

Idle Agent, model, Mode, and reasoning changes atomically replace the turn host.
The session keeps its connected MCP runtime when the resolved MCP server set and
connection concurrency are unchanged, avoiding an unnecessary network or
subprocess handshake. Structural MCP changes still reconnect. Reconfiguration
logs include phase timings but omit selected values and credentials.

Plan projection is driven by durable work-state revisions, not by recognizing a
`plan_update` tool call. Each session reads and publishes the authoritative
complete Plan after a change, deduplicates by `(plan_id, revision)`, flushes the
final revision before prompt completion, and emits empty entries when the Plan
is removed. Load, resume, detached Goal continuation, and host remount share the
same projector.

`edit`, `write`, and `apply_patch` use one `Editing files` card. A successful
typed mutation shows only its structured add/modify/delete diff in visible
content while preserving the complete original result in `rawOutput`.
Pre-write failures show actionable text without a fabricated diff. Partial or
otherwise uncertain mutations remain failed, preserve observed paths or diffs,
and carry `_meta.zuno.outcome: "uncertain"`. Live delivery and replay use the
same policy.

Operational notices — a remote rule file that could not be fetched (its rules are not in
force while the turn proceeds), a turn stopped by its
token, tool-call, or wall-clock allowance, a compaction the budget policy requested —
are projected as `agent_thought_chunk` updates tagged `_meta.zuno.notice` with
`severity` (`info`, `warning`, or `error`) and a stable `code` such as
`instruction.not_in_force`, `budget.compact`, or `budget.token_budget`. The tag is how
a client distinguishes them from model output; they are never part of the transcript
the model sees.

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
Configuration changes inside one active session do reuse its connected MCP
runtime when the effective MCP configuration is unchanged.

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
