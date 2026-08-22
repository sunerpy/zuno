# Configuration reference

## Files

Zuno reads only `zuno.json` and `zuno.jsonc`. The global files are under `$XDG_CONFIG_HOME/zuno` (normally `~/.config/zuno`); project layers are bare `zuno.json[c]` files from the worktree root to the current directory and files under `.zuno/`. `ZUNO_CONFIG` adds one explicit file, `ZUNO_CONFIG_DIR` adds a directory, and `ZUNO_CONFIG_CONTENT` supplies the final environment layer.

Objects merge recursively from lower to higher precedence. Arrays and scalar values replace the lower value. The top level rejects unknown keys.

## JSON Schema

The canonical schema is [`schemas/zuno.json`](../../schemas/zuno.json). It is generated from the same Rust types that deserialize configuration:

```sh
cargo run -p zuno-config --example generate-schema > schemas/zuno.json
cargo test -p zuno-config json_schema
```

For a repository-root configuration:

```json
{
  "$schema": "./schemas/zuno.json",
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model"
}
```

Use an editor-specific schema association or an absolute file URI when the config file and repository schema are not in the same tree. A later documentation deployment can publish this unchanged artifact at a stable HTTPS URL.

The complete checked starter is [`examples/config/zuno.json`](../../examples/config/zuno.json). It declares a native `openai` transport and contains no package-manager or AI SDK field. Follow the [provider initialization guide](providers.md#first-run-initialization) to install it, edit the endpoint and model ids, and store credentials.

## Main config versus TUI config

`theme`, `mouse`, key bindings, prompt dimensions, diff layout, and notification settings do **not** belong in `zuno.json`. They belong in `tui.json` or `tui.jsonc` at the corresponding global or project configuration layer.

The default enables application-owned mouse interaction:

```json
{
  "theme": "system",
  "mouse": true
}
```

With `mouse` absent or `true`, Zuno captures button, drag, release, and wheel events. The transcript provides its own selection and copy behavior, clamps a drag to the transcript instead of crossing into the sidebar, exposes clickable tool and sidebar disclosure rows, and mounts a draggable scrollbar when a conversation overflows.

Set `"mouse": false` to opt out of those interactions and return drag selection to the terminal. In that mode native selection may cross the transcript, sidebar, and input area; terminals that implement alternate-scroll mode can still translate wheel notches into transcript scrolling while the composer is empty.

The `system` theme queries the terminal's OSC 10/11 foreground and background colours before the TUI starts, then derives its text, panel, input, border, and syntax colours from that result. Terminals which do not support colour queries fall back to `COLORFGBG`; if neither source is available, Zuno applies a neutral root, panel, and input hierarchy so the interface does not collapse into one near-black surface.

## Product subagents

`productAgent` is a map of named, default-off Codex or Claude Code instances. Each enabled instance contributes one statically named tool:

```json
{
  "productAgent": {
    "codex": {
      "kind": "codex",
      "enabled": true,
      "command": "codex",
      "toolName": "subagent_codex",
      "permissionMode": "never"
    },
    "claude-code": {
      "kind": "claude-code",
      "enabled": true,
      "command": "claude",
      "toolName": "subagent_claude_code",
      "permissionMode": "dontAsk"
    }
  }
}
```

Instances inherit the Zuno process environment, working directory, and the product's native configuration and login. An optional `env` object overlays inherited variables, including proxy variables. Zuno does not copy Codex or Claude Code tokens into its provider credential store.

Dangerous modes `dangerouslyBypassApprovals` and `bypassPermissions` are accepted only when written explicitly. Tool names must be unique and cannot collide with native tools. See [Codex and Claude Code product agents](../design/product-agents.md) for protocol, job, cancellation, and TUI behavior.

## Concurrency

Independent runtime work has three bounded controls:

```json
{
  "concurrency": {
    "tool_calls": 8,
    "mcp_connections": 8,
    "lsp_requests": 4
  }
}
```

- `tool_calls` limits model-issued calls that explicitly declare themselves safe
  to overlap. Permission prompts and argument preparation remain ordered.
- `mcp_connections` limits simultaneous lifecycle operations across different
  MCP servers. One server's operations remain serialized.
- `lsp_requests` is the shared cap for language-server startup and request fan-out
  across servers.

Each field accepts `1..=64`; omission uses the values above. Set a field to `1`
to restore serial behavior for that layer.

## Inspecting the result

Use `zuno debug paths` to inspect resolved roots and `zuno debug config` to inspect the merged configuration. A validation error names every rejected top-level key; for example, putting `theme` in `zuno.json` is rejected because it belongs in `tui.json`.
