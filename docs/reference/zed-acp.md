# Use Zuno in Zed through ACP

Zuno exposes a native Agent Client Protocol (ACP) server over standard input
and standard output. Zed can launch that server as a custom external Agent.
The upstream Zed configuration contract is documented in
[External agents](https://zed.dev/docs/ai/external-agents); Zuno's implemented
protocol boundary and pinned upstream evidence are documented in
[Zed ACP integration](../design/zed-acp-integration.md).

## 1. Verify the installed Zuno binary

Locate the same binary that Zed should launch:

```sh
# Linux and macOS
command -v zuno
zuno acp --check
```

```powershell
# Windows PowerShell
(Get-Command zuno).Source
zuno acp --check
```

The check must complete without starting a session and print:

```text
ACP stdio adapter ready (protocol v1; schema v1.21.0)
```

If a terminal finds `zuno` but Zed does not, use the absolute path reported by
`command -v zuno` or `Get-Command zuno`. Desktop applications often receive a
different `PATH` from an interactive shell.

## 2. Add Zuno as a custom Zed Agent

Open Zed's Agent Panel, open Agent Settings, select **Add Agent**, then
**Add Custom Agent**. The equivalent Zed settings entry is:

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/absolute/path/to/zuno",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

An absolute executable path is the most reliable form. Examples:

### Linux

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/home/you/.local/bin/zuno",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

### macOS

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/Users/you/.local/bin/zuno",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

### Windows

JSON strings require escaped backslashes:

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "C:\\Users\\you\\.local\\bin\\zuno.exe",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

Do not wrap the command in a shell script that writes banners or status text
to stdout. ACP stdout contains only newline-delimited JSON-RPC frames.

## 3. Choose the Zuno configuration used by Zed

Zed sends the selected project as an absolute working directory. Zuno resolves
the same global and project `zuno.json`/`zuno.jsonc` chain it uses in the TUI:

- global configuration under the platform configuration root;
- project configuration from the worktree and `.zuno/` layers;
- configured Agent definitions, Skills, extensions, MCP servers, permissions,
  sandbox policy, providers, and models.

Provider login and credentials remain Zuno-owned. Configure and verify them
before starting the Zed Agent:

```sh
zuno debug config
zuno auth list
zuno models
```

Do not copy provider secrets into Zed settings merely to make ACP start. Use
Zuno's credential store or the provider environment variables described in
[Providers and credentials](providers.md).

To select an existing switchable configuration overlay for this Zed Agent,
set `ZUNO_CONFIG_DIR` in the custom Agent environment:

```json
{
  "agent_servers": {
    "Zuno (Kiro profile)": {
      "type": "custom",
      "command": "/absolute/path/to/zuno",
      "args": ["acp"],
      "env": {
        "ZUNO_CONFIG_DIR": "/home/you/.config/zuno/profiles/kiro"
      }
    }
  }
}
```

On Windows, use an escaped absolute path. Multiple Zed entries may launch the
same Zuno binary with different `ZUNO_CONFIG_DIR` overlays.

The `env` object on the Zed custom Agent launches the `zuno acp` process. Proxy
variables placed there therefore apply to every ordinary in-process session
request: providers, OAuth, remote MCP, remote catalogs, `webfetch`, and
`web_search`. For example:

```json
{
  "agent_servers": {
    "Zuno": {
      "type": "custom",
      "command": "/absolute/path/to/zuno",
      "args": ["acp"],
      "env": {
        "HTTPS_PROXY": "http://127.0.0.1:1080",
        "ALL_PROXY": "socks5h://127.0.0.1:1080",
        "NO_PROXY": "127.0.0.1,localhost,::1"
      }
    }
  }
}
```

An `env` object attached to one ACP-provided stdio MCP server belongs only to
that child process; it does not rewrite Zuno's process environment or other
session traffic.

### Opening the Agent Panel does not create history

ACP requires `session/new` to return a session id immediately, but Zuno keeps
that fresh identity process-local. Resolving MCP, commands, Skills, or changing
Agent, model, Mode, and reasoning before the first prompt does not insert a
Session row and the id is absent from `session/list`. The first accepted user
prompt or durable native command creates the Session and its first input in one
transaction. Closing an unused Agent Panel therefore leaves no “New Agent
Thread” in Zuno history.

## 4. Process loss, reconnect, and background work

ACP stdio is owned by the editor process that launched `zuno acp`. If that
process exits, the JSON-RPC transport and its live projection subscriptions are
gone. The durable session is not: messages, inbox rows, Goal, Plan, Todo, Job,
usage, prompt receipts, pending human requests, and retry deadlines remain in
SQLite.

After reconnect:

1. launch a fresh `zuno acp` process;
2. use the client's saved session id with `session/load`;
3. use `session/resume` only when the negotiated client/server capability exposes
   that operation;
4. let Zuno replay durable state and recover eligible pending continuation work.

Stable ACP v1 load is the baseline reconnection contract. A future or negotiated
resume operation can improve client ergonomics, but it does not replace durable
load. The background-TUI supervisor described in the TUI guide intentionally
retains PTYs and does not wrap ACP stdio; an ACP deployment that must remain
resident should use an editor-owned remote process, service manager, or another
transport supervisor that preserves the ACP process itself.

Peer-session messages are also durable. A root Agent may use `session_message` to
address another root in the same project. When this ACP session is active, the
message is driven as attributed plain-text peer context; when it is offline, the
row remains queued and is handled after load. Child Agents never receive the
sending tool.

## 5. Select `deep` or another session Agent

A new ACP session resolves Zuno's normal default Agent and model. Zuno then
publishes these session controls to Zed:

- **Mode**: Build or Plan;
- **Agent**: the available implementation Agents;
- **Model**: models from the resolved Zuno provider catalog;
- **Reasoning**: `Configured default` plus the canonical levels supported by
  the selected model, such as Low, High, Extra High, or Maximum.

Mode is authoritative for the Plan/Work boundary. The Agent selector contains
implementation Agents only. In Plan mode it selects the future Work Agent while
the durable mode remains read-only; a cold session does not start a `plan` host
merely because the selector changed. Whenever the session activates in Plan,
the host uses the read-only `plan` Agent. Zuno sends
`current_mode_update` and `config_option_update` after an accepted control so
both selectors show the durable state.

To use the directly selectable `deep` Agent:

1. create a Zuno external-Agent thread;
2. keep **Mode** set to **Build**;
3. open the **Agent** configuration selector and choose `deep`;
4. choose the desired model if the current Zuno profile exposes more than one;
5. choose a reasoning level when the selected model advertises reasoning.

Plan mode selects the read-only `plan` Agent for the next active turn. Mode
remains independent from the Agent selector: choosing an implementation Agent
while planning updates the saved Work Agent and does not exit Plan or wake a
cold runtime. `/start-work` or
`session/set_mode(build)` restores that Agent with the saved provider, model, and
reasoning settings after validating the exact handoff-ready Plan revision. Agent
and model changes are session-local and are rejected while the session has work
in flight unless they are the durable Start Work control queued for a safe point.
A dormant configuration change updates only durable identity and command state.
An active replacement prepares a candidate, atomically publishes its runtime
snapshot, and retires the prior one; optional MCP connections are reused only
when their complete connection identity still matches.
Structured logs report the reconfiguration phase timings without recording the
selected Agent, model, reasoning value, or credentials.

`zuno acp` does not accept an `--agent` launch argument. Agent selection is an
ACP session configuration operation, not a second process-level configuration
surface.

## 6. Slash commands and Skills

After session creation, loading, resuming, or a successful reconfiguration,
Zuno publishes native session controls, executable commands from its normal
command catalog, and unambiguous slash-invokable Skills. A running session also
publishes a fresh `available_commands_update` whenever the shared Skill catalog
generation changes, so installing, editing, deleting, or renaming a Skill does
not require restarting ACP. Zed then exposes the current set in `/` completion.

The sources are the same as other Zuno surfaces:

- native session controls with real runtime handlers: `/compact`, `/goal`,
  `/plan`, `/start-plan`, and `/start-work`;
- global `command/*.md` or `commands/*.md` under the Zuno config directory;
- project `.zuno/command/*.md` or `.zuno/commands/*.md`;
- built-in commands that have real handlers;
- discovered Skills whose names do not conflict with commands.

`/compact` accepts no arguments. It invokes the same durable compaction path as
the TUI, returns only after the command reaches a terminal lifecycle event, and
does not send the literal slash command to the model. Native controls take
precedence, so a user-defined command or Skill named `compact` is not published
as a second ambiguous entry.

`/goal` exposes the same durable goal handler as the TUI. With no arguments it
shows the current goal. `/goal <objective>` creates a goal when none exists or
the previous goal is complete or cancelled, and otherwise updates the current
objective without resetting its lifecycle state, budget, or usage. Explicit
`show`, `history`, `create <objective>`, `edit <objective>`, `pause`, `resume`,
`budget <positive tokens|none>`, `block <reason>`, `complete`, and `cancel`
actions remain available and take precedence when their name is the first token.

Objective changes also supersede
unfinished work by archiving the prior visible Plan and binding a fresh root
Plan to the current Goal for multi-stage work. An atomic objective never rebinds an
already terminal historical Plan; one that belongs to a previous Goal is archived as
completed history and the panel is cleared.

The command output is projected
as an ordinary Agent message rather than as reasoning. Invalid arguments to an
explicit action are returned as JSON-RPC invalid params, not as an internal
session error. A successful create or edit then advances the active Goal
immediately. On a fresh session Zuno durably admits the objective as the first
user turn anchor; the literal slash command never enters provider input.

`/plan` and `/start-plan` enter the read-only Plan mode idempotently.
`/start-work` is the only slash-command handoff to Build; it requires a durable
Plan whose current revision has a handoff-ready record. A bound Draft review is
refused unless the user supplies `--accept-draft-risk <reason>`. Successful
changes emit ACP `current_mode_update` and `config_option_update`
notifications, while the control itself starts a `UserControl` turn and never
becomes a synthetic user message.

ACP Plan updates are driven by durable work-state revisions, not by recognizing
the `plan_update` tool name. Each session subscribes to the active host, reads
the authoritative Plan after a change, and publishes a complete
`sessionUpdate: "plan"` snapshot. Zuno deduplicates by `(plan_id, revision)`,
may collapse a rapid burst to its newest revision, and flushes the final
revision before returning from a prompt. Removing a Plan sends empty entries so
Zed clears its previous panel. Load and resume project the current Plan through
the same path, and a host remount replaces the subscription without resetting
the revision cursor.

ACP has no native `superseded` status, so Zuno maps it to
`completed` and preserves the semantic outcome in
`_meta.zuno.outcome: "superseded"`. Each non-empty Plan snapshot also carries
`_meta.zuno.planId`, `revision`, `title`, and `stackDepth`, plus `goalId` when the Plan is
bound to a Goal and `parentPlanId` while a focused child Plan is visible, so a client can
tell a pushed child from a replaced root without diffing entries; every entry carries
`_meta.zuno.stepId`, and the clearing update carries only `_meta.zuno.cleared: true`.

Executing `/name arguments` uses Zuno's existing command-template or Skill
driver, including normal permission and durable-session behavior. ACP does not
create product-specific `/dual-review`, `/auto-release`, or other workflows;
users may define those in their own command or Skill directories.

## 7. Images, selection, branch diff, and attachments

Zuno advertises ACP `image` and `embeddedContext` support. In Zed this enables
image attachments and generic embedded context such as the current selection,
diagnostics, fetched context, and branch diff.

- Inline and embedded images support PNG, JPEG, GIF, and WebP, with valid
  base64 payloads up to 5 MiB.
- Embedded text resources keep their URI, MIME type, and text in the durable
  prompt envelope and are limited to 50 KiB and 2,000 lines each.
- Binary embedded resources other than images are rejected.
- Ordinary file references may arrive as `resource_link`; Zuno keeps those
  fields typed through durable storage and load replay.
- Audio remains unsupported and is not advertised.

The selected provider/model must also advertise image input. ACP capability
negotiation cannot make a text-only model accept an image.

## 8. Permissions, tools, diffs, and lifecycle

Zed presents permission and elicitation requests, but Zuno remains the policy
owner:

- Zuno permission rules decide whether a tool runs, is denied, or asks;
- reusable ACP asks offer `Allow once`, `Allow for session`, and `Reject`;
  a session grant is exact to the permission and resource patterns, survives
  Agent/model/reasoning remounts, and is cleared by `session/close`;
- strict or Shell-risk human-only asks offer only `Allow once` and `Reject`;
  effective `allow_all`, including `danger-full-access`, emits no permission
  request at all;
- Zuno's Shell sandbox controls filesystem and network authority;
- `edit`, `write`, and `apply_patch` share an `Editing files` card. Successful
  native mutations show only typed `A/M/D` diffs in visible content; the
  original success text remains available in `rawOutput`. A success without a
  diff keeps a short text fallback;
- a pre-write file failure shows actionable text and no fabricated diff.
  Partial or otherwise uncertain mutations are failed cards that retain any
  observed paths/diffs and `_meta.zuno.outcome: "uncertain"`;
- Zuno-configured MCP servers remain available when the selected Agent profile
  permits them;
- ACP-provided stdio and Streamable HTTP MCP servers are session-scoped and
  published only after the complete requested set connects and discovers;
- cancellation, session load, resume, close, plan state, usage, and tool
  history use the same durable runtime as the TUI.

Structured `question` calls use ACP form controls rather than a generic prompt:

- single-choice options are rendered from `oneOf`;
- multi-choice options are rendered as an array selection;
- when the question permits a custom answer, the choices remain clickable and
  a separate optional `Other` field is shown;
- submitting `Other` takes precedence over selected options, matching the Zuno
  TUI, while an empty optional form is reported as unanswered.

Questions are owned by the durable session, not the originating prompt RPC.
`question_async` permits optional follow-up while the Agent continues independent
work or finishes its summary. Native form cancellation defers without consent;
an explicit decline cancels. Null/empty acceptance is not an answer. Saved
`draftAnswers` remain separate from confirmed answers and are not sent to the
model. The client can list and respond through `questions/list` and
`questions/respond`, using stable item IDs, a command ID and expected revision.
Only a matching required answer can clear its human-wait gate.

When Zuno re-presents a question or permission left pending by an earlier
process and the client cannot be reached at all, nothing is recorded. The
request stays pending and answerable from the TUI or the HTTP API, this
`session/prompt` ends with `stopReason: end_turn`, and sending another prompt
can inspect the pending set again. Malformed responses also leave the request
untouched; they are not recorded as a user choice.

New tool results are receipts; accepted answers enter the durable inbox once.
The same service handles ordinary and Goal questions. `plan_exit` needs an
explicit typed approve decision for the displayed Plan/review/Work identity.
Early approval waits for successful source-cycle handoff; a normal compaction
recovery may change the engine turn ID without changing that logical cycle.
`/resume` resumes already-authorized paused Work, never a pending Plan approval.

Older stored tool results containing authoritative answer metadata still replay
as static answered cards, including their historical continuation markers.

After settlement and on historical replay, the question remains a static tool
card showing its prompt, choices, status, and—when durable answer metadata is
available—the selected values. `rawInput` and `rawOutput` remain available in
tool details; loading history never reopens an elicitation request.

Only provider reasoning deltas are projected into Zed's Thinking surface, with one
tagged exception: a Zuno-originated notice — a remote rule file that could not be fetched,
an intact local rule file skipped because it did not fit the prompt budget, a turn stopped
by its allowance, or a compaction requested by the budget or context policy — is sent as an
`agent_thought_chunk` whose `_meta.zuno.notice` carries `severity` (`info`, `warning`,
or `error`) and a stable code from the `instruction.*` or `budget.*` families.
A proactive threshold crossing in a long tool turn uses the separate
`context.compact` code. The tag is how a client tells it from model output; the
text is written for a person.
Generated titles use ACP `session_info_update`, and other operational status or
provider failure text is handled by lifecycle/error reporting rather than being
rendered as model thought.
Historical tool-declaration repair uses diagnostic audience: it is written to
structured logs and deliberately omitted from Zed thought chunks.

Once compaction succeeds, Zed receives the exact durable summary as an
`agent_message_chunk` tagged `_meta.zuno.kind: "compaction_summary"`. The live
automatic path emits it before retrying in the same host drive; load and resume
replay the same durable summary and tag. A successful internal compaction
therefore does not surface as a terminal turn failure or require a second prompt
to continue.

ACP projects the native `ContextUsageSnapshot` as an absolute `usage_update`,
with the full source-aware snapshot in `_meta.zuno.contextUsage`. Occupancy is
the latest provider-confirmed baseline plus estimated material not yet counted
by it, not the session's cumulative bill. Partial usage frames merge; cache and
reasoning subcounts are not added twice. A lower request estimate cannot replace
the confirmed baseline. Compaction advances the epoch, and load/resume uses the
same durable snapshot. Unknown counters or window sizes stay unknown.

Historical replay keeps provider reasoning capsules durable for future provider
requests, but does not render an exact capsule copy when the same message
already contains its visible reasoning summary. Provider-only reasoning remains
visible, so replay deduplication does not hide the only available thought.

Shell tool-call titles are the exact submitted command, not an interpreter-prefixed
pseudo-command. For example, Zed receives `git diff --check` as the copyable title
and receives the resolved `zsh` identity separately in
`_meta.zuno.interpreter`. Completion and historical replay preserve the same shape.

### Concurrent prompts in one session

A session runs one turn at a time, but a prompt sent while a turn is running is
never discarded. Zuno commits the prompt to the session's durable input inbox
first and decides who runs it second:

- normally the prompt is steered into the turn that is already running. The
  model receives it at that turn's next safe point — between provider requests,
  or after the tool calls in flight — and keeps working in the same turn;
- if the running turn ends before the steer lands, the prompt stays durably
  queued and the next turn in the session promotes it in admission order.

Standard `session/prompt` waits for the accepted input's associated processing
outcome. Busy acceptance is not an error and does not manufacture an immediate
`stopReason`. The native driver owns execution; RPC requests observe durable
receipts. A completed request returns a legal `stopReason` and
`_meta.zuno.receipt`. Acceptance, recording in history, inclusion in a provider
request, and execution completion are distinct states.

For retry-safe admission, supply a session-scoped client message ID:

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "session/prompt",
  "params": {
    "sessionId": "ses_x",
    "prompt": [{ "type": "text", "text": "Adjust the active work." }],
    "_meta": { "zuno": { "messageId": "client-message-42" } }
  }
}
```

The client ID must contain 1–256 bytes. Reusing it with the same content attaches
another observer to the existing input; conflicting reuse is rejected.
Identical text with different IDs is not deduplicated. Failure after admission
includes the receipt in error data, distinguishing it from genuine rejection.
An RPC disconnection does not withdraw already accepted input.

Zuno also exposes an extension success shape for clients that opt into it.
Initialize advertises `_meta.zuno.steering`, and every turn-scoped
`session/update` carries `_meta.zuno.turnId`. The client can send:

```json
{
  "method": "session/steer",
  "params": {
    "sessionId": "ses_x",
    "expectedTurnId": "turn_x",
    "messageId": "msg_optional",
    "prompt": [{ "type": "text", "text": "Adjust the active work." }]
  }
}
```

Success returns immediately with `turnId`, `inputId`, `admittedSequence`,
`admission: "steered"`, and `delivery: "steer"`. The expected-turn check and
queue insertion share the live-turn registry lock, so a handoff cannot steer a
successor turn. Rejections use `-32002`; `data.reason` is one of
`noActiveTurn`, `expectedTurnMismatch`, `activeTurnNotSteerable`, or
`emptyInput`. Commands remain idle-only. Standard `session/prompt` waits for
processing; `session/steer` is the immediate-admission interface.

A slash command is different. It is resolved against the host command catalog
and runs as its own turn, so it cannot be steered into work already in flight.
Zuno refuses it with a busy error, `admission: "rejected"`, and
`reason: "commandRequiresIdleSession"`, and writes nothing durable; send it
again once the session is idle. Only a prompt that actually names a command, an
unambiguous Skill, or a native session control is a command invocation. A prompt
that merely begins with `/` — an absolute POSIX path, a regular expression — is
ordinary content, so it is admitted durably and steered like any other prompt
rather than refused as an unresolvable command.

Cancellation is keyed by RPC request identity, never by equal text.
`$/cancel_request` withdraws that request's own still-pending contribution;
`-32800` reports the withdrawal and its durable receipt. It cannot erase input
already applied to a model request, and a duplicate-ID observer cannot withdraw
the original contributor's input. Use `session/cancel` for an explicit
session-wide interruption. Closing an observer, explicit withdrawal, and
terminating the Zuno process are separate lifecycle events.

### Delegated child sessions

The ordinary `task` tool is always the compatibility surface. Its card shows
the Agent, objective, state, and, when known, child session/job/model/effort
identity while retaining the raw tool details.

Zuno also supports the draft native-subagent projection used by the reviewed
official `codex-acp` adapter. It is enabled only when the ACP client sends the
direct initialize capability:

```json
"clientCapabilities": {
  "subagents": {}
}
```

When negotiated, foreground delegation is routed as a session tree:

- the parent receives `subagent_spawned`;
- the child session receives its own replay, prompt, messages, reasoning,
  tools, plan, and usage;
- the direct parent receives exactly one terminal
  `subagent_state_update` after child output drains.

Nested foreground children use their direct durable parent. Historical child
trees are restored on `session/load`, but their state is shown as
`disconnected` because a restarted process cannot prove that old work is still
live. Child-specific cancel/close are not advertised yet.

Background delegation deliberately stays on the stable task/job lifecycle,
even when native subagents were negotiated. Closing a root session cancels and
joins only that root's background jobs before releasing its runtime resources.

Permission requests raised by a child use the child session id only in native
mode. For clients without native-subagent support, Zuno sends the request on the
known root session and includes the durable child id at
`_meta.zuno.childSessionId`; this prevents a client from receiving an unknown
session id while preserving attribution. Delegated children do not receive the
`question` tool; they report blockers to the parent, which owns any subsequent
user elicitation on the root session.

ACP-provided MCP advertises stdio and Streamable HTTP; SSE remains unsupported.
Every new/load/resume request supplies the complete list. Names are validated or
stably slugged, stdio commands must be absolute and use the session directory as
cwd, and HTTP headers are strictly validated. The list remains process-local:
load and resume validate and freeze it without starting transports. On
activation, every ACP-provided server is required and eager; all must connect
and discover before publication, and partial startup is disposed in reverse
order. A host-configured optional server may remain transport-free when a cached
tool directory matches its complete connection identity; its first real tool
call starts one session-local connection through a singleflight gate. Commands,
environment values, and headers are never stored in SQLite or logs. Client
filesystem RPC and terminal RPC remain
unadvertised; Zuno handles file and Shell work through its own tools, permission
policy, and sandbox.

Restoring a thread is cold by default. `session/load` reconstructs the durable
transcript, Plan, usage, configuration, and command projection; `session/resume`
restores the durable control projection without replaying transcript content.
Neither operation starts TurnHost, MCP, plugin hosts, or the file watcher. The
first prompt, `/start-work`, `session/set_mode(build)`, or an active root Goal
activates the shared session exactly once. Concurrent load/resume requests for
one session id share the same registry entry and activation gate rather than
replacing one another. An active Goal is scheduled through the detached
continuation path without requiring another prompt and without manufacturing a
user message.
Load replay is bounded to the newest 512 retained messages, a 16 MiB stored-part
and total projection budget, and an 8 MiB per-update frame. Zuno emits an
omission notice when history exceeds those bounds. Stored part blobs are sized
in SQLite before JSON hydration, so an oversized tool output is not first loaded
into process memory and then discarded.

Historical file references are not trusted merely because they were durable.
Only existing regular files that canonicalize inside the project worktree
remain actionable as diff paths, locations, or local resource links. A missing,
external, or symlink-escaped local resource is displayed as non-actionable
explanatory text. One ACP stdio connection may retain at most 32 open sessions;
by default at most 8 may own an active resource-bearing runtime. An eligible
runtime sleeps after 15 idle minutes, releasing TurnHost, MCP, plugin processes,
watchers, and its active slot while retaining the durable session. Active turns
or Goals, queued/running/uncertain Jobs, pending reports or inputs, unresolved
human requests, and background commands prevent sleep. Capacity pressure sleeps
the least-recently-used eligible runtime first; otherwise activation waits up to
30 seconds and returns a typed retryable capacity error. `session/close`
releases the open slot and shuts down any active resources. These defaults are
configurable under `acp.runtime`.

## 9. Troubleshooting

### Agent fails to start

Run the exact configured command in a terminal:

```sh
/absolute/path/to/zuno acp --check
```

Check that the binary is executable, its configuration/data directories are
writable, and its configured provider can be resolved. An absolute command path
avoids most GUI `PATH` differences.

### Provider or model is missing

Run:

```sh
zuno debug config
zuno auth list
zuno models
```

If the Zed entry uses `ZUNO_CONFIG_DIR`, use the same environment while running
these commands. Project-specific configuration depends on the folder opened in
Zed.

### Protocol or tool stream is malformed

In Zed, run:

```text
dev: open acp logs
```

For temporary Zuno diagnostics, change the arguments to:

```json
"args": ["acp", "--print-logs", "--log-level", "DEBUG"]
```

`--print-logs` writes diagnostics to stderr. It does not place logs on ACP
stdout. Remove verbose logging after diagnosis.

### Opening a workspace repeatedly restores an old thread or consumes CPU

Closing or hiding Zed's Agent panel does not necessarily send
`session/close`. Zed may keep its external-Agent process and workspace thread
selection alive in the background.

Current Zuno versions keep one stable process-local entry per open session.
Repeated load/resume requests share it and remain cold unless an active Goal
requires continuation. Load replay is bounded and explicit, stale actionable
file paths are filtered, and one connection defaults to 32 open sessions but
only 8 active resource-bearing runtimes. Provider or tool activity immediately
after restoration therefore means an active Goal resumed, not that panel
rendering started a host.

If the problem persists:

1. run `dev: open acp logs` and confirm whether Zed is repeatedly issuing
   `session/load` or reopening the same session;
2. close the external-Agent thread, not only the panel, or stop and restart the
   configured Agent server so its stdio process reaches EOF;
3. if Zed immediately selects the same known-bad thread after restart, clear
   that workspace's last active Agent-thread association using the maintenance
   procedure for the installed Zed version, after backing up Zed state;
4. inspect Zed logs separately for repeated worktree, watcher, or
   `OpenBufferByPath` activity. Zuno does not own or remove Zed-created
   worktrees and filesystem watchers.

An eligible idle runtime sleeps automatically while its durable session remains
open. If memory or child-process use stays high, inspect active Goals, Jobs,
pending inbox or human-request rows, and background commands; each intentionally
blocks sleep.

### Agent or model selector is absent

Confirm Zed connected successfully, then create a new external-Agent thread.
Run `zuno acp --check` to verify the production adapter, and inspect the ACP
logs for initialization or session-creation errors.

### A Kiro prompt fails with `unsupported_content_block_projection`

The 2026-08-28 `kiro-provider` build accepts consecutive all-text blocks and
concatenates them byte-for-byte with no inserted separator only at Kiro's final
scalar text boundary. Use:

```json
"retry": {
  "max_attempts": 3,
  "recovery_window_ms": 660000
},
"options": {
  "baseURL": "http://127.0.0.1:8787/v1",
  "maxTokens": null,
  "timeout": false,
  "headerTimeout": 330000,
  "chunkTimeout": 330000,
  "reasoningReplay": "encrypted",
  "reasoningReplayMaxAge": 86400000
}
```

Remove a stale `responsesTextBlocks: "single"` option: Zuno's generic
compatibility mode inserts one blank line and would alter the current
provider's exact projection. Mixed text and non-text blocks whose ordering Kiro
cannot preserve still fail closed. If pure text still produces the old error,
verify that Zed is reaching the newly built provider process.

`chunkTimeout` deliberately exceeds kiro-provider's configured 300-second
stream-idle deadline. The retry window begins only after that typed failure
returns, leaving enough time for two bounded replacement attempts.

`reasoningReplay: "encrypted"` opts the route into sealed reasoning replay. An ACP
session is where this matters most, because the editor drives long multi-step
turns: without it the gateway reports `reasoning_replay_locked: false` on every
request and each step starts without the previous step's reasoning. The provider
entry around these options has to declare `"transport": "openai"` and
`"surface": "responses"`, or the config is refused with the offending key path:
its endpoint comes from `baseURL`, which resolves to Chat Completions on its own.
Confirm replay from the `session.provider.request` event's
`replayedReasoningCapsules`, which rises above zero from the second request of a
session onward.

`kiro-provider` v0.5.0 and later also distinguish retryable stream failures from
fatal protocol failures. Zuno retries only
`upstream_stream_error`, `upstream_stream_incomplete`,
`upstream_stream_idle_timeout`, `malformed_upstream_tool_arguments`, and
`request_deadline_exceeded`; the legacy generic `upstream_error` is not enough
to authorize retry. Providers should use
`malformed_upstream_tool_arguments` only when a completed model tool-call
argument payload is invalid JSON. Structural tool-call violations continue to
use the terminal `invalid_upstream_tool_call` code. Every call is recorded under
`session.provider.attempt.1`, and the same durable session affinity is reused.
Because ACP cannot retract an appended message chunk, Zuno commits provider
text, reasoning, and pending tool rows only after the attempt is checkpointed.
A failed partial attempt is discarded instead of being concatenated with its
replacement. A durable accepted-question result is not attempt-scoped: its
`in_progress` continuation marker survives `RetryRollback`, and its terminal
tool update is emitted with the successful replacement checkpoint.

## 10. Acceptance checks

After configuration:

1. open a real project folder in Zed and create a Zuno Agent thread;
2. select `deep`, the intended model, and `xhigh` or `max`, then confirm the
   choice is shown in the session controls;
3. type `/`, confirm `/compact`, `/goal`, `/plan`, `/start-plan`, and
   `/start-work` each appear exactly once;
4. execute `/goal verify ACP shorthand`, `/goal edit verify ACP actions`, and
   `/goal show`; confirm the result appears as Agent output rather than Thinking;
5. execute `/start-plan`, confirm Zed switches to Plan, then create and patch a
   durable Plan; confirm the bottom Plan panel appears and updates before the
   prompt completes without reverting to an older revision, then execute
   `/start-work`;
6. after enough conversation history exists, execute `/compact` and confirm the
   summary survives a session reload;
7. execute one configured command or unambiguous Skill;
8. attach an image, selection, and branch diff and confirm they reach the turn;
9. while still in Plan, answer a structured question and confirm its tool row
   stays `in_progress` until the continuation checkpoint, then becomes
   `completed` before the next assistant output; inject one retryable stream
   failure and confirm the failed partial attempt is absent while the
   accepted-question marker remains;
10. delegate a foreground child and confirm either the negotiated child-session
    stream or the complete stable task card, depending on client capability;
11. delegate a background child, close the root thread, and confirm the job is
    cancelled without a foreground native-child stream;
12. request one file edit under an ask policy and confirm Zed displays both the
    permission request and an `Editing files` card containing only the typed
    diff, with no duplicate success sentence; also confirm a pre-write failure
    has no fabricated diff and an uncertain mutation reports failed status plus
    observed paths;
13. cancel a running prompt and confirm the session returns to idle;
14. close and reload the session and confirm content, question/task cards,
   child history, tools, plan, and usage are replayed once;
15. load the same open session again and confirm the transcript is not duplicated.

Repository-level ACP verification is:

```sh
cargo test -p zuno-acp
cargo test -p zuno --test acp_stdio
```
