# Files and precedence

Zuno reads two filenames for its main configuration: `zuno.json` and `zuno.jsonc`. Nothing
else. It also reads `tui.json` and `tui.jsonc` at the corresponding layers for interface
settings, and `AGENTS.md` for instructions.

Before reasoning about precedence, confirm which roots this executable actually resolved:

```sh
zuno debug paths
```

## Resolved roots

```text
home       /config
data       /config/.local/share/zuno
bin        /config/.cache/zuno/bin
log        /config/.local/share/zuno/log
repos      /config/.local/share/zuno/repos
cache      /config/.cache/zuno
config     /config/.config/zuno
state      /config/.local/state/zuno
tmp        /tmp/zuno
```

| Root | What it holds |
| --- | --- |
| `config` | `zuno.json`, `tui.json`, `AGENTS.md`, agents, skills, commands, extensions, profiles, themes |
| `data` | Session databases, the credential store, logs, repos |
| `log` | The structured operational log store |
| `cache` | Model catalog caches and downloaded binaries |
| `state` | Runtime state that is neither configuration nor durable session data |
| `tmp` | Temporary working files |

The paths above come from one real host, so yours will differ. They follow the XDG
variables: `config` is `$XDG_CONFIG_HOME/zuno`, `data` is `$XDG_DATA_HOME/zuno`, and `cache`
is `$XDG_CACHE_HOME/zuno`.

## Layer order

Lowest precedence first:

| Order | Layer | Source |
| --- | --- | --- |
| 1 | Global | `$XDG_CONFIG_HOME/zuno/zuno.json[c]` |
| 2 | Project walk | bare `zuno.json[c]` from the worktree root down to the current directory |
| 3 | Project `.zuno` | files under `.zuno/` over the same walk |
| 4 | `ZUNO_CONFIG` | one explicit file |
| 5 | `ZUNO_CONFIG_DIR` | one explicit directory containing `zuno.json[c]` |
| 6 | `ZUNO_CONFIG_CONTENT` | the final environment layer, supplied inline |

Nearer project directories are later, so they win over the worktree root.

Objects merge recursively from lower to higher precedence. Arrays and scalar values replace
the lower value. The top level rejects unknown keys.

## Environment overrides

| Variable | Effect |
| --- | --- |
| `ZUNO_CONFIG` | Adds one explicit configuration file as a high-precedence layer |
| `ZUNO_CONFIG_DIR` | Adds one directory containing `zuno.json[c]` as a switchable overlay |
| `ZUNO_CONFIG_CONTENT` | Supplies the final layer inline, for managed or ephemeral environments |

```sh
ZUNO_CONFIG="$HOME/audit/zuno.json" zuno debug config
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

Since there is no named `--profile` flag, `ZUNO_CONFIG_DIR` is how a whole team or provider
selection is switched. The overlay deep-merges, so it need not repeat provider definitions;
it can contain only the top-level selections:

```json
{
  "model": "kiro-local/claude-opus-5",
  "small_model": "kiro-local/gpt-5.6-luna",
  "preset": "kiro-local"
}
```

## First-run defaults

On the first ordinary discovery, Zuno creates a missing global `zuno.json` as `{}` and a
missing global `AGENTS.md` from its own starter guidance. Creation uses exclusive new-file
semantics and never overwrites either file.

An explicit `ZUNO_CONFIG`, `ZUNO_CONFIG_DIR`, or `ZUNO_CONFIG_CONTENT` launch does not
materialize defaults. So an installation, or one ordinary launch, should precede a
profile-only first run.

## Main config versus TUI config

`theme`, `mouse`, keybindings, prompt dimensions, diff layout, and notification settings do
not belong in `zuno.json`. They belong in `tui.json` or `tui.jsonc` at the corresponding
layer.

```json
{
  "theme": "system",
  "mouse": true,
  "leader_timeout": 5000
}
```

A validation error names every rejected top-level key, so putting `theme` in `zuno.json`
produces a clear refusal rather than a setting that mysteriously does nothing. See
[Themes and keybindings](/config/theming).

## Sandbox authority follows provenance

Not every layer may select every sandbox mode, and this is the one place where precedence is
not the whole story.

| Layer | Sandbox authority |
| --- | --- |
| Trusted global, explicit config, managed, environment, CLI | May select any mode |
| Project `zuno.json[c]` and `.zuno` | May only narrow to `read-only`, deny networking, add protected paths, or set `onUnavailable` to `deny` |

A project layer cannot select a wider mode, grant host networking, or add external writable
roots. It also cannot enable `run-unconfined`. A checked-in repository configuration
therefore cannot escalate its own confinement, which is the property that makes cloning an
unfamiliar repository safe.

Use a trusted one-invocation override when a wider mode is genuinely wanted:

```sh
zuno --sandbox read-only
zuno --sandbox danger-full-access
zuno --sandbox workspace-write --sandbox-on-unavailable run-unconfined
```

The environment equivalent for unavailable-only fallback is
`ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`. Managed policy has later precedence and may
still narrow either override or force the unavailable action back to `deny`. See
[Permissions and sandboxing](/guide/permissions).

## Where other assets are discovered

| Asset | Locations |
| --- | --- |
| Agents | `agent/*.md` and `agents/*.md` under the global config root and each project `.zuno` |
| Commands | `command/**/*.md` and `commands/**/*.md`, recursively, at the same roots |
| Skills | Project `.zuno/skill(s)`, project `.agents/skills`, global config roots, `~/.agents/skills`, `skills.paths`, remote indexes |
| Extensions | `.zuno/extensions` for a project; `extensions` under the global config root |

Zuno never scans `.opencode` or an OpenCode configuration directory for any of these.

## Which database this build opens

The session database filename depends on the build channel, which is the most common cause
of an apparently empty session list:

| Condition | File |
| --- | --- |
| `ZUNO_DB` is `:memory:` | in memory |
| `ZUNO_DB` is an absolute path | that path, verbatim |
| `ZUNO_DB` is relative | joined onto the data directory, not the working directory |
| Channel is `latest`, `beta`, or `prod`, or `ZUNO_DISABLE_CHANNEL_DB` is exactly `1` or `true` | `zuno.db` |
| Otherwise | `zuno-<channel>.db` |

A source build has no channel define, so it resolves `zuno-local.db`. See
[Database lifecycle](/migration).

## See also

- [Configuration overview](/config/)
- [History and Notes continuity](/config/continuity)
- [Variables and substitution](/config/variables)
- [Instructions and AGENTS.md](/config/instructions)
- [Configuration reference](/reference/configuration)
