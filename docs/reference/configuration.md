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
  "model": "myopenai/example-model"
}
```

Use an editor-specific schema association or an absolute file URI when the config file and repository schema are not in the same tree. A later documentation deployment can publish this unchanged artifact at a stable HTTPS URL.

## Main config versus TUI config

`theme`, `mouse`, key bindings, prompt dimensions, diff layout, and notification settings do **not** belong in `zuno.json`. They belong in `tui.json` or `tui.jsonc` at the corresponding global or project configuration layer.

The default preserves terminal-native selection:

```json
{
  "theme": "system",
  "mouse": false
}
```

With `mouse` absent or `false`, Zuno does not enable terminal mouse capture, so dragging can select text across the transcript, sidebar, and input area. Set `"mouse": true` only when TUI click and wheel handling is more important than native drag selection.

The `system` theme keeps the terminal's default foreground and background. It may use non-invasive environment color hints, but it does not query the terminal through stdin.

## Inspecting the result

Use `zuno debug paths` to inspect resolved roots and `zuno debug config` to inspect the merged configuration. A validation error names every rejected top-level key; for example, putting `theme` in `zuno.json` is rejected because it belongs in `tui.json`.
