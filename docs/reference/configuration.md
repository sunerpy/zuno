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

## Context compaction

Zuno can compact older conversation history before the model window is exhausted:

```json
{
  "compaction": {
    "auto": true,
    "threshold_percent": 80,
    "tail_turns": 2,
    "reserved": 12000
  }
}
```

- `threshold_percent` accepts `1..=100` and defaults to `80`. It is applied to
  the usable context window after the model's output allowance and configured
  reserve are removed.
- `auto: false` disables proactive threshold compaction. Manual `/compact`
  remains available.
- A provider-confirmed context-limit failure still uses the bounded compaction
  recovery path before retrying; this is recovery from an already failed
  request, not the proactive threshold.
- `/compact` persists the summary through the same durable compaction pipeline,
  so subsequent turns and resumed clients see the same retained history.

## Plugin packages

Plugins are package directories, not `zuno.json` fields. Install them globally
or below the selected project's `.zuno/extensions` directory:

```sh
zuno plugin add /path/to/package --project
zuno plugin list
zuno plugin update /path/to/package --project
zuno plugin remove package-id --project
```

Custom agents and workflows use native tool permissions for file, network, and
environment access. Runtime tools use an explicitly granted WASI component or a
contained `host.full` process. See [plugins, custom agents, and
workflows](../plugins.md).

## Strict HITL authorization

Strict authorization is off by default. Enable it when every side-effecting tool
invocation must receive a fresh decision from an attached user:

```json
{
  "authorization": {
    "strict": true
  }
}
```

Strict mode is an additional gate above ordinary `permission` rules:

- An explicit `deny` remains terminal.
- `allow`, a plugin allow, an earlier "Allow always", and TUI `--auto` cannot
  authorize a side effect.
- The prompt offers only "Allow once" and "Reject"; approval is never remembered
  for a later call.
- A headless run fails closed because it has no user to ask.

Native file reads, `glob`, `grep`, skill/session/job/goal inspection, read-only
LSP operations, MCP resource reads, `webfetch`, and `web_search` do not receive
the extra strict prompt. `bg list/output/wait` are read-only while `bg cancel`
requires approval. Shell, file writes, durable state changes, delegation,
product agents, extension lifecycle mutations, and unknown harness or MCP tools
are side-effecting by default.

`bash` always requires strict approval, even for a command such as `rg`, because
the shell is not an operating-system sandbox. Use the native `grep` or `glob`
tool when the intended operation is read-only.

Independently of strict mode, the shell risk gate requires fresh approval before
bounded destructive operations or replacing an existing redirect target. New
static files under the working directory or OS temporary directory are treated
as creation. There is no tool argument that lets a model approve its own risky
call, and an explicit permission deny always wins.

## Memory learning

Resident memory is enabled by default, but model and reflection writes enter an
auditable candidate queue first:

```json
{
  "memory": {
    "resident": true,
    "tool": true,
    "reflection": true,
    "global_char_limit": 2200,
    "project_char_limit": 3000,
    "nudge_interval": 10,
    "promotion": "review",
    "auto_confidence": 0.9
  }
}
```

- `resident` injects the frozen global and project memory blocks into prompts.
- `tool` exposes `memory_propose`; it never grants direct file mutation.
- `reflection` reviews completed delivered turns with `small_model`. Reflection
  is disabled with a visible diagnostic when no explicit reachable small model
  is configured; Zuno does not silently spend the session model.
- `nudge_interval` triggers periodic review every N durably recorded delivered
  assistant messages. The count survives host rebuilds and process restarts. A
  verified recovery can trigger review earlier; zero disables only the periodic
  trigger.
- `promotion` is `review` (default), `high_confidence`, or `automatic`.
  `high_confidence` applies only candidates at or above `auto_confidence`.
- `auto_confidence` is a finite value in `0..=1` and defaults to `0.9`.

Each reflection request contains the exact resident-memory snapshot so the
reviewer can propose audited `replace` or `remove` operations instead of adding
duplicates. An expired running reflection job becomes `uncertain` and is never
automatically replayed.

`memory: false` disables resident injection, proposal tools, and reflection.
`/memory` reviews, edits, approves, rejects, removes, and undoes durable changes.
See [auditable memory and reflection](../design/memory-learning.md).

## Inspecting the result

Use `zuno debug paths` to inspect resolved roots and `zuno debug config` to inspect the merged configuration. A validation error names every rejected top-level key; for example, putting `theme` in `zuno.json` is rejected because it belongs in `tui.json`.
