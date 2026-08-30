# Workflows and commands

Two different things share this page because people reach for the wrong one. A
command is a prompt template the user triggers with a slash. A workflow is an
immutable multi-agent DAG the runtime executes. A command asks the model to do
something; a workflow decides which agents run in what order.

Neither grants tools or bypasses agent permissions.

## Commands

A command is a literal prompt template with argument macros. Use one when the value
is the exact wording of a repeated request. If the value is reusable guidance that
should trigger on match, use a Skill instead — see
[Authoring Skills](/config/authoring-skills).

### Configuration form

Commands live in the `command` map, keyed by command name:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `template` | `string` | required | The prompt template |
| `agent` | `string` \| `null` | none | Agent to run the command as |
| `description` | `string` \| `null` | none | What the command does |
| `model` | `string` \| `null` | none | Model to run the command with |
| `subtask` | `boolean` \| `null` | none | Run the command in a subtask instead of the current session |
| `variant` | `string` \| `null` | none | Model variant to run the command with |

`template` is the only required field in the whole configuration schema's leaves.

```json
{
  "command": {
    "audit-deps": {
      "description": "Audit a dependency for supply-chain risk",
      "agent": "reviewer",
      "subtask": true,
      "template": "Audit the dependency $1 for supply-chain risk. Report maintenance status, transitive additions, and pinned version."
    }
  }
}
```

`subtask: true` matters more than it looks. Without it the template runs in the
current session and consumes its context; with it the work happens in a child and the
parent receives a report.

### Markdown form

Zuno loads `command/**/*.md` and `commands/**/*.md` recursively from the global config
directory and every project `.zuno` config directory. Frontmatter supplies the same
metadata; the body is the template.

```markdown
---
description: Audit a dependency for supply-chain risk
agent: reviewer
subtask: true
---

Audit the dependency $1 for supply-chain risk.
Report maintenance status, transitive additions, and pinned version.
```

The command name derives from the path with the `command/` or `commands/` prefix
stripped, so `command/review/security.md` is `/review/security`. Markdown commands
need no separate precedence level — configuration discovery loads them into the
`command` map before resolution sees it.

### Argument expansion

Expansion happens during resolution, before dispatch, so a dispatched prompt is
already final.

| Placeholder | Expands to |
| --- | --- |
| `$1`, `$2`, `$3` and so on | That single tokenized argument |
| The highest-numbered placeholder | Every remaining argument, joined by one space |
| A placeholder past the end of the argument list | Empty string |
| `$ARGUMENTS` | The raw untokenized input, with quotes, spaces, and newlines intact |

The greedy-highest rule is the one that surprises people: in `A=[$1] B=[$2]` with four
arguments, `B` receives three of them. That is what makes `$1 $2` usable for "a flag
and then free text" without quoting.

When a template mentions no placeholder at all and the input is not blank, the raw
input is appended after a blank line. So a template with no macros still receives
arguments rather than dropping them.

`$ARGUMENTS` preserves the input exactly as typed, which is why it is the right choice
for anything the model should see verbatim — a shell command, a diff, a quoted
sentence.

### Which definition wins

Four sources can define the same command name. Ascending precedence:

1. built-in `init` and `init-deep`;
2. `command` map entries, including Markdown commands;
3. MCP prompts, keyed `<server>:<prompt>`;
4. Skills — only when the name is still free.

Levels 2 and 3 overwrite unconditionally. Level 4 does not: a Skill never shadows a
built-in, a configured command, or an MCP prompt. An MCP prompt is keyed with its
server prefix, so it can only collide with a configured command whose key is literally
`server:prompt`.

An unambiguous Skill is already invokable as `/<skill-name>`, so do not add a command
file merely to create a slash entry.

Zuno does not register a built-in `/review`. Product- and organization-specific
workflows such as a review or release policy stay user-owned; define them as a Skill
or command with the exact semantics your project needs.

## Workflows

A workflow is a named, configuration-owned DAG template instantiated by the
model-facing workflow tool. The template is immutable at run time — the model chooses
whether to run it, not what it contains.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `nodes` | array of node | required | The immutable DAG instantiated by the model-facing workflow tool |
| `maxAgents` | integer \| `null` | `12` | Maximum nodes admitted into one run |
| `maxParallel` | integer \| `null` | `4` | Maximum simultaneously running nodes |

Each node:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `id` | `string` | required | Stable node id within the template |
| `agent` | `string` | required | Configured or built-in agent to run |
| `dependsOn` | `string[]` | `[]` | Node ids that must complete successfully first |
| `description` | `string` \| `null` | none | Human-readable purpose shown in runtime projections |
| `prompt` | `string` \| `null` | none | Optional node-specific instruction appended to the run prompt |

```json
{
  "workflows": {
    "release-check": {
      "maxParallel": 2,
      "maxAgents": 4,
      "nodes": [
        {
          "id": "code",
          "agent": "reviewer",
          "description": "Review the diff",
          "dependsOn": []
        },
        {
          "id": "upstream",
          "agent": "explorer",
          "description": "Check upstream breaking changes",
          "dependsOn": []
        },
        {
          "id": "synthesis",
          "agent": "deep",
          "description": "Reconcile both reports",
          "dependsOn": ["code", "upstream"]
        }
      ]
    }
  }
}
```

`dependsOn` requires successful completion, so a failed dependency stops its
dependents rather than letting them run on incomplete input. `maxParallel` bounds
simultaneous nodes and `maxAgents` bounds the whole run; both are validated, and both
exist because an unbounded fan-out is a cost and rate-limit problem, not just a
scheduling one.

Nodes name agents. A node's model comes from the active preset's category route, then
the parent session model — the `general` agent route is deliberately not consulted.
Use `categories` in a preset when nodes should not hard-code an agent name. See
[Model routing](/config/models).

Every agent named by a node must be delegable from the running agent. `delegates` on
the parent is an exact allowlist and applies to workflow nodes as well as direct
delegation, so a workflow cannot route around a narrowed contract. See
[Custom agents](/config/custom-agents).

## See also

- [Authoring Skills](/config/authoring-skills)
- [Custom agents](/config/custom-agents)
- [Agent orchestration](/orchestration)
- [Configuration reference](/reference/configuration)
