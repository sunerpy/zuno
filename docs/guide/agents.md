# Agents

An agent is a contract: a prompt, a model route, an exact tool surface, permission rules,
and a delegation boundary. Selecting an agent is how you choose both what work gets done
and what authority is available to do it.

The direction matters. An agent contract can only *narrow* authority. It cannot widen it,
which is what makes a read-only agent a guarantee rather than a default that
configuration can quietly reverse.

## The built-in roster

| Agent | Responsibility | Delegation |
| --- | --- | --- |
| `orchestrator` | Own the outcome, partition work, integrate results, verify completion | May delegate |
| `build` | Direct end-to-end implementation in one lane | No child tools |
| `plan` | Read-only research and implementation-ready planning | No child tools |
| `deep` | Deep-work mode, or delegated root-cause and cross-cutting implementation | No recursive delegation |
| `fixer` | Focused local change and its regression scope | No recursive delegation |
| `general` | Bounded work with no narrower specialist | No recursive delegation |
| `explorer` | Read-only repository and call-chain research | No recursive delegation |
| `librarian` | Current external documentation and upstream research | No recursive delegation |
| `oracle` | Read-only architecture and root-cause review | No recursive delegation |
| `looker` | Visual artifact inspection | No recursive delegation |

`orchestrator` is the default and the only native primary agent exposing the `task`
delegation tool. `deep` has mode `all`, so it can be selected directly as a session agent
while `orchestrator` can also target it; direct selection does not grant it recursive
delegation.

## Choosing one

```sh
zuno run --agent plan "why does the retry budget start before the first attempt?"
zuno run --agent build "add pagination to the /users endpoint and run the tests"
zuno run --agent deep "the compaction boundary drops the tail on resume; find the root cause"
zuno tui --agent orchestrator
```

A practical rule:

| Situation | Agent |
| --- | --- |
| You want an answer or a plan, and no writes | `plan` |
| A well-scoped change in one area | `build` |
| A local fix plus its regression scope | `fixer` |
| A hard cross-cutting problem | `deep` |
| Work that fans out across independent pieces | `orchestrator` |
| Read-only code archaeology | `explorer` |
| Current external documentation | `librarian` |

Selection resolves in order: an agent explicitly selected by the client, then top-level
`default_agent`, then built-in `orchestrator`.

## How a contract narrows authority

Four layers apply, and every one of them can only remove capability:

1. The parent Attempt's actual provider-visible tool schemas, for a delegated turn.
2. The target agent role and its extension-tool inheritance policy.
3. The agent's exact `tools` allowlist, when configured.
4. Effective user and agent permission rules.

An `allow` cannot restore a tool that was absent from the parent Attempt, and
`permission.mode: "allow_all"` suppresses prompts without widening this intersection.
Schema identity counts: a same-named tool with a different provider-visible schema is
outside the bound.

The sandbox follows the same one-way rule. A read-only agent receives `read-only`
confinement even when the invocation selected `workspace-write` or
`danger-full-access`:

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## Read-only is a role boundary, not just a sandbox mode

`explorer` is native-read-only rather than shell-read-only. Its default surface is
`read`, `glob`, `grep`, and read-only `lsp`; `shell`, edits, delegation, and network
research are denied. Commands such as `du`, `stat`, and `file` are executables reached
through `shell`, so they do not belong to `explorer` even though they only read.

Global `permission.mode: "allow_all"` skips ordinary confirmation but does not erase that
explicit deny. When command-based inspection is needed, delegate to a shell-capable agent
such as `deep` or `general`, or run the bounded command in the parent session.

## Plan mode

`/plan` in the terminal application switches collaboration mode, and the restriction is
enforced below the prompt by a deny-by-default capability overlay: repository inspection,
read-only LSP and search, questions, Skills, and typed Goal/Plan/Todo operations are
allowed while shell and file mutation are denied.

Returning to Work mode requires a durable plan to exist, and the confirmation names its
title, revision, and completed-step count. The model can recommend starting work but
cannot select it for you. A confirmed selection is persisted as the session agent, so a
resume restores the mode.

## Inspecting what an agent actually resolves to

```sh
zuno agent list
zuno debug agent explorer
zuno debug permissions
```

`debug agent` reports the effective agent-filtered view, including metadata and
selected-body Skill budgets, rendered and omitted coverage, and a bounded preview.
`debug permissions` reports both the configured and the effective permission mode. Use
these rather than inferring the result from configuration, because global and project
definitions overlap.

## Custom agents

An agent is defined either in `zuno.json` under `agents.<name>` or as a Markdown file
with frontmatter under `.zuno/agent/`:

```sh
zuno agent create --path .zuno/agent/reviewer.md --mode subagent \
  --description "Review diffs for regressions" --model openai/gpt-5
```

A configured or extension agent whose mode is `subagent` or `all` can join the delegation
roster. A `primary`-only agent cannot be delegated to. The complete field list is in
[Custom agents](/config/custom-agents), and delegation mechanics are in
[Orchestration](/orchestration).
To package an Agent with Skills or tools, or to implement WASI/native behavior,
see [Developing agents and extensions](/guide/extension-development).

## See also

- [Custom agents](/config/custom-agents)
- [Tools](/guide/tools)
- [Permissions and sandboxing](/guide/permissions)
- [Orchestration](/orchestration)
- [Developing agents and extensions](/guide/extension-development)
