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
| Model-visible session History and Notes | `zuno.json` / `zuno.jsonc` | [History and Notes continuity](/config/continuity) |
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

Forty-one keys exist. Grouped by what they decide:

| Group | Keys |
| --- | --- |
| Model routing | `model`, `small_model`, `preset`, `presets`, `provider`, `enabled_providers`, `disabled_providers` |
| Agents and delegation | `agents`, `default_agent`, `subagent_depth`, `subagent_model_selection`, `workflows`, `productAgent` |
| Authority | `permission`, `sandbox`, `shell`, `trust` |
| Instructions, Skills, and learning | `instructions`, `skills`, `command`, `memory`, `learning` |
| Context | `compaction`, `continuity`, `tool_output`, `attachment`, `references` |
| Integrations | `mcp`, `lsp`, `formatter`, `web_search`, `watcher` |
| Runtime | `concurrency`, `goal`, `runtime`, `snapshot`, `tools`, `navigation`, `logLevel` |
| Deployment | `server` |
| Editor support | `$schema` |

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
    "network": "deny",
    "onUnavailable": "deny"
  }
}
```

There is no default model id. Zuno does not ship hidden provider defaults, so a
configuration without a reachable route produces a visible routing diagnostic rather than
silently choosing something.

## Choosing no-sandbox behavior

The default remains fail-closed: `workspace-write` plus `onUnavailable: "deny"` requires
the requested confinement backend to be deployable.

To always use the native host process backend, select the explicit mode:

```json
{
  "sandbox": {
    "mode": "danger-full-access"
  }
}
```

To prefer confinement but allow a write-capable `workspace-write` Agent to continue only
when the backend has an eligible typed availability failure:

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "run-unconfined"
  }
}
```

`run-unconfined` is accepted only from a trusted global, explicit, environment, CLI, or
managed layer. Project configuration cannot enable it, read-only Agents never use it, and
managed policy may force it back to `deny`. See
[Permissions and sandboxing](/guide/permissions) for the exact fallback boundary.

To run every Agent's Shell natively, read-only Agents included, while keeping the
configured permission mode, select the backend explicitly instead of a mode:

```json
{
  "sandbox": {
    "backend": "native"
  }
}
```

`native` is a host declaration for a machine without an OS sandbox, not a fallback and
not confinement: the requested authority is recorded but not OS-enforced, and a read-only
Agent's contract becomes a tool, permission and risk-gate boundary. It is accepted only
from the same trusted layers, a project layer may only say `auto`, and managed policy may
force `auto`. The one-invocation spellings are `zuno --sandbox-backend native` and
`ZUNO_SANDBOX_BACKEND=native`.

A project layer that declares a host command is refused. `shell`, a local `mcp.*.command`,
an `lsp.*.command` or `formatter.*.command` that is not disabled, and a
`productAgent.*.command` in a project `zuno.json[c]` or `.zuno` file all fail validation;
remote MCP servers are unaffected. Only `trust.project_host_commands` in a trusted layer
admits a checkout, and a project layer that sets `trust` itself is refused too. See
[Files and precedence](/config/files).

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
- [History and Notes continuity](/config/continuity)
- [Configuration reference](/reference/configuration)
- [Variables and substitution](/config/variables)
- [Model routing](/config/models)
