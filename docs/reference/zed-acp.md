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
- **Model**: models from the resolved Zuno provider catalog.

To use the directly selectable `deep` Agent:

1. create a Zuno external-Agent thread;
2. keep **Mode** set to **Build**;
3. open the **Agent** configuration selector and choose `deep`;
4. choose the desired model if the current Zuno profile exposes more than one.

Plan mode always activates the read-only `plan` Agent. Returning to Build mode
restores the selected implementation Agent. Agent and model changes are
session-local and are rejected while a prompt is actively running.

`zuno acp` does not accept an `--agent` launch argument. Agent selection is an
ACP session configuration operation, not a second process-level configuration
surface.

## 5. Permissions, tools, diffs, and lifecycle

Zed presents permission and elicitation requests, but Zuno remains the policy
owner:

- Zuno permission rules decide whether a tool runs, is denied, or asks;
- Zuno's Shell sandbox controls filesystem and network authority;
- native file tools emit typed creation and edit diffs for Zed;
- Zuno-configured MCP servers remain available when the selected Agent profile
  permits them;
- cancellation, session load, resume, close, plan state, usage, and tool
  history use the same durable runtime as the TUI.

ACP-provided client MCP, client filesystem RPC, and terminal RPC are not
advertised. Prompt text and `resource_link` content are supported; prompt image,
audio, and embedded-resource capabilities are currently not advertised.

Restoring a thread is deliberately cold. `session/load` and `session/resume`
validate the session and expose its selectors without starting a `TurnHost` or
connecting configured MCP servers. The first prompt performs that activation.
Load replay is sent once per open ACP session and is bounded to the newest 512
retained messages, a 4 MiB transcript budget, and 1 MiB per transcript update.
Zuno emits an omission notice when history exceeds those bounds. Stored part
blobs are sized in SQLite before JSON hydration, so an oversized tool output is
not first loaded into process memory and then discarded.

Historical file references are not trusted merely because they were durable.
Only existing regular files that canonicalize inside the project worktree
remain actionable as diff paths, locations, or local resource links. A missing
or external local resource is displayed as non-actionable explanatory text.

## 6. Troubleshooting

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

## 7. Acceptance checks

After configuration:

1. open a real project folder in Zed and create a Zuno Agent thread;
2. select `deep`, send a read-only repository question, and confirm reasoning
   and tool updates stream incrementally;
3. request one file edit under an ask policy and confirm Zed displays both the
   permission request and typed diff;
4. cancel a running prompt and confirm the session returns to idle;
5. close and reload the session and confirm content, tools, plan, and usage are
   replayed once;
6. load the same session again and confirm the transcript is not duplicated.

Repository-level ACP verification is:

```sh
cargo test -p zuno-acp
cargo test -p zuno-cli --test acp_stdio
```
