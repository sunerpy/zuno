# Custom agents

An agent is a contract, not a persona. It fixes which model runs, which tools are
visible, which children may be delegated to, and what authority the turn has. The
reason to define one is to make a narrower capability set reproducible instead of
relying on prompt wording.

The rule that governs everything below: an agent contract can only narrow. A
configured `allow`, including `permission.mode: "allow_all"`, cannot restore a
capability the parent attempt never had. See [Permissions and sandboxing](/guide/permissions)
for the full authority model.

## Two definition surfaces

Agents live in the `agents` map, keyed by agent name:

```json
{
  "agents": {
    "reviewer": {
      "description": "Reviews a diff and reports findings without editing",
      "mode": "subagent",
      "tools": ["read", "grep", "glob"],
      "permission": { "rules": { "shell": "deny" } }
    }
  }
}
```

Or as Markdown with frontmatter, discovered under `{agent,agents}/**/*.md` in the
global config directory and every project `.zuno` directory. The frontmatter accepts
the same fields as one `agents` map entry; the body is the system prompt.

```markdown
---
description: Reviews a diff and reports findings without editing
mode: subagent
tools: [read, grep, glob]
---

Report findings as a list. Do not edit files.
```

Prefer Markdown when the prompt is long enough that JSON string escaping hurts, and
JSON when the definition is mostly capability fields.

## Every field

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `color` | theme colour \| `#rrggbb` \| `null` | none | Display colour |
| `delegates` | `string[]` \| `null` | none | Exact child-agent allowlist for direct delegation and workflows |
| `description` | `string` \| `null` | none | When to use the agent |
| `disable` | `boolean` \| `null` | none | Remove this agent |
| `hidden` | `boolean` \| `null` | none | Hide the agent from the `@` autocomplete menu |
| `mode` | `subagent` \| `primary` \| `all` \| `null` | none | Where the agent may be used |
| `model` | `string` \| `null` | none | Model in `provider/model` form |
| `options` | object \| `null` | none | Provider options, including every swept unknown key |
| `permission` | object \| `null` | none | Per-tool permissions for this agent |
| `prompt` | `string` \| `null` | none | System prompt |
| `reasoning` | `off` \| `low` \| `medium` \| `high` \| `xhigh` \| `max` \| `null` | none | Provider-neutral reasoning level, applied only with the configured model |
| `requiredSkills` | `string[]` \| `null` | none | Skills loaded at the start of every turn for this agent |
| `steps` | positive integer \| `null` | unbounded | Maximum tool-capable iterations before one text-only finalization request |
| `temperature` | number \| `null` | none | Sampling temperature |
| `tools` | `string[]` \| `null` | none | Exact model-visible tool allowlist for this agent |
| `top_p` | number \| `null` | none | Nucleus-sampling cutoff |
| `variant` | `string` \| `null` | none | Default model variant, applied only with the agent's configured model |

The named theme colours are `primary`, `secondary`, `accent`, `success`, `warning`,
`error`, and `info`. A hex code is validated on the way in.

Unknown keys are not rejected here. The `agents` map sweeps any key it does not name
into `options` and keeps it verbatim, which is how provider-specific settings reach
the SDK without the schema having to enumerate them. This is the one place in the
configuration where a typo does not produce an error, so check `zuno debug agent`
after adding an unusual key.

## Fields with non-obvious behavior

`tools` is an exact allowlist, not an addition. Setting it replaces whatever the role
would otherwise expose, and because arrays replace across configuration layers, a
project layer setting `tools` discards the global list entirely. The same applies to
`delegates` and `requiredSkills`.

`mode` decides reachability. `subagent` means the agent is only available as a
delegation target and never as a top-level session agent; `primary` is the reverse;
`all` is both. A `default_agent` must be a primary agent.

`steps` counts tool-capable provider iterations within one turn. When the limit is
exhausted and the model still requests continuation, Zuno sends one additional
text-only request with no tools, asking for a concise account of completed work,
remaining work, evidence, and blockers. It does not silently raise or reset the limit.
Omit the field for the default unbounded behavior.

`requiredSkills` guarantees instructions, not authority. Each name must resolve to
exactly one source after profile and agent visibility filtering; a missing or
ambiguous name fails child startup rather than silently skipping the requirement. The
field does not copy a Skill into configuration and does not grant tools — so
`"requiredSkills": ["codegraph"]` guarantees CodeGraph instructions, not CodeGraph
execution authority.

`reasoning` and `variant` apply only with the agent's configured `model`. Setting
either without a model is not an error, but it has no effect on a route selected
elsewhere.

`disable` removes a built-in agent from the roster. That is the intended way to drop
a role you do not want available, rather than trying to strip its tools one by one.

`hidden` only affects the `@` autocomplete menu. A hidden agent is still delegable
and still reachable by name.

## The capability ceiling

For a delegated turn, the parent attempt's actual provider-visible tool schemas form
an immutable upper bound. The target role, its MCP and extension inheritance policy,
the agent's exact `tools` allowlist, and effective permission rules can only narrow
that set. A same-named tool with a different provider-visible schema is also outside
the bound.

MCP and extension tools are therefore not automatically available to every read-only
agent. All of these must hold: the exact schema was visible in the parent attempt, the
target role either inherits extension tools automatically or carries an exact
per-agent `permission.rules` grant, the allowlist retains the wire id, and no later
explicit rule denies it. Unknown MCP tools remain side-effecting by default, so
granting one audited query tool does not opt the agent into every MCP tool.

## Per-agent permissions

The `permission` object mirrors the global one:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `mode` | `standard` \| `strict` \| `allow_all` | `standard` | How unresolved and side-effecting calls are admitted |
| `rules` | object | `{}` | Ordered per-tool rules. Explicit denies remain terminal in every mode |

Explicit denies remain terminal in every mode, including `allow_all`. That asymmetry
is the whole point: `allow_all` removes prompts, not restrictions.

## Model routing for agents

An agent's `model` is the most specific route. When it is absent, the active preset's
agent route applies, then the parent session model. Presets are the better tool for
routing a whole team — see [Model routing](/config/models) — because they keep model
choices in one place instead of scattered across agent definitions.

## Inspecting a definition

Zuno does not generate agent definitions. Author one by editing `zuno.json` or by
writing a Markdown file under `.zuno/agent/` with the frontmatter shown in
[Two definition surfaces](#two-definition-surfaces), then read back what Zuno
resolved:

```sh
zuno agent list
zuno debug agent reviewer
```

`zuno debug agent <name>` prints the effective resolved contract: model route, tool
visibility, permission ruleset, and the agent-filtered Skill view with metadata and
selected-body budgets. Read it after any change to `tools`, `permission`, or
`requiredSkills`, because the resolved set is the product of several layers and is not
reliably predictable by reading configuration alone.

## Related top-level keys

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `default_agent` | `string` \| `null` | none | Agent to use when none is named. Must be a primary agent |
| `subagent_depth` | integer \| `null` | `1` | Maximum subagent nesting depth |

## See also

- [Agents](/guide/agents)
- [Permissions and sandboxing](/guide/permissions)
- [Model routing](/config/models)
- [Agent orchestration](/orchestration)
