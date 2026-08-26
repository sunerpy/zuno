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
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model"
}
```

Use an editor-specific schema association or an absolute file URI when the config file and repository schema are not in the same tree. A later documentation deployment can publish this unchanged artifact at a stable HTTPS URL.

The complete checked starter is [`examples/config/zuno.json`](../../examples/config/zuno.json). It declares a native `openai` transport and contains no package-manager or AI SDK field. Follow the [provider initialization guide](providers.md#first-run-initialization) to install it, edit the endpoint and model ids, and store credentials.

## Main config versus TUI config

`theme`, `mouse`, key bindings, prompt dimensions, diff layout, and notification settings do **not** belong in `zuno.json`. They belong in `tui.json` or `tui.jsonc` at the corresponding global or project configuration layer.

The default enables application-owned mouse interaction:

```json
{
  "theme": "system",
  "mouse": true,
  "leader_timeout": 5000
}
```

`leader_timeout` is milliseconds. Its default is 5000, so the `Ctrl+X` continuation
overlay remains readable for five seconds unless another key completes or cancels the
sequence first.

With `mouse` absent or `true`, Zuno captures button, drag, release, and wheel events. The transcript and both root and child composers provide their own selection and copy behavior. Releasing a drag copies the selected text through the configured clipboard and leaves the highlight visible. Transcript selection remains clamped instead of crossing into the sidebar; tool and sidebar disclosure rows are clickable, and an overflowing conversation mounts a draggable scrollbar.

Set `"mouse": false` to opt out of those interactions and return drag selection to the terminal. In that mode native selection may cross the transcript, sidebar, and input area; terminals that implement alternate-scroll mode can still translate wheel notches into transcript scrolling while the composer is empty. The composer still renders a high-contrast theme-derived caret for keyboard editing.

The `system` theme queries the terminal's OSC 10/11 foreground and background colours before the TUI starts, then derives its text, panel, input, border, and syntax colours from that result. Terminals which do not support colour queries fall back to `COLORFGBG`; if neither source is available, Zuno applies a neutral root, panel, and input hierarchy so the interface does not collapse into one near-black surface.

## Product subagents

`productAgent` is a map of named, default-off Codex or Claude Code instances. Each enabled instance contributes one statically named tool:

```json
{
  "productAgent": {
    "codex": {
      "kind": "codex",
      "enabled": true,
      "command": "codex",
      "toolName": "subagent_codex",
      "permissionMode": "never"
    },
    "claude-code": {
      "kind": "claude-code",
      "enabled": true,
      "command": "claude",
      "toolName": "subagent_claude_code",
      "permissionMode": "dontAsk"
    }
  }
}
```

Instances inherit the Zuno process environment, working directory, and the product's native configuration and login. An optional `env` object overlays inherited variables, including proxy variables. Zuno does not copy Codex or Claude Code tokens into its provider credential store.

Dangerous modes `dangerouslyBypassApprovals` and `bypassPermissions` are accepted only when written explicitly. Tool names must be unique and cannot collide with native tools. See [Codex and Claude Code product agents](../design/product-agents.md) for protocol, job, cancellation, and TUI behavior.

## Concurrency

Independent runtime work has four bounded controls:

```json
{
  "concurrency": {
    "tool_calls": 8,
    "delegations": 8,
    "mcp_connections": 8,
    "lsp_requests": 4
  }
}
```

- `tool_calls` limits model-issued calls that explicitly declare themselves safe
  to overlap. Permission prompts and argument preparation remain ordered. The
  bound also applies when a single safe group is larger than the limit;
  `Exclusive` calls drain all earlier safe/background calls and prevent later
  calls from starting until the barrier settles.
- `delegations` is the per-process cap shared by every turn host for the same
  workspace process: native child sessions, workflow nodes, Council seats, and
  Codex/Claude Code product agents all consume it. Background work from an
  earlier turn continues to consume capacity. Waiting delegations are admitted
  in fair FIFO order, and a workflow's `maxParallel` remains an additional,
  narrower bound. A workflow immediately refills a free slot with the next ready
  node in template order; it does not wait for the slowest node in an earlier
  wave. Parent cancellation takes priority over same-tick final completion.
  Independent Zuno processes do not yet coordinate this quota.
- `mcp_connections` limits simultaneous lifecycle operations across different
  MCP servers. One server's operations remain serialized.
- `lsp_requests` is the shared cap for language-server startup and request fan-out
  across servers.

Each field accepts `1..=64`; omission uses the values above. Set a field to `1`
to restore serial behavior for that layer.

Background native and product-agent jobs are persisted as `queued` before they
wait for delegation capacity and become `running` only after admission. If the
process restarts, a still-queued job is safely cancelled because its runner never
started; a running job becomes `uncertain` and is never replayed.

Foreground native `task` delegation is not detached: it inherits the parent
turn interrupt, aborts the live child turn when fired, and waits for child drain
and runtime shutdown before the tool call settles.

## Agent model presets

Presets are typed team-wide model routes. They select a model and optional
provider-neutral reasoning level for an Agent or semantic workflow category;
they do not create Agents, grant tools, change permissions, or authorize
delegation.

See [agent orchestration and model routing](../orchestration.md) for complete
Agent definitions, direct delegation, category routing, background delivery,
workflow DAGs, and Council.

```json
{
  "preset": "house",
  "presets": {
    "house": {
      "agents": {
        "orchestrator": {
          "model": "myopenai/primary-model",
          "reasoning": "max"
        },
        "deep": {
          "model": "myopenai/primary-model",
          "reasoning": "high"
        },
        "explorer": "myopenai/fast-model"
      },
      "categories": {
        "cheap": "myopenai/fast-model"
      }
    }
  }
}
```

Every preset body must use the explicit `agents` and/or `categories` objects;
flat compatibility forms and provider-specific `variant` fields are rejected.
The expanded form accepts `reasoning` values `off`, `low`, `medium`, `high`,
`xhigh`, and `max`. A bare `provider/model` string leaves reasoning unchanged.

For direct `task` delegation with `subagent_type`, model precedence is an
explicit task `model`, `agents.<target>.model`, the active preset's Agent route,
then the parent session model. For `category`, the child is the `general` Agent
and model precedence is an explicit task `model`, the active preset's category
route, then the parent session model; the `general` Agent route is deliberately
not consulted. Effort precedence is explicit task `effort`, the reasoning or
variant attached to the winning route, then the model/provider default.

The top-level turn follows the same specificity principle for its selected
Agent. An unavailable route produces a visible diagnostic before falling
through; Zuno never ships hidden model-id defaults. The selected preset is
frozen with the turn plan, so editing configuration cannot mutate an in-flight
attempt.

In the TUI, `/preset` opens the configured preset picker and
`/preset <name>` selects one directly. The replacement is prepared and applied
inside the current TUI; it does not restart the interface or interrupt an
in-flight turn. A preset switch clears prior manual model and reasoning
overrides so the selected team's routes take effect. A later explicit model or
reasoning choice overrides that team route for the top-level Agent while the
preset continues to route delegations. The choice is session-local runtime
state; set the top-level `preset` key to make a team the startup default.

## Native Council launcher

The TUI exposes `/council` only when the active Agent's final capability
snapshot can actually reach the native `council_run` tool. For example, the
native `orchestrator` Agent exposes it while a non-delegating `build` Agent does
not. An Agent tool allowlist or permission rule that hides `council_run` also
hides the launcher, so the picker cannot advertise a run the dispatcher would
later reject.

- `/council` opens the frozen Council preset picker.
- `/council <preset> <question>` launches that preset for the exact question.
- A launch submitted during an active turn enters the durable FIFO queue; it is
  not used as a mid-turn steering message.

The exact slash text remains the durable user message. For that turn only, Zuno
adds a typed `routing.council` developer-context block that requires one
permission-gated `council_run` invocation with background execution and
`nextStep` report delivery. It does not synthesize assistant/tool history or
create a second Council executor. The existing native service continues to own
seat isolation, concurrency, retry, deadline, quorum, cancellation, synthesis,
durable job state, and parent report delivery.

Once launched, the right-sidebar `Jobs` section shows Council preset, aggregate
status, elapsed time, token usage, and durable seat progress. Its inspection hint
uses the configured `session_child_first` binding (the default is the leader plus
Down) and always keeps `/subagent` available. `/subagent` shows each seat's Agent,
status, timing, and diagnostics without presenting the seat as a resumable child
session. Ordinary configured workflows use the same projection for node progress.

## Context compaction

Zuno can compact older conversation history before the model window is exhausted:

```json
{
  "compaction": {
    "auto": true,
    "threshold_percent": 80,
    "tail_turns": 2,
    "reserved": 12000
  }
}
```

- `threshold_percent` accepts `1..=100` and defaults to `80`. It is applied to
  the usable context window after the model's output allowance and configured
  reserve are removed.
- `auto: false` disables proactive threshold compaction. Manual `/compact`
  remains available.
- A provider-confirmed context-limit failure still uses the bounded compaction
  recovery path before retrying; this is recovery from an already failed
  request, not the proactive threshold.
- `/compact` persists the summary through the same durable compaction pipeline,
  so subsequent turns and resumed clients see the same retained history.

## Plugin packages

Plugins are package directories, not `zuno.json` fields. Install them globally
or below the selected project's `.zuno/extensions` directory:

```sh
zuno plugin add /path/to/package --project
zuno plugin list
zuno plugin update /path/to/package --project
zuno plugin remove package-id --project
```

Custom agents and workflows use native tool permissions for file, network, and
environment access. Runtime tools use an explicitly granted WASI component or a
contained `host.full` process. See [plugins, custom agents, and
workflows](../plugins.md).

## Permission modes and rules

Zuno accepts one permission shape only: `permission.mode` selects the HITL
policy and `permission.rules` carries ordered per-tool rules. Legacy string,
direct-rule, and `authorization.strict` forms are rejected.

To run tool calls without Zuno HITL prompts, use `allow_all`:

```json
{
  "permission": {
    "mode": "allow_all",
    "rules": {}
  }
}
```

For a narrower shell-oriented configuration, keep standard mode and allow both
the shell and its external-path escalation:

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "bash": "allow",
      "external_directory": "allow"
    }
  }
}
```

An explicit `deny` still wins in every mode. `allow_all` bypasses HITL only; it
does not bypass sandboxing, argument validation, explicit denies, or the shell's
destructive-command safety gate. Use `zuno debug permissions` and
`zuno debug config` to inspect the effective policy.

## Strict HITL authorization

Strict mode requires a fresh decision for every side-effecting invocation:

```json
{
  "permission": {
    "mode": "strict",
    "rules": {}
  }
}
```

Strict mode evaluates explicit rules before its fresh-approval gate:

- An explicit `deny` remains terminal.
- `allow`, a plugin allow, an earlier "Allow always", and TUI `--auto` cannot
  authorize a side effect.
- The prompt offers only "Allow once" and "Reject"; approval is never remembered
  for a later call.
- A headless run fails closed because it has no user to ask.

Native file reads, `glob`, `grep`, skill/session/job/goal inspection, read-only
LSP operations, MCP resource reads, `webfetch`, and `web_search` do not receive
the extra strict prompt. `bg list/output/wait` are read-only while `bg cancel`
requires approval. Shell, file writes, durable state changes, delegation,
product agents, extension lifecycle mutations, and unknown harness or MCP tools
are side-effecting by default.

`bash` always requires strict approval, even for a command such as `rg`, because
the shell is not an operating-system sandbox. Use the native `grep` or `glob`
tool when the intended operation is read-only.

Independently of strict mode, the shell risk gate requires fresh approval before
bounded destructive operations or replacing an existing redirect target. New
static files under the working directory or OS temporary directory are treated
as creation. An exact, non-recursive `rm -f` of a statically named path that is
currently absent below the OS temporary directory is treated as no-op cleanup;
an existing target, recursive removal, dynamic target, or overwrite still
requires approval. There is no tool argument that lets a model approve its own
risky call, and an explicit permission deny always wins.

## Skill discovery

Zuno discovers skills in this scope order:

1. project `.zuno/skill(s)` roots from the current directory to the worktree;
2. project `.agents/skills`, then `.claude/skills`, over the same walk;
3. Zuno's global and configured config directories;
4. global `~/.agents/skills`, then `~/.claude/skills`;
5. explicit `skills.paths`;
6. configured remote indexes.

Project scope is therefore advertised before user-global scope. Zuno never
scans `.opencode` or `$XDG_CONFIG_HOME/opencode` for skills. The same canonical
source path is de-duplicated, including symlink aliases, while same-named files
from different sources remain independently addressable; no hidden winner is
selected. Set `ZUNO_DISABLE_EXTERNAL_SKILLS=1` to disable `.agents` and
`.claude` roots, or `ZUNO_DISABLE_CLAUDE_CODE_SKILLS=1` to disable only Claude
skill roots. Zuno-native `.zuno` roots remain enabled by the broad external
switch.

The model prompt receives a bounded catalog rather than every `SKILL.md` body.
By default its approximate budget is two percent of the model context (8,000
characters when the context is unknown), capped at 10,000 tokens. Configure it
under `skills`:

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000
  }
}
```

`includeInstructions: false` removes both the trigger policy and catalog from
model prompts. The `skill` tool still supports paged `list` and `search`
discovery. `load` and `read_resource` return content-bound continuation cursors;
the caller must read through `complete: true` before applying the instructions.

Use `zuno debug skill` after restarting to inspect the exact catalog and source
locations visible to a session. A generic prompt such
as "follow skill guidance" does not select every skill; a skill is loaded only
when its name is explicit or its description clearly matches the request.

Zuno also compiles seven original first-party Skills into the
`zuno-orchestration` pack: `customize-zuno`, `deepwork`, `codemap`,
`verification-planning`, `reflect`, `worktree`, and `git-workflow`. Each has a
stable `builtin://zuno-orchestration/...` source, content hash, provenance,
allowed Agent profiles, and required-tool declaration. The active profile and
its declared tool visibility filter the advertised set; selecting a Skill can
never widen the runtime capability snapshot.

An unambiguous Skill that does not collide with a real command is directly
invokable as `/<skill-name>`. Zuno resolves that exact advertised source and
loads its body before the next provider request. Same-named Skills from multiple
sources deliberately disable the ambiguous direct slash form; use the Skill
picker or the typed `skill` tool with an exact source instead. The current
`worktree` Skill performs preflight guidance only. Worktree creation, leases,
cleanup, and quota enforcement remain runtime services planned separately.

## Memory learning

Resident memory is enabled by default, but model and reflection writes enter an
auditable candidate queue first:

```json
{
  "memory": {
    "resident": true,
    "tool": true,
    "reflection": true,
    "global_char_limit": 2200,
    "project_char_limit": 3000,
    "nudge_interval": 10,
    "promotion": "review",
    "auto_confidence": 0.9
  }
}
```

- `resident` injects the frozen global and project memory blocks into prompts.
- `tool` exposes `memory_propose`; it never grants direct file mutation.
- `reflection` reviews completed delivered turns with `small_model`. Reflection
  is disabled with a visible diagnostic when no explicit reachable small model
  is configured; Zuno does not silently spend the session model.
- `nudge_interval` triggers periodic review every N durably recorded delivered
  assistant messages. The count survives host rebuilds and process restarts. A
  verified recovery can trigger review earlier; zero disables only the periodic
  trigger.
- `promotion` is `review` (default), `high_confidence`, or `automatic`.
  `high_confidence` applies only candidates at or above `auto_confidence`.
- `auto_confidence` is a finite value in `0..=1` and defaults to `0.9`.

Each reflection request contains the exact resident-memory snapshot so the
reviewer can propose audited `replace` or `remove` operations instead of adding
duplicates. An expired running reflection job becomes `uncertain` and is never
automatically replayed.

`memory: false` disables resident injection, proposal tools, and reflection.
`/memory` reviews, edits, approves, rejects, removes, and undoes durable changes.
See [auditable memory and reflection](../design/memory-learning.md).

## Inspecting the result

Use `zuno debug paths` to inspect resolved roots and `zuno debug config` to inspect the merged configuration. A validation error names every rejected top-level key; for example, putting `theme` in `zuno.json` is rejected because it belongs in `tui.json`.
