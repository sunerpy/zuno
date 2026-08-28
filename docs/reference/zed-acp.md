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
Zuno publishes the executable commands from its normal command catalog plus
unambiguous slash-invokable Skills. Zed then exposes them in `/` completion.

The sources are the same as other Zuno surfaces:

- global `command/*.md` or `commands/*.md` under the Zuno config directory;
- project `.zuno/command/*.md` or `.zuno/commands/*.md`;
- built-in commands that have real handlers;
- discovered Skills whose names do not conflict with commands.

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
- Zuno's Shell sandbox controls filesystem and network authority;
- native file tools emit typed creation and edit diffs for Zed;
- Zuno-configured MCP servers remain available when the selected Agent profile
  permits them;
- cancellation, session load, resume, close, plan state, usage, and tool
  history use the same durable runtime as the TUI.

ACP-provided client MCP, client filesystem RPC, and terminal RPC are not
advertised. Zuno handles file and Shell work through its own tools, permission
policy, and sandbox rather than claiming Zed client RPC handlers.

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

### Agent or model selector is absent

Confirm Zed connected successfully, then create a new external-Agent thread.
Run `zuno acp --check` to verify the production adapter, and inspect the ACP
logs for initialization or session-creation errors.

### A Kiro prompt fails with `unsupported_content_block_projection`

If a file reference plus user text produces an HTTP 400 saying that one message
contains multiple text content blocks, declare the gateway limitation in the
Kiro-compatible provider:

```json
"options": {
  "baseURL": "http://127.0.0.1:8787/v1",
  "maxTokens": null,
  "responsesTextBlocks": "single"
}
```

This joins only provider-bound text projections. Zuno still stores the original
typed resource link and user text separately. Do not enable it for a standard
Responses endpoint that accepts multiple `input_text` blocks.

## 9. Acceptance checks

After configuration:

1. open a real project folder in Zed and create a Zuno Agent thread;
2. select `deep`, the intended model, and `xhigh` or `max`, then confirm the
   choice is shown in the session controls;
3. type `/` and execute one configured command or unambiguous Skill;
4. attach an image, selection, and branch diff and confirm they reach the turn;
5. send a read-only repository question and confirm reasoning
   and tool updates stream incrementally;
6. request one file edit under an ask policy and confirm Zed displays both the
   permission request and typed diff;
7. cancel a running prompt and confirm the session returns to idle;
8. close and reload the session and confirm content, tools, plan, and usage are
   replayed once.

Repository-level ACP verification is:

```sh
cargo test -p zuno-acp
cargo test -p zuno-cli --test acp_stdio
```
