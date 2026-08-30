# Themes and keybindings

The terminal application reads its own configuration file, separate from Zuno's
main configuration. This page covers that file: where it lives, how its layers
merge, and every key it accepts.

## Why this is a separate file

`theme`, `keybinds` and the other keys below are absent from `zuno.json` on
purpose. They configure one client — the terminal application — rather than the
agent runtime, so a headless run, the ACP server and the HTTP server never read
them. Keeping them in `tui.json` means a keybinding cannot affect what a scripted
`zuno run` does.

The consequence worth remembering: if you put `theme` in `zuno.json` it is not an
error, and it also has no effect.

## File locations

The file is `tui.json` or `tui.jsonc`, discovered in this order. **Later paths
win.**

| Layer | Path |
| --- | --- |
| Global | `~/.config/zuno/tui.json` |
| Project, walking up from the working directory | `.zuno/tui.json` |

`ZUNO_TUI_CONFIG` overrides the discovered set.

A missing file is not an error; it is a layer that contributes nothing.

### Merging is per key, not per file

This is the part that surprises people. A later layer does not replace the earlier
one wholesale — it overrides only the keys it actually sets:

```json
// ~/.config/zuno/tui.json
{
  "theme": "system",
  "keybinds": { "session_new": "ctrl+n" }
}
```

```json
// .zuno/tui.json in one project
{
  "theme": "gruvbox"
}
```

The project file changes the theme and **leaves `session_new` bound**. Nested
objects merge the same way, so a file setting only `prompt.max_height` does not
erase `prompt.max_width`.

## Keys

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `$schema` | string | — | Schema reference. Accepted and unused. |
| `theme` | string | built-in default | Theme name, or `system` to derive a palette from the terminal's own colours. |
| `keybinds` | object | — | Per-action overrides, keyed by action name. |
| `leader_timeout` | number | — | Leader-key timeout in milliseconds. |
| `prompt` | object | — | Prompt size settings, including `max_height` and `max_width`. |
| `scroll_speed` | number | — | Lines scrolled per wheel notch. |
| `scroll_acceleration` | object | — | Scroll acceleration settings. |
| `diff_style` | string | — | Diff rendering style. |
| `mouse` | boolean | `false` | Application mouse handling. |
| `attention` | object | all off | Notification and sound-cue settings. |

### `theme`

A name any theme layer provides, plus the special value `system`.

```json
{
  "theme": "system"
}
```

A name no layer provides is **not** an error. The theme registry falls back to the
default and reports a diagnostic naming the theme it could not find, so a typo
surfaces as a message rather than a failure to start.

### `mouse`

Disabled by default, and the reason is worth stating: with application mouse
handling off, the terminal keeps its own drag-selection and copy behaviour across
the transcript, sidebar, prompt, dialogs and notices. Turning it on trades that for
click-to-toggle sections and wheel scrolling.

```json
{
  "mouse": true
}
```

### `keybinds`

Keyed by action name. A value is either a single binding or an object carrying
several spellings.

```json
{
  "keybinds": {
    "session_new": "ctrl+n",
    "input_paste": { "key": "ctrl+v", "alt": "cmd+v" }
  },
  "leader_timeout": 800
}
```

An unrecognized **key name** inside a binding is accepted and ignored. An
unrecognized **action name** is reported, because a keybinding you believe you set
and which silently does nothing is worse than a diagnostic.

### `attention`

Notifications and sound cues, with a master default of **off**. Nothing here makes
noise until you ask for it.

## Inspecting the result

```sh
zuno debug config
```

This prints the resolved configuration, so a layer that did not merge the way you
expected is visible rather than inferred.

## See also

- [The terminal application](/guide/tui) — the interface these keys configure
- [Configuration files and precedence](/config/files) — the main configuration stack
- [zuno tui](/cli/tui) — command-line options
- [zuno debug](/cli/debug) — `debug config` and the other introspection subcommands
