# Configuration overview

Zuno reads `zuno.json` and `zuno.jsonc`. Everything the runtime can be told is a validated
field in that document, plus a small number of files that own their own concerns:
`tui.json` for interface settings, `AGENTS.md` for instructions, and package directories for
extensions and Skills.

The mental model that makes the rest predictable: **configuration declares maximums, and
narrower layers can only reduce them.** An agent contract narrows a sandbox mode. A project
layer narrows sandbox authority. A permission `allow` never restores a capability a
narrower layer removed.

## Where things live

| What | File | Page |
| --- | --- | --- |
| Runtime, providers, agents, permissions, sandbox | `zuno.json` / `zuno.jsonc` | [Files and precedence](/config/files) |
| Theme, keybindings, mouse, prompt size, diff layout, notifications | `tui.json` / `tui.jsonc` | [Themes and keybindings](/config/theming) |
| Instructions injected into every prompt | `AGENTS.md`, `AGENTS.local.md` | [Instructions and AGENTS.md](/config/instructions) |
| Reusable guidance with its own identity | `SKILL.md` in a Skill directory | [Authoring Skills](/config/authoring-skills) |
| Prompt templates and argument macros | `command/**/*.md` | [Workflows and commands](/config/workflows) |
| Agents as Markdown | `agent/*.md`, `agents/*.md` | [Custom agents](/config/custom-agents) |
| Extension packages | `extension.json` in a package directory | [Plugins](/plugins) |

Putting a TUI key such as `theme` in `zuno.json` is rejected rather than ignored, and the
validation error names the rejected key. The top level of `zuno.json` accepts no unknown
keys at all, which is deliberate: a typo that silently did nothing would be worse than a
refusal.

## Merge semantics

Objects merge recursively from lower to higher precedence. Arrays and scalars replace the
lower value.

That single rule explains most surprises. An array is not additive, so an agent's `tools`
list in a higher layer replaces the lower list rather than extending it, and the same is
true of `delegates`, `requiredSkills`, `writableRoots`, and `instructions`.

## The top-level shape

Forty keys exist. Grouped by what they decide:

| Group | Keys |
| --- | --- |
| Model routing | `model`, `small_model`, `preset`, `presets`, `provider`, `enabled_providers`, `disabled_providers` |
| Agents and delegation | `agents`, `default_agent`, `subagent_depth`, `workflows`, `productAgent` |
| Authority | `permission`, `sandbox`, `shell` |
| Instructions and Skills | `instructions`, `skills`, `command`, `memory` |
| Context | `compaction`, `tool_output`, `attachment`, `references` |
| Integrations | `mcp`, `lsp`, `formatter`, `web_search`, `watcher` |
| Runtime | `concurrency`, `goal`, `snapshot`, `tools`, `logLevel` |
| Deployment | `server`, `share`, `autoupdate`, `enterprise`, `experimental` |
| Presentation | `username`, `$schema` |

The key-by-key reference is [Configuration reference](/reference/configuration). This page
and its siblings explain the groups; that page is authoritative for a single field.

## A minimal working configuration

```json
{
  "$schema": "https://raw.githubusercontent.com/sunerpy/zuno/main/schemas/zuno.json",
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY"],
      "options": { "baseURL": "https://gateway.example.com/v1" },
      "models": {
        "primary-model": {
          "name": "Primary model",
          "reasoning": true,
          "tool_call": true,
          "limit": { "context": 200000, "output": 32000 }
        },
        "fast-model": {
          "name": "Fast model",
          "tool_call": true,
          "limit": { "context": 128000, "output": 16000 }
        }
      }
    }
  },
  "permission": {
    "mode": "standard",
    "rules": { "shell": "ask" }
  },
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny"
  }
}
```

There is no default model id. Zuno does not ship hidden provider defaults, so a
configuration without a reachable route produces a visible routing diagnostic rather than
silently choosing something.

## Schema validation while editing

The canonical schema is generated from the same Rust types that deserialize configuration,
so it cannot drift from what the binary accepts:

```json
{
  "$schema": "./schemas/zuno.json"
}
```

Use an editor schema association or an absolute file URI when the config file and the
schema are not in the same tree.

## Inspecting the result

Never infer the merged result. Print it:

```sh
zuno debug paths
zuno debug config
zuno debug permissions
zuno debug agent build
zuno debug sandbox --mode workspace-write --check
```

`debug paths` shows which roots this executable resolved, which is the first thing to check
when an edit appears to have no effect. `debug config` shows the merged document.
`debug permissions` reports both the configured and the effective mode, which differ when
an agent contract or `danger-full-access` is involved.

## Switching whole configurations

There is no `--profile` flag. Use `ZUNO_CONFIG_DIR` as a final higher-precedence overlay
directory:

```sh
zuno

ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

The overlay is a deep merge, so a provider defined there is added rather than replacing the
global catalog. Top-level `model`, `small_model`, and a non-null `preset` replace their
lower values. Do not use `"preset": null` as a tombstone: optional typed fields treat JSON
null as "no higher-layer value", so the inherited preset stays selected. Select an explicit
overlay preset instead.

## See also

- [Files and precedence](/config/files)
- [Configuration reference](/reference/configuration)
- [Variables and substitution](/config/variables)
- [Model routing](/config/models)
