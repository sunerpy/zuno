# Agent orchestration and model routing

This guide explains how Zuno's default `orchestrator` delegates work, how to
choose child-agent models and reasoning levels, and when to use direct tasks,
categories, configured workflows, or Council.

The short answer is:

- configure agent identity, tools, permissions, and delegation boundaries under
  `agents`;
- configure switchable team-wide model routes under `presets`;
- use `task.model` and `task.effort` only for a deliberate one-child override;
- use `workflows` when the graph and dependency order must be configuration
  owned rather than invented by the model.

## What the orchestrator owns

Agent selection resolves in this order:

1. an agent explicitly selected by the client;
2. top-level `default_agent`;
3. built-in `orchestrator`.

`orchestrator` is the default multi-agent delivery owner and the only native
primary agent that exposes the `task` delegation tool. Its built-in direct
targets are:

- `deep`;
- `fixer`;
- `general`;
- `explorer`;
- `librarian`;
- `oracle`;
- `looker` when a vision-capable route is available.

Configured or extension agents whose mode is `subagent` or `all` can join that
target roster. A `primary`-only agent cannot be delegated to.

The built-in workflow is:

```text
durable user input
  -> selected primary Agent and frozen capabilities
  -> primary model request
  -> task, workflow, or council tool call
  -> permission, target, depth, and model-route validation
  -> durable child session or background job
  -> child model/tool loop
  -> durable result/report admitted back to the parent
```

Delegation is not a prompt-only convention. The target roster, model route,
permissions, recursion bound, cancellation, job state, and report delivery are
typed runtime decisions.

## Built-in agent responsibilities

| agent | responsibility | delegation |
| --- | --- | --- |
| `orchestrator` | Own the outcome, partition work, integrate results, and verify completion | May delegate |
| `build` | Direct end-to-end implementation in one lane | No child tools |
| `plan` | Read-only research and implementation-ready planning | No child tools |
| `deep` | Difficult cross-cutting implementation | No recursive delegation |
| `fixer` | Focused local change and regression scope | No recursive delegation |
| `general` | Bounded work without a narrower specialist | No recursive delegation |
| `explorer` | Read-only repository and call-chain research | No recursive delegation |
| `librarian` | Current external documentation and upstream research | No recursive delegation |
| `oracle` | Read-only architecture and root-cause review | No recursive delegation |
| `looker` | Visual artifact inspection | No recursive delegation |

`explorer` is deliberately native-read-only, not shell-read-only. Its default tool
surface contains `read`, `glob`, `grep`, and read-only `lsp`, and denies `shell`, edits,
delegation, and network research. Commands such as `du`, `stat`, and `file` therefore do
not belong to `explorer`: they are executables reached through `shell`, not native read
operations. Delegate command-based inspection to `deep`, `general`, or another
shell-capable Agent, or execute the bounded command in the parent session. Global
`permission.mode: "allow_all"` skips ordinary confirmation but does not erase this
explicit Agent deny. A user can replace the Agent permission overlay explicitly, but
doing so intentionally changes `explorer`'s read-only contract and is normally less
clear than selecting the correct Agent.

That deny remains until the active platform provides the complete OS sandbox gate
described in [the Shell sandbox roadmap](design/shell-sandbox-roadmap.md). Zuno does
not treat command parsing, process groups, or a prompt that says "read-only" as
confinement.

### Optional configured designer

UI method is provided by the first-party `ui-design` Skill rather than a new native
Agent. When a project benefits from a separate model/context lane, add
`.zuno/agent/designer.md`:

```markdown
---
description: Review and implement bounded UI and interaction work
mode: subagent
permission:
  mode: standard
  rules:
    "*": deny
    read: allow
    glob: allow
    grep: allow
    lsp: allow
    edit: allow
    shell: ask
    skill: allow
    plan_get: allow
    todo_get: allow
---

Own only the delegated UI/UX implementation scope. Load the `ui-design` Skill before
acting. Respect the existing design system, do not perform broad external research,
do not make product or backend architecture decisions, and do not delegate children.
Return changed files, interaction/accessibility checks, visual evidence, and risks.
```

Add the configured Agent to the orchestrator's exact delegation allowlist. The
list replaces rather than extends the native value, so preserve every default
specialist explicitly:

```json
{
  "agents": {
    "orchestrator": {
      "delegates": [
        "deep",
        "fixer",
        "general",
        "explorer",
        "librarian",
        "oracle",
        "looker",
        "designer"
      ]
    }
  }
}
```

Do not hard-code a model unless the project needs a dedicated route. Screenshot,
image, PDF, or video observation stays with `looker`; the parent passes structured
observations to `designer`. Promote `designer` into the native roster only if repeated
evidence shows incorrect routing, context pollution, or a required capability boundary
that configuration cannot express.

The orchestrator prompt asks for bounded child objectives, explicit
deliverables, non-overlapping writers, dependency-aware scheduling, and parent
verification. Child output is evidence for the parent; it is not automatically
the final answer.

## The three configuration layers

### Agent definitions

`agents.<name>` controls one Agent's stable behavior:

- `description` and `prompt`;
- `mode`: `primary`, `subagent`, or `all`;
- `model` plus either `reasoning` or provider-specific `variant`;
- `temperature`, `top_p`, provider options, and `steps`;
- exact model-visible `tools`;
- exact direct-child `delegates`;
- per-tool `permission`.

`reasoning` and `variant` require an explicit `model` and are mutually
exclusive. Canonical reasoning values are:

```text
off, low, medium, high, xhigh, max
```

Use an Agent-level model when that Agent must always use the same route,
regardless of the active team preset.

### Presets

`presets` are switchable team-wide routes. They map existing agents and semantic
categories to a qualified `provider/model` and optional provider-neutral
reasoning level. Presets do not create agents, grant tools, change permissions,
or authorize delegation.

Use presets when the same Agent roster should switch between teams such as
`balanced`, `fast`, and `thorough`.

A preset may route different Agents through different providers. This is the
preferred way to combine, for example, a long-context Claude model for
orchestration and planning, a GPT model for implementation and architecture
review, and a smaller multimodal model for exploration or visual inspection.
The providers remain independently configured in the shared catalog; the
preset contains only qualified model routes and canonical reasoning levels.
See the checked
[`zuno-multi-provider.json`](../examples/config/zuno-multi-provider.json) for
`myopenai`, `kiro-local`, and mixed `hybrid` teams covering the complete native
user-Agent roster.

### Per-task overrides

The `task` tool accepts `model` and `effort` for one child only. This is useful
when the parent discovers that one particular task needs a different model or
reasoning level. It is less suitable than configuration for a stable policy,
because the orchestrator model must choose to send the fields.

## Recommended switchable configuration

This example defines a custom release reviewer and routes the team through one
active preset:

```json
{
  "default_agent": "orchestrator",
  "subagent_depth": 1,
  "agents": {
    "orchestrator": {
      "delegates": [
        "deep",
        "fixer",
        "general",
        "explorer",
        "librarian",
        "oracle",
        "release-reviewer"
      ]
    },
    "release-reviewer": {
      "description": "Reviews release safety, evidence, and rollback readiness.",
      "mode": "subagent",
      "prompt": "Review only the supplied release scope. Do not delegate. Return findings, evidence, and residual risk.",
      "tools": [
        "read",
        "glob",
        "grep",
        "lsp",
        "webfetch",
        "web_search"
      ],
      "permission": {
        "mode": "standard",
        "rules": {
          "*": "deny",
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "lsp": "allow",
          "webfetch": "allow",
          "web_search": "allow"
        }
      }
    }
  },
  "preset": "balanced",
  "presets": {
    "balanced": {
      "agents": {
        "orchestrator": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        },
        "deep": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        },
        "fixer": {
          "model": "myopenai/primary-model",
          "reasoning": "medium"
        },
        "general": {
          "model": "myopenai/primary-model",
          "reasoning": "medium"
        },
        "explorer": "myopenai/fast-model",
        "librarian": "myopenai/fast-model",
        "oracle": {
          "model": "myopenai/primary-model",
          "reasoning": "max"
        },
        "release-reviewer": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        }
      },
      "categories": {
        "cheap": "myopenai/fast-model",
        "deliberate": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        }
      }
    }
  }
}
```

The example deliberately leaves `model` and `reasoning` out of the Agent
definitions. A configured `agents.<name>.model` is more specific than a preset
route and therefore wins. If preset switching should affect an Agent, keep its
model choice in the preset.

The `tools` array is an exact final allowlist, not an additive list. Avoid
setting it on `orchestrator` unless the complete desired surface is known. If
it hides `task`, the Agent cannot delegate even when `delegates` is present.
Configured workflow and Council tools must likewise remain visible if the
orchestrator is expected to call them.

The `delegates` array is also exact. It narrows the built-in target set and can
add valid configured subagents. Zuno rejects unavailable names rather than
silently ignoring them.

## Direct child model precedence

For `task` with `subagent_type`, Zuno chooses the child model in this order:

1. valid `task.model`;
2. `agents.<target>.model`;
3. `presets.<active>.agents.<target>`;
4. the parent session model.

An unavailable or unqualified model produces a visible routing diagnostic and
falls through to the next configured candidate. Model ids must use
`provider/model` form and exist in the resolved catalog.

Reasoning or variant resolution is:

1. valid `task.effort`;
2. the reasoning or variant attached to the model route that won;
3. the selected model/provider default.

One important consequence is that an explicit `task.model` without
`task.effort` does not inherit the configured target Agent's reasoning level.
It uses the explicit model's default.

Example one-child override:

```json
{
  "description": "Trace auth call chain",
  "prompt": "Trace login, refresh, logout, credential storage, and every caller. Return exact source evidence.",
  "subagent_type": "explorer",
  "model": "myopenai/fast-model",
  "effort": "low",
  "background": true,
  "reportDelivery": "nextStep"
}
```

`subagent_type` and `category` are mutually exclusive.

## Category routing

A category is a semantic model tier, not an Agent name:

```json
{
  "description": "Summarize test failures",
  "prompt": "Classify the supplied failures and identify the first actionable cause.",
  "category": "cheap",
  "background": false
}
```

Category delegation always runs the `general` Agent. Its model order is:

1. valid `task.model`;
2. `presets.<active>.categories.<category>`;
3. the parent session model.

`agents.general.model` and `presets.<active>.agents.general` are not consulted
for a category call. Configure the category itself. If the active preset does
not define the requested category, Zuno emits a diagnostic and uses the parent
session model.

Categories are useful when a prompt means “use the inexpensive tier” rather
than “use the repository explorer.” Use `subagent_type` when the specialist's
prompt, tools, permissions, or output contract matters.

## Top-level Agent model routing

The selected primary Agent follows a related but separate precedence:

1. an explicit client/runtime model selection;
2. `agents.<selected>.model`;
3. `presets.<active>.agents.<selected>`;
4. top-level `model`;
5. normal catalog selection.

A TUI manual model or reasoning selection affects the top-level Agent. It does
not erase preset routes for later child agents.

## Background jobs and report delivery

`background: false` runs the child in the foreground. Parent interruption
reaches the child, and the task call settles only after child shutdown drains.

`background: true` commits a durable queued job before capacity admission and
returns a job id. `reportDelivery` then controls the terminal result:

- `nextStep` settles the child, admits its report to the parent's durable inbox,
  and wakes the parent for another turn;
- `quiet` persists the result without injecting a parent input.

`reportDelivery` is valid only for background work. On process restart, a job
that never began becomes cancelled; a job that was already running becomes
uncertain and is never replayed.

Use background delegation for independent research or long-running work. Keep a
dependency on the critical path in the foreground unless the parent has other
useful work and can reliably consume a later report.

## Interacting with an attached child

The TUI treats each observed native child as a complete session surface rather than a
details popup. Use `Ctrl+X Down` to enter the first direct child, `Ctrl+X Left` or
`Ctrl+X Right` to cycle siblings, and `Ctrl+X Up` to return to the parent. Each child keeps
its own composer draft while navigation changes the visible session.

Pressing Enter in a running child admits the text to that child's durable inbox and steers
its active turn. Pressing Enter after the child settles admits a continuation and wakes the
same child identity with its resolved Agent, model, effort, permissions, and orchestration
lineage. The resolved continuation identity is persisted in the child session metadata, and
the TUI rebuilds the durable child tree and retained transcript when the parent is resumed in
a later process. A restored child therefore supports continuation rather than becoming a
read-only historical panel. Child text is literal: slash-looking input such as `/help` is sent
to the child and is not executed as a root TUI command. Product-agent invocations and workflow
projections are not presented as resumable child conversations.

Direct child input opens the same complete `TurnHost` path as a root turn. It receives the
assembled tool dispatcher, normal permission and question bridges, cancellation and lifecycle
events, durable usage, retry behavior, and proactive or recovery compaction. This is engine
parity, not an authority expansion: recursive `task` registration still requires the child
Agent's explicit delegate set and remaining depth described below.

## Depth, permissions, and capacity

Top-level `subagent_depth` defaults to `1`. It is a hop ceiling, not a grant.
Increasing it does not automatically give a child the `task` tool, add delegate
targets, or loosen permissions. The built-in specialists intentionally do not
delegate recursively.

Delegation uses the `task` permission domain. An Agent needs all of:

1. the model-visible `task` tool;
2. a target in its exact `delegates` set;
3. permission to call `task` for that target;
4. remaining `subagent_depth`;
5. available process-local delegation capacity.

Native tasks, workflow nodes, Council seats, and product agents share
`concurrency.delegations` for one workspace process. Background jobs wait in a
fair FIFO queue. Changing the bound does not cancel active work, and separate
Zuno processes do not yet share one durable quota lease.

## Configuration-owned workflow DAGs

Use `workflows` when the allowed agents, dependencies, and maximum parallelism
must be immutable configuration rather than a graph invented by the
orchestrator:

```json
{
  "workflows": {
    "release-check": {
      "maxParallel": 2,
      "maxAgents": 4,
      "nodes": [
        {
          "id": "code",
          "agent": "deep",
          "description": "Review implementation and tests",
          "prompt": "Inspect implementation correctness and regression coverage.",
          "dependsOn": []
        },
        {
          "id": "upstream",
          "agent": "librarian",
          "description": "Verify upstream constraints",
          "prompt": "Check current upstream documentation, releases, and authorization constraints.",
          "dependsOn": []
        },
        {
          "id": "risk",
          "agent": "oracle",
          "description": "Integrate residual risk",
          "prompt": "Use the completed evidence to identify release blockers and rollback requirements.",
          "dependsOn": ["code", "upstream"]
        }
      ]
    }
  }
}
```

The model invokes the immutable template through:

```json
{
  "workflow": "release-check",
  "prompt": "Review release candidate v0.4.0.",
  "description": "Release readiness",
  "background": true,
  "reportDelivery": "nextStep"
}
```

The model can select a configured template and supply the root prompt. It
cannot change the graph, agents, dependencies, or concurrency limits. Workflow
nodes reuse direct Agent model routing:

1. `agents.<node-agent>.model`;
2. active preset Agent route;
3. parent session model.

`maxParallel` defaults to 4, `maxAgents` defaults to 12, and both are validated
in `1..=64`. Zuno rejects duplicate ids, missing dependencies, cycles, unknown
or disabled agents, and a parallel bound larger than the agent bound.

The scheduler is work-conserving but publishes durable results in template
order. A node begins only after all declared dependencies complete
successfully.

## Council

Council is another consumer of the same native child-turn path. It does not
have a separate model router, permission bypass, or scheduler. Each seat names
an Agent, and that Agent resolves through the same configured Agent and preset
route.

The TUI `/council` launcher adds a one-turn routing instruction that asks the
current Agent to invoke `council_run` once in the background with `nextStep`
delivery. The original user message and resulting job remain durable.

Use Council when multiple independent perspectives should assess the same
question. Use a workflow DAG when seats have different prompts or dependencies.
Use direct `task` calls when the orchestrator should adaptively decide whether
delegation is useful.

## First-party Skills and slash entry points

The resources in `crates/zuno-orchestration/src/skills` are embedded Skill
descriptors and Markdown bodies. They are not CLI subcommands and are not copied
into the user's config directory during installation. Mounting the first-party
profile publishes them into the same immutable catalog as user and extension
Skills, with source identity, digest, profile visibility, and required-tool
metadata.

When one advertised Skill name is unambiguous and does not collide with a real
command, the TUI exposes it directly as `/<skill-name>`. The slash entry loads
the selected Skill before the next model request; it does not create a second
host-side command handler. Ambiguous names remain selectable through the Skill
picker or typed `skill` tool.

`/develop-zuno` is the first-party authoring guide for deciding whether a change
belongs in configuration, Agent Markdown, a user-owned Skill or command, an
`extension.json` package, or native Rust. It links the repository's current
configuration, plugin, process-plugin, orchestration, and runtime contracts.
User-specific policies such as `dual-review` and `auto-release` remain external
Skills or commands; Zuno does not compile those workflows into the product.

## TUI and CLI controls

In the TUI:

- `/agent` opens the Agent picker;
- `/model` opens the model picker;
- the reasoning control cycles only levels declared by the selected model;
- `/preset` opens the preset picker;
- `/preset <name>` applies a configured preset to the current session.

Switching a preset remounts the prepared runtime composition without
interrupting an in-flight turn. It clears prior manual top-level model and
reasoning overrides so the selected team's routes take effect. Set top-level
`preset` for the startup default.

The headless `run` command currently uses the configured top-level `preset`;
there is no separate `--preset` flag. Prefer a project config, an explicit
config layer, or a dedicated preset-specific launch configuration for
repeatable automation.

Useful inspection commands are:

```sh
# Resolved Agent catalog and effective capability rules.
zuno agent list

# One Agent's catalog definition.
zuno debug agent orchestrator

# Merged, validated configuration and source layers.
zuno debug config

# Available models and declared model capabilities.
zuno models myopenai --verbose

# Active permission policy.
zuno debug permissions
```

After a real turn, `zuno debug prompt` can inspect prompt provenance. Use
`--show-sensitive` only when it is safe to print full model-visible instruction,
AGENTS, skill, and memory content.

## Common configuration mistakes

### “The preset does not change this Agent”

`agents.<name>.model` is more specific and wins over the preset. Remove the
fixed Agent model if the preset should own it.

### “The orchestrator no longer delegates”

An exact `tools` allowlist hid `task`, an exact `delegates` list removed the
target, the permission policy denied the target, or `subagent_depth` was
exhausted. Inspect `zuno agent list` and `zuno debug permissions`.

### “My category uses the wrong model”

Configure `presets.<active>.categories.<category>`. Category calls do not use
the `general` Agent's configured or preset Agent route.

### “The explicit child model ignored the Agent's reasoning”

This is intentional. `task.model` selects a new route. Supply `task.effort` too,
or let that model use its own default.

### “The custom reviewer cannot be targeted”

Set `mode` to `subagent` or `all`, keep it enabled, and add its exact name to
the delegating Agent's `delegates` list when that list is configured.

### “A configured workflow does not appear”

The selected Agent must expose the native `workflow` tool and have `task`
permission for every node Agent. A published configuration with unknown agents,
cycles, or invalid limits is rejected during assembly rather than registered as
a placeholder command.

## Related documentation

- [Configuration reference](reference/configuration.md)
- [Harness runtime](harness-runtime.md)
- [Plugins, custom agents, and workflows](plugins.md)
- [Agent orchestration execution roadmap](design/agent-orchestration-execution-roadmap.md)
