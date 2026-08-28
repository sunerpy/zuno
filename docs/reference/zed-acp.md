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

## 4. Select `deep` or another session Agent

A new ACP session resolves Zuno's normal default Agent and model. Zuno then
publishes these session controls to Zed:

- **Mode**: Build or Plan;
- **Agent**: the available implementation Agents;
- **Model**: models from the resolved Zuno provider catalog;
- **Reasoning**: `Configured default` plus the canonical levels supported by
  the selected model, such as Low, High, Extra High, or Maximum.

To use the directly selectable `deep` Agent:

1. create a Zuno external-Agent thread;
2. keep **Mode** set to **Build**;
3. open the **Agent** configuration selector and choose `deep`;
4. choose the desired model if the current Zuno profile exposes more than one;
5. choose a reasoning level when the selected model advertises reasoning.

Plan mode always activates the read-only `plan` Agent. Returning to Build mode
restores the selected implementation Agent. Agent and model changes are
session-local and are rejected while a prompt is actively running.

`zuno acp` does not accept an `--agent` launch argument. Agent selection is an
ACP session configuration operation, not a second process-level configuration
surface.

## 5. Slash commands and Skills

After session creation, loading, resuming, or a successful reconfiguration,
Zuno publishes native session controls, executable commands from its normal
command catalog, and unambiguous slash-invokable Skills. Zed then exposes them
in `/` completion.

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

`/goal` exposes the same durable goal handler as the TUI. It accepts
`show`, `history`, `create <objective>`, `edit <objective>`, `pause`, `resume`,
`block <reason>`, `complete`, and `cancel`; omit the action to show the current
goal. The command output is projected as an ordinary Agent message rather than
as reasoning.

`/plan` toggles between Build and Plan. `/start-plan` enters the read-only Plan
mode directly, while `/start-work` returns to Build. Leaving Plan requires a
durable plan, so an early handoff fails explicitly instead of weakening the
mode boundary. Successful changes emit ACP `current_mode_update` and
`config_option_update` notifications, keeping Zed's selectors synchronized.
None of these native commands is sent to the model.

Executing `/name arguments` uses Zuno's existing command-template or Skill
driver, including normal permission and durable-session behavior. ACP does not
create product-specific `/dual-review`, `/auto-release`, or other workflows;
users may define those in their own command or Skill directories.

## 6. Images, selection, branch diff, and attachments

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

## 7. Permissions, tools, diffs, and lifecycle

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
- native file tools emit typed creation and edit diffs for Zed;
- Zuno-configured MCP servers remain available when the selected Agent profile
  permits them;
- cancellation, session load, resume, close, plan state, usage, and tool
  history use the same durable runtime as the TUI.

Structured `question` calls use ACP form controls rather than a generic prompt:

- single-choice options are rendered from `oneOf`;
- multi-choice options are rendered as an array selection;
- when the question permits a custom answer, the choices remain clickable and
  a separate optional `Other` field is shown;
- submitting `Other` takes precedence over selected options, matching the Zuno
  TUI, while an empty optional form is reported as unanswered.

After completion and on historical replay, the question remains a static tool
card showing its prompt, choices, status, and—when durable answer metadata is
available—the selected values. `rawInput` and `rawOutput` remain available in
tool details; loading history never reopens an elicitation request.

Only provider reasoning deltas are projected into Zed's Thinking surface.
Generated titles use ACP `session_info_update`, and operational status or
provider failure text is handled by lifecycle/error reporting rather than being
rendered as model thought.

Shell tool-call titles are the exact submitted command, not an interpreter-prefixed
pseudo-command. For example, Zed receives `git diff --check` as the copyable title
and receives the resolved `zsh` identity separately in
`_meta.zuno.interpreter`. Completion and historical replay preserve the same shape.

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

Permissions and questions raised by a child use the child session id only in
native mode. For clients without native-subagent support, Zuno sends the request
on the known root session and includes the durable child id at
`_meta.zuno.childSessionId`; this prevents a client from receiving an unknown
session id while preserving attribution.

ACP-provided client MCP, client filesystem RPC, and terminal RPC are not
advertised. Zuno handles file and Shell work through its own tools, permission
policy, and sandbox rather than claiming Zed client RPC handlers.

Restoring a thread is deliberately cold. `session/load` and `session/resume`
validate the session, expose its selectors, and publish commands without
starting a `TurnHost` or connecting configured MCP servers. The first prompt
performs that activation. Load replay is sent once per open ACP session and is
bounded to the newest 512 retained messages, a 16 MiB stored-part and total
projection budget, and an 8 MiB per-update frame. Zuno emits an omission notice
when history exceeds those bounds. Stored part blobs are sized in SQLite before
JSON hydration, so an oversized tool output is not first loaded into process
memory and then discarded.

Historical file references are not trusted merely because they were durable.
Only existing regular files that canonicalize inside the project worktree
remain actionable as diff paths, locations, or local resource links. A missing,
external, or symlink-escaped local resource is displayed as non-actionable
explanatory text. One ACP stdio connection may retain at most 32 open sessions;
`session/close` releases the slot and shuts down any activated host and MCP
runtime.

## 8. Troubleshooting

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

Current Zuno versions make a restored session dormant until its first prompt,
deduplicate repeated load replay, bound transcript replay, filter stale
actionable file paths, and cap one ACP connection at 32 open sessions. These
protections keep Zuno from eagerly reconnecting MCP servers or replaying an
unbounded historical transcript merely because Zed restored a thread.

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

An already activated but idle Zuno session remains mounted until Zed closes it
or the ACP process exits. Zuno does not currently demote active sessions on an
idle timer.

### Agent or model selector is absent

Confirm Zed connected successfully, then create a new external-Agent thread.
Run `zuno acp --check` to verify the production adapter, and inspect the ACP
logs for initialization or session-creation errors.

### A Kiro prompt fails with `unsupported_content_block_projection`

The 2026-08-28 `kiro-provider` build accepts consecutive all-text blocks and
concatenates them byte-for-byte with no inserted separator only at Kiro's final
scalar text boundary. Use:

```json
"options": {
  "baseURL": "http://127.0.0.1:8787/v1",
  "maxTokens": null
}
```

Remove a stale `responsesTextBlocks: "single"` option: Zuno's generic
compatibility mode inserts one blank line and would alter the current
provider's exact projection. Mixed text and non-text blocks whose ordering Kiro
cannot preserve still fail closed. If pure text still produces the old error,
verify that Zed is reaching the newly built provider process.

## 9. Acceptance checks

After configuration:

1. open a real project folder in Zed and create a Zuno Agent thread;
2. select `deep`, the intended model, and `xhigh` or `max`, then confirm the
   choice is shown in the session controls;
3. type `/`, confirm `/compact`, `/goal`, `/plan`, `/start-plan`, and
   `/start-work` each appear exactly once;
4. execute `/goal create verify ACP`, then `/goal show`, and confirm the result
   appears as Agent output rather than Thinking;
5. execute `/start-plan`, confirm Zed switches to Plan, then create a durable
   plan and execute `/start-work`;
6. after enough conversation history exists, execute `/compact` and confirm the
   summary survives a session reload;
7. execute one configured command or unambiguous Skill;
8. attach an image, selection, and branch diff and confirm they reach the turn;
9. send a read-only repository question and confirm reasoning
   and tool updates stream incrementally;
10. delegate a foreground child and confirm either the negotiated child-session
    stream or the complete stable task card, depending on client capability;
11. delegate a background child, close the root thread, and confirm the job is
    cancelled without a foreground native-child stream;
12. request one file edit under an ask policy and confirm Zed displays both the
   permission request and typed diff;
13. cancel a running prompt and confirm the session returns to idle;
14. close and reload the session and confirm content, question/task cards,
   child history, tools, plan, and usage are replayed once;
15. load the same open session again and confirm the transcript is not duplicated.

Repository-level ACP verification is:

```sh
cargo test -p zuno-acp
cargo test -p zuno-cli --test acp_stdio
```
