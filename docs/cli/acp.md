# zuno acp

`zuno acp` speaks Agent Client Protocol over stdin and stdout. Editors that support ACP
launch the executable as a child process and exchange framed messages on the pipes, so
there is no port to bind and no HTTP surface to secure. This is the integration path for
Zed and other ACP clients.

Because the protocol owns stdout, do not read that stream as human output. Use `--check`
when you only want to confirm the adapter is present, and `--print-logs` to route
diagnostics to stderr where they will not corrupt the protocol stream.

The editor launches one process and keeps it. That process is the one serving the
protocol, so terminating it ends the session and its pipes reach end of file — see
[One invocation, one process](/cli/#one-invocation-one-process).

Opening Zuno in an Agent Panel calls ACP `session/new`, which reserves an
in-process session id and resolves configuration, commands, Skills, and MCP. An
ordinary unused panel remains ephemeral. A durable native collaboration control,
such as entering Plan mode, materializes the Session because its mode, Goal pause,
and future Work identity must survive restart.

## Agent, Mode, Plan, and file projection

Mode and Agent are independent durable choices. **Mode** owns the Plan/Work
boundary. The Agent selector contains only implementation Agents; while Plan is
active it updates the saved Work Agent without leaving the read-only `plan`
host. Model and reasoning changes update the same future Work identity.

`/plan` and `/start-plan` enter Plan idempotently. `/start-work` and
`session/set_mode(build)` invoke the same atomic authorization: the exact Plan
revision must be handoff-ready, a bound review must be Ready unless the user
supplies `--accept-draft-risk <reason>`, and the saved Agent/provider/model/
reasoning identity is restored. Idle sessions start immediately; busy sessions
queue the durable control for the next safe point.

Idle Agent, model, Mode, and reasoning changes atomically replace the turn host.
The session keeps its connected MCP runtime when the resolved MCP server set and
connection concurrency are unchanged, avoiding an unnecessary network or
subprocess handshake. Structural MCP changes still reconnect. Reconfiguration
logs include phase timings but omit selected values and credentials.

A `session/prompt` that arrives while the session is already running a turn is
committed to the durable input inbox first and then steered into that turn, so
the model receives it without interrupting the running work. That second request
is answered with JSON-RPC error `-32001`; its `data` reports `admission`
(`steered`, `queued`, or `rejected`), `sessionId`, and the durable `inputId`; the
streamed output and the `stopReason` stay on the request that owns the turn.

A slash command cannot be steered and is refused with
`reason: "commandRequiresIdleSession"` and nothing durable written; only text
that resolves to a real command, Skill, or native control counts as a slash
command, so a prompt that merely starts with `/` is admitted as ordinary content.

Withdrawing a prompt request with `$/cancel_request` before it returns cancels
the durable row that request admitted, so the withdrawn text never reaches the
model, and answers that request with `-32800` and `data.admission: "withdrawn"`.
See [Zed ACP integration](/reference/zed-acp) for the full shape.

Background completion is different from a slash command. Terminal commands,
subagents, workflows, and product Agents publish a deterministic completion
envelope and wake the parent automatically. Synchronous `bg wait` is capped at
60 seconds and competes with callback delivery for one durable owner, preventing
duplicate turns.

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

Operational notices — a remote rule file that could not be fetched or an intact local
rule file skipped because it did not fit the prompt budget (its rules are not in force
while the turn proceeds), a turn stopped by its
token, tool-call, or wall-clock allowance, or a compaction requested by the budget or
context policy —
are projected as `agent_thought_chunk` updates tagged `_meta.zuno.notice` with
`severity` (`info`, `warning`, or `error`) and a stable `code` such as
`instruction.not_in_force`, `budget.compact`, or `budget.token_budget`.
A proactive threshold crossing during a long turn uses the separate
`context.compact` code. The tag is how a client distinguishes them from model
output; they are never part of the transcript the model sees.

After compaction succeeds, ACP receives the exact persisted summary as an
`agent_message_chunk` tagged `_meta.zuno.kind: "compaction_summary"`. Automatic
mid-turn compaction publishes it before retrying inside the same host drive, so
the editor does not first receive a terminal prompt failure or wait for another
wake. Historical load/resume projects the same durable summary with the same
tag.

For a known model context window, `ProviderRequestStarted` immediately publishes an
ACP `usage_update` with the assembled prompt estimate. When provider token usage
arrives, a second update replaces it with the measured value. These are absolute
request-occupancy updates rather than cumulative percentages, so a request after
compaction immediately recalculates the editor's context indicator.

## Goal continuation

`/goal <objective>` is a native control followed by autonomous execution. Zuno
persists the typed command result, then immediately advances the active Goal
through the shared driver. On a fresh session the objective is admitted through
the durable inbox as the first user turn; the literal slash command is never
sent to the provider.

An automatic Goal turn uses the Agent and model selected on the current ACP
host. The newest real user message remains only its causal transcript anchor and
grants no authority. Reconfiguring from `deep` to `orchestrator`, or selecting
another model, does not require another prompt and does not rewrite that
message. The exact trigger, Goal revision, anchor, Agent, provider, and model are
persisted in `session.turn.started.1`.

`session/load` and `session/resume` rebuild the session runtime and automatically
resume an active root Goal. No extra prompt is required, including for a session
written by 0.6.0 that contains an active Goal but no user message, or for a
compacted session whose retained tail starts with an assistant message.

`/goal budget <positive tokens|none>` changes one Goal's explicit ceiling.
`goal_update` accepts `in_progress` and `active` only as idempotent confirmation
of an already-active Goal; a paused or blocked Goal still requires the user-owned
`/goal resume` control.

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
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
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
