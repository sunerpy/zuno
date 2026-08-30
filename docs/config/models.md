# Model routing

Zuno ships no default model id. A configuration without a reachable route produces a
visible routing diagnostic rather than silently choosing something, which is the only
honest behavior when the cost and capability difference between models is this large.

Routing has three layers: a session default, a cheap side-task model, and optional
named team presets that route individual agents and workflow categories.

## The two top-level model keys

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `model` | `string` \| `null` | none | Default model, in `provider/model` form |
| `small_model` | `string` \| `null` | none | Model for cheap side tasks such as title generation |

```json
{
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model"
}
```

Both are qualified `provider/model` ids. The provider segment is the key in the
`provider` map, and the model segment is the key in that provider's `models` map — not
the vendor's marketing name. This is why one gateway can expose several upstream
vendors under one provider id.

`small_model` exists because title generation, summaries, and similar side tasks run
often and do not benefit from a frontier model. Leaving it unset does not disable
those tasks; it means they use the same route as everything else.

Both keys are scalars, so a higher-precedence configuration layer replaces the lower
value outright.

## Presets: routing a whole team

A preset is a typed team-wide model route. It selects a model and an optional
provider-neutral reasoning level for an agent or a semantic workflow category. It
does not create agents, grant tools, change permissions, or authorize delegation.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `preset` | `string` \| `null` | none | Active team model-routing preset |
| `presets` | map of preset \| `null` | none | Named team model-routing presets |

Each preset body has exactly two optional fields:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `agents` | map of model choice | `{}` | Per-agent model choices |
| `categories` | map of model choice | `{}` | Semantic category model choices |

Agent routes drive direct and delegated agent selection. Categories are semantic
shorthands for workflow nodes that should not hard-code an agent name.

A model choice is either a bare `provider/model` string or an object:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `model` | `string` | required | Model in `provider/model` form |
| `reasoning` | `off` \| `low` \| `medium` \| `high` \| `xhigh` \| `max` \| `null` | none | Provider-neutral reasoning level for this route |

```json
{
  "preset": "house",
  "presets": {
    "house": {
      "agents": {
        "orchestrator": { "model": "myopenai/primary-model", "reasoning": "max" },
        "deep": { "model": "myopenai/primary-model", "reasoning": "high" },
        "explorer": "myopenai/fast-model"
      },
      "categories": {
        "cheap": "myopenai/fast-model"
      }
    }
  }
}
```

Every preset body must use the explicit `agents` and `categories` objects. Flat
compatibility forms and provider-specific `variant` fields inside a preset are
rejected rather than ignored. A bare string leaves reasoning unchanged.

## Which route wins

For direct `task` delegation naming an agent, precedence is:

1. `agents.<target>.model`;
2. the active preset's agent route;
3. the parent session model.

For host-owned workflow and Council category routes:

1. the active preset's category route;
2. the parent session model.

The `general` agent route is deliberately not consulted for category routing, so a
broad agent route cannot quietly capture workflow nodes. The model-facing delegation
tool does not accept `model`, `effort`, or `category` overrides; routing is a
configuration decision, not a per-call one.

Reasoning comes from the winning route, then the model or provider default. The
selected preset is frozen with the turn plan, so editing configuration cannot mutate
an in-flight attempt.

## Reasoning levels and variants

The six canonical reasoning levels are `off`, `low`, `medium`, `high`, `xhigh`, and
`max`. They are provider-neutral names that Zuno maps onto whatever the selected
model actually exposes, and they apply only with that agent's configured model.

Variants are the model's own named option sets, declared in the provider catalog:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `provider.<id>.models.<id>.variants` | map of variant \| `null` | none | Named variants |
| `provider.<id>.models.<id>.variants.<name>.disabled` | `boolean` \| `null` | none | Disable this variant for the model |

For one headless invocation, `zuno run --variant <name>` overrides configured
reasoning with the exact model-declared variant. Canonical names are accepted only
when the selected model declares them, or when the model exposes generic reasoning
without a named variant catalog. A non-canonical name copies that variant's complete
provider option object, so a model declaring only `deliberate` does not silently
acquire canonical levels. Unknown names fail before HTTP I/O and list what is
available.

`zuno run --thinking` asks the host to select `high` when available, otherwise the
strongest declared non-`off` canonical level. It fails for a non-reasoning model and
for a named-only custom variant catalog whose semantics cannot be inferred.
`--thinking` and `--variant` are mutually exclusive. Prefer `--variant max` or
`--variant xhigh` when exact effort matters.

## Per-model catalog fields

A model entry describes capability so the runtime can refuse an impossible request
before making it. The commonly set fields:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | `string` \| `null` | none | Display name |
| `id` | `string` \| `null` | the map key | Model id override |
| `reasoning` | `boolean` \| `null` | none | Whether the model reasons |
| `tool_call` | `boolean` \| `null` | none | Whether the model can call tools |
| `attachment` | `boolean` \| `null` | none | Whether the model accepts attachments |
| `temperature` | `boolean` \| `null` | none | Whether the model honours `temperature` |
| `limit` | object \| `null` | none | Token limits |
| `cost` | object \| `null` | none | Pricing |
| `modalities` | object \| `null` | none | Input and output modalities |
| `family` | `string` \| `null` | none | Model family |
| `status` | enum \| `null` | none | Lifecycle status |
| `experimental` | `boolean` \| `null` | none | Whether the model is experimental |
| `release_date` | `string` \| `null` | none | Release date |
| `interleaved` | `boolean` \| `string` \| object \| `null` | none | Interleaved-reasoning configuration |
| `headers` | object of `string` \| `null` | none | Extra headers for requests to this model |
| `options` | object \| `null` | none | Model options handed to the provider SDK |
| `provider` | object \| `null` | none | The native transport and API endpoint backing this model |
| `variants` | map \| `null` | none | Named variants |

`limit.context` is what several other budgets derive from — the Skill catalog budget
and the selected-body budget are both percentages of a known context. Omitting it
means those budgets fall back to fixed approximate values instead.

## Switching teams at runtime

In the TUI, `/preset` opens the configured picker and `/preset <name>` selects one
directly. The replacement is prepared and applied inside the current TUI; it does not
restart the interface or interrupt an in-flight turn.

A preset switch clears prior manual model and reasoning overrides so the selected
team's routes take effect. A later explicit model or reasoning choice overrides that
team route for the top-level agent while the preset continues to route delegations.
That choice is session-local runtime state — set the top-level `preset` key to make a
team the startup default.

Do not use `"preset": null` as a tombstone in an overlay. Optional typed fields treat
JSON null as "no higher-layer value", so the inherited preset stays selected. Name an
explicit overlay preset instead.

## Verifying a route

```sh
zuno models myopenai --verbose
zuno debug config
zuno debug agent build
```

`zuno models` lists what the catalog actually resolved, including whether a model
declares reasoning and variants. `debug agent` shows the effective route for one
agent, which is where an unexpected preset interaction becomes visible.

## See also

- [Authentication and credentials](/config/authentication)
- [Providers and credentials](/reference/providers)
- [Custom agents](/config/custom-agents)
- [Agent orchestration](/orchestration)
