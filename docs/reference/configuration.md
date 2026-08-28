# Configuration reference

## Files

Zuno reads only `zuno.json` and `zuno.jsonc`. The global files are under `$XDG_CONFIG_HOME/zuno` (normally `~/.config/zuno`); project layers are bare `zuno.json[c]` files from the worktree root to the current directory and files under `.zuno/`. `ZUNO_CONFIG` adds one explicit file, `ZUNO_CONFIG_DIR` adds a directory, and `ZUNO_CONFIG_CONTENT` supplies the final environment layer.

Objects merge recursively from lower to higher precedence. Arrays and scalar values replace the lower value. The top level rejects unknown keys.

On the first ordinary discovery, Zuno creates a missing global `zuno.json` as
`{}` and a missing global `AGENTS.md` from its Zuno-authored starter guidance.
Creation uses exclusive new-file semantics and never overwrites either file.
An explicit `ZUNO_CONFIG`, `ZUNO_CONFIG_DIR`, or `ZUNO_CONFIG_CONTENT` launch
does not materialize defaults, so installation or one ordinary launch should
precede a profile-only first run.

### Global and project instructions

Zuno loads native instruction files in this order:

1. `$XDG_CONFIG_HOME/zuno/AGENTS.md`;
2. `ZUNO_CONFIG_DIR/AGENTS.md`, when the profile directory supplies one;
3. project directories from the worktree root to the current directory.

The base global file remains active when `ZUNO_CONFIG_DIR` selects a provider
or team. A profile-level file appends narrower, higher-priority guidance rather
than replacing the base global rules. In one project directory,
`AGENTS.local.md` replaces `AGENTS.md`; nearer directories are appended later.

The starter covers ownership, verification, scoped Git operations, and safe
worktree decisions. Detailed procedures remain in the built-in `git-workflow`
and `worktree` Skills so they are loaded only when relevant. Zuno does not copy
OpenCode, Codex, Claude, or another product's instruction file, and it never
overwrites a user-maintained global `AGENTS.md`.

### Switchable configuration overlays

Zuno does not currently have a named `--profile` flag. Use a final configuration
directory as a switchable overlay instead. A normal launch keeps the global and
project configuration unchanged; setting `ZUNO_CONFIG_DIR` adds one higher-precedence
directory containing `zuno.json` or `zuno.jsonc`:

```sh
zuno

ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

The overlay is a deep merge, so a provider defined there is added without deleting the
global provider. Top-level `model`, `small_model`, and a non-null `preset` replace their
lower-layer values. This distinction matters when switching a whole agent team: changing
only `model` changes the root turn, while an active lower-layer preset can keep delegated
Agents routed to its original provider. Do not use `"preset": null` as a tombstone:
optional typed fields currently treat JSON null as no higher-layer value, so the inherited
preset remains selected. Select an explicit overlay preset instead.

One `zuno.json` can declare several providers. Provider ids are catalog
namespaces, while presets choose which qualified `provider/model` route each
Agent uses. The checked
[`examples/config/zuno-multi-provider.json`](../../examples/config/zuno-multi-provider.json)
keeps both `myopenai` and `kiro-local` in one catalog and defines three teams:

- `myopenai` preserves an all-`myopenai` team;
- `kiro-local` uses the loopback gateway for the whole team;
- `hybrid` combines Kiro coding/reasoning models with
  `myopenai/us.anthropic.claude-fable-5` for long-context orchestration,
  planning, general work, and research.

Each team routes only Zuno's current user-facing roster: `orchestrator`,
`build`, `plan`, `deep`, `fixer`, `general`, `explorer`, `librarian`, `oracle`,
and `looker`. OMO Agent names and OMO categories are not copied. A category in
Zuno is an optional user-defined semantic route, so an unused category should
not be present merely because another harness defines one.

Use `/preset` in the TUI to inspect the teams, or switch directly:

```text
/preset myopenai
/preset kiro-local
/preset hybrid
```

The selection is session-local. To choose a startup team through the
environment, keep the providers and presets in the global file and make the
overlay only select the top-level defaults. For example,
`$HOME/.config/zuno/profiles/kiro/zuno.json` can contain:

```json
{
  "model": "kiro-local/claude-opus-5",
  "small_model": "kiro-local/gpt-5.6-luna",
  "preset": "kiro-local"
}
```

Then launch it with:

```sh
ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" zuno
```

The same pattern can select `hybrid` or return explicitly to `myopenai`; the
overlay need not duplicate either provider definition. Checked selector files
for all three teams live under
[`examples/config/profiles`](../../examples/config/profiles).

Model names belong to the provider's live account-aware catalog, not to an
authentication plugin or to every account uniformly. The checked example is
aligned with the 2026-08-28 compiled-provider validation, which observed
`claude-opus-5`, `low`/`medium`/`high`/`xhigh`/`max`, a 1,000,000-token
context limit, and a 128,000-token output limit. `kiro-provider` now accepts
inline Responses documents in its verified native formats. Zuno does not yet
have a native document request block or an end-to-end Kiro document test, so
the checked profile deliberately continues to advertise only text plus inline
image data. Keep the static Zuno capability catalog aligned with the path Zuno
has actually verified, not merely the broadest model-catalog declaration.

For a loopback [kiro-provider](https://github.com/sunerpy/kiro-provider)
Responses gateway, use a unique provider id such as `kiro-local`: Zuno checks
configured and stored credentials before the provider's environment-variable
list, so reusing an unrelated provider id can select an old credential. The
gateway and Zuno need the same private loopback key, but that key should remain
in the environment or a secret manager rather than in either JSON file:

```sh
KIRO_PROVIDER_API_KEYS="$ZUNO_KIRO_LOCAL_API_KEY" \
  /path/to/kiro-provider/dist/kiro-provider serve

ZUNO_CONFIG_DIR="$HOME/.config/zuno/profiles/kiro" \
ZUNO_KIRO_LOCAL_API_KEY="$ZUNO_KIRO_LOCAL_API_KEY" \
  zuno
```

The custom OpenAI `baseURL` route is handled by Zuno's native compatible
Responses adapter and appends `/responses`, so the Kiro API root in `baseURL`
must end in `/v1`. `kiro-provider` defaults to `127.0.0.1:8787`, fails closed
without a non-empty `KIRO_PROVIDER_API_KEYS`, and by default uses its
`opencode-shared` authentication authority. That shared account authority and
the local Bearer key are separate layers: the former authorizes the gateway
upstream, while the latter protects the loopback HTTP endpoint. The resolved
Responses surface is also preserved for title, compaction, summary, Council
synthesis, and memory reflection requests; those internal Agents do not
silently fall back to Chat Completions.

Set the provider options to:

```json
{
  "baseURL": "http://127.0.0.1:8787/v1",
  "maxTokens": null
}
```

The null output default prevents Zuno's generic provider layer from injecting
an unsupported 32,000-token cap. Declare each model's real `limit.output`
instead. Current `kiro-provider` preserves consecutive all-text blocks in its
canonical request and concatenates them byte-for-byte with no inserted separator
only at Kiro's scalar text boundary. Do not set
`responsesTextBlocks: "single"` for this provider version: that older Zuno
compatibility option inserts a blank line and therefore changes the request.
Mixed text and non-text blocks whose ordering cannot be represented still fail
closed at the provider boundary.

Current Zuno sends native Responses `instructions`, so the verified
functional deployment uses kiro-provider's explicit
`protocol_projection_mode: "legacy-user-prefix"` migration mode. The
provider's default `safe` mode correctly rejects that request rather than
silently rewriting it.

`legacy-user-prefix` changes instruction projection only; it does not select
Chat Completions. Keep Zuno on `surface: "responses"` and keep
kiro-provider's `enable_legacy_chat_completions: false`. The migration mode
joins instruction, system, and developer text into the first user content, so
role priority is no longer a security boundary. Tool authorization remains
owned by Zuno's permission and approval layers.

Do not set `reasoningSummary` on a Kiro model or Agent route.
`kiro-provider` has no proven lossless mapping for Responses
`reasoning.summary` and rejects it before contacting Kiro. Reasoning levels
remain available through the independent standard `reasoning.effort` field;
Zuno must not add a provider-id special case that silently drops summary.

For a foreground root or delegated-child turn, Zuno attaches the session's
durable identity to standard Responses
`metadata.zuno_session_id`. Tool continuations and a later process resume reuse
the same identity. Title, summary, compaction, reflection, and Council requests
remain explicitly isolated and do not receive foreground affinity.
`zuno_session_id` is reserved: provider `extraBody` or request parameters may
add unrelated object-shaped metadata but cannot override that field. A gateway
using `session_affinity_mode: "explicit-only"` can therefore bind the request
without private headers or model-visible prompt prefixes. See
[Kiro Provider Native Integration](../design/kiro-provider-native-integration.md)
for the ownership and verification contract.

Do not add `previous_response_id`, Responses `conversation`, `store: true`,
structured-output controls, native Web Search, or document payloads through
`extraBody`; remote image URLs are unsupported too. The gateway rejects
unsupported fields instead of silently weakening them. Inline data-URL images
and function tools are the rich-input subset currently emitted by Zuno. The
provider's native `input_file` support can be enabled in Zuno only after a
typed document block, capability validation, durable replay, and real E2E test
land together.

Run one long-lived kiro-provider process for the credential-owning OS user.
Do not spawn a provider per Zuno session: the process owns shared
authentication, refresh locking, persisted affinity, and account-scoped
transport pools.

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
sequence first. Keyboard or pointer interaction while the overlay remains active
restarts the five-second deadline; resolving or abandoning the sequence still closes it
immediately.

With `mouse` absent or `true`, Zuno captures button, drag, release, and wheel events. The transcript and both root and child composers provide their own selection and copy behavior. Releasing a drag copies the selected text through the configured clipboard and leaves the highlight visible. Transcript selection remains clamped instead of crossing into the sidebar; tool and sidebar disclosure rows are clickable, and an overflowing conversation mounts a draggable scrollbar.

Wheel input is precise at the start of a gesture: the default first notch moves one
row, then a sustained fast gesture accelerates. Setting `scroll_speed` chooses a
constant row multiplier. Setting `scroll_acceleration.enabled` to `true` explicitly
selects velocity acceleration and wins when both fields are present; setting it to
`false` keeps constant movement (one row when no speed is supplied). Root and attached
child transcripts use the same policy and keep independent offsets.

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

## Optional Agent step guard

An Agent has no fixed provider-step limit unless its definition sets `steps`:

```json
{
  "agents": {
    "orchestrator": {
      "steps": 200
    }
  }
}
```

The same field is available in Agent Markdown frontmatter. `steps` must be a
positive integer and counts tool-capable provider iterations within one turn.
Omit it for the default unbounded behavior.

When the configured iteration is exhausted and the model still requests
continuation, Zuno sends one additional text-only request. That request has no
tools and asks for a concise account of completed work, remaining work,
evidence, and blockers. It does not silently raise or reset the configured
limit.

## Agent capability ceilings and required Skills

An Agent definition may set an exact `tools` allowlist and may require instruction
sets by name:

```json
{
  "agents": {
    "explorer": {
      "requiredSkills": ["codegraph"]
    }
  }
}
```

`requiredSkills` is an array of non-empty, unique Skill names. Like other arrays,
a higher-precedence configuration layer replaces the lower array. The field does
not copy a Skill into configuration and does not grant tools.

For a delegated turn, the parent Attempt's actual provider-visible tool schemas
form an immutable upper bound. The target role, its MCP/extension inheritance
policy, the Agent's exact `tools` allowlist, and effective permission rules can
only narrow that set. A configured `allow`, including `permission.mode:
"allow_all"`, cannot restore an absent parent schema. A same-named tool with a
different provider-visible schema is also outside the bound.

MCP and extension tools are not automatically available to all read-only Agents.
The exact schema must have been visible in the parent Attempt, the target role
must either inherit extension tools automatically or carry an exact per-Agent
`permission.rules` grant, the Agent allowlist must retain the wire id, and no later
explicit permission rule may deny it. Unknown MCP tools remain side-effecting by
default; granting one audited query tool does not opt the Agent into every MCP tool.

Every initial or resumed child host performs Skill discovery independently.
Parent-loaded Skill bodies are not copied. After profile and Agent visibility
filtering, each `requiredSkills` name must resolve to exactly one source. Before each
provider-bound input, Zuno ensures the resolved body is present in the durable prompt
and de-duplicates an already loaded source. A missing name or an ambiguous same-name
source fails child startup instead of silently skipping the requirement.

Consequently, `"requiredSkills": ["codegraph"]` guarantees CodeGraph instructions,
not CodeGraph execution authority. CodeGraph MCP tools still have to survive the
parent Attempt ceiling, automatic role inheritance or an exact per-Agent grant, the
exact `tools` allowlist, and explicit permission rules.

This child-capability construction is informed by Codex's effective-parent-config
pattern, but it is Zuno's own contract. It does not make Codex configuration, MCP,
Skill, role, or wire behavior a compatibility target.

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

For direct `task` delegation with `agent`, model precedence is
`agents.<target>.model`, the active preset's Agent route, then the parent
session model. The model-facing tool does not accept `model`, `effort`, or
`category` overrides. Host-owned workflow and Council category routes use the
active preset's category route, then the parent session model; the `general`
Agent route is deliberately not consulted. Reasoning comes from the winning
route, then the model/provider default.

The top-level turn follows the same specificity principle for its selected
Agent. An unavailable route produces a visible diagnostic before falling
through; Zuno never ships hidden model-id defaults. The selected preset is
frozen with the turn plan, so editing configuration cannot mutate an in-flight
attempt.

For one headless invocation, `zuno run --variant <name>` overrides configured
reasoning with the exact model-declared variant. Canonical names
`off|low|medium|high|xhigh|max` are accepted only when the selected model
declares them, or when the model exposes generic reasoning without declaring a
named variant catalog. A non-canonical name copies that variant's complete
provider option object. A model that declares only custom names such as
`deliberate` does not silently acquire canonical variants. Unknown names fail
before HTTP I/O and list the available variants.

`zuno run --thinking` asks the host to select `high` when available, otherwise
the strongest declared non-`off` canonical level. It fails for a non-reasoning
model and for a named-only custom variant catalog whose semantics cannot be
inferred. `--thinking` and `--variant` are mutually exclusive. Prefer
`--variant max` or `--variant xhigh` when exact effort matters; `--thinking` is
intentionally an automatic convenience.

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
      "shell": "allow",
      "external_directory": "allow"
    }
  }
}
```

An explicit `deny` still wins in every mode. `allow_all` suppresses every Zuno
tool-approval prompt, including confirmable Shell-risk requests; it does not
bypass sandboxing, argument validation, explicit denies, or catastrophic Shell
targets that the risk gate rejects directly. Use `zuno debug permissions` to
inspect both the configured and effective mode, and `zuno debug config` to
inspect the merged configuration.

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

`shell` always requires strict approval in strict mode, even for a command such
as `rg`. Approval and confinement are independent: the native OS sandbox still
compiles the effective read-only or workspace-write policy after admission.
The explicit `danger-full-access` sandbox mode is the exception by design: it
sets the effective permission mode to `allow_all`, so an authored `strict` mode
remains visible in debug output but does not open approval prompts.

The top-level `shell` field chooses the actual interpreter for both terminal and
model-issued command execution. The command tool resolves an explicit value first,
then the operating-system account shell, inherited `SHELL`, and finally the platform
fallback. An invalid, non-executable, or command-syntax-unsupported explicit value
fails tool assembly; interactive PTYs may still use another executable login shell.
TUI and durable background state show the resolved command interpreter (`zsh`,
`pwsh`, and so on), not a fixed tool-id label.

Interactive PTYs accept any executable login shell. Model-issued commands are narrower:
Zuno currently has invocation and risk-analysis semantics for POSIX shells and PowerShell
only. `fish`, `nu`, unknown interpreters, and native Windows `cmd.exe` are rejected rather
than being analyzed as Bash. On Windows the command resolver tries `pwsh`, PowerShell,
and Git Bash; a host with only `cmd.exe` has no model command shell until a native `cmd`
parser and risk gate exist.

The top-level `sandbox` object sets the maximum authority for model-issued Shell
commands. The default is `workspace-write`:

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "writableRoots": ["../shared-cache"],
    "protectedPaths": [".zuno", ".agents", "secrets"]
  }
}
```

The exact modes are:

- `read-only`: the host filesystem is read-only. Private temporary storage may
  still be used, but no host writable root is accepted.
- `workspace-write`: the host root is read-only while the active workspace and
  explicitly trusted `writableRoots` are writable. This is the default.
- `danger-full-access`: run the configured shell directly as the Zuno user, with
  host filesystem, process, credential, and network access. It also sets the
  effective permission mode to `allow_all`, suppressing Zuno tool-approval
  prompts. This mode is explicit and is never selected when a confined backend
  fails.

An Agent's own capability contract may only narrow that configured maximum. A
read-only Agent therefore receives `read-only` even when the invocation selected
`workspace-write` or `danger-full-access`.

In confined modes, `network` defaults to `deny`; set it to `allow` only from a
trusted layer when model-initiated commands genuinely need host networking.
`danger-full-access` always inherits host networking, so combining it with
`network: "deny"` is invalid. It also rejects `writableRoots` and
`protectedPaths`, because claiming to enforce either would be misleading.

Relative paths resolve from the active workspace. `writableRoots` entries must
already be directories and are considered only in `workspace-write`.
`protectedPaths` must exist, may not be symbolic links, and are reapplied
read-only after writable mounts. Zuno protects existing `.git`, `.zuno`,
`.agents`, resolved external Git metadata, and its sandbox helper; configuration
can add protections but cannot disable confinement.

Sandbox authority follows configuration provenance. Trusted global, explicit
config, managed, environment, and CLI layers may select any mode. Project
`zuno.json[c]` and `.zuno` layers may only narrow to `read-only`, deny networking,
or add protected paths; they cannot select a wider mode, grant host networking,
or add external writable roots. Use a trusted one-invocation override when
needed:

```sh
zuno --sandbox read-only
zuno --sandbox workspace-write
zuno --sandbox danger-full-access
```

Managed policy has later precedence and may still narrow that override.

On Linux, confined Shell registration requires a trusted system bubblewrap plus
successful user, mount, PID, UTS, IPC, seccomp, and—when `network` is
`deny`—network namespace probes. A failed probe stops tool assembly; Zuno never
falls back to raw host execution. Confined macOS and Windows backends are not yet
implemented and fail closed, while an explicit `danger-full-access` invocation
uses the native process backend on all supported platforms. See the
[sandbox FAQ](../faq.md) for the security boundary, Ubuntu AppArmor setup, and
nested-sandbox diagnosis.

The Shell risk gate distinguishes confirmable risk from catastrophic denial.
Bounded destructive operations, dynamic targets, and replacing an existing
redirect target request fresh approval unless the effective permission mode is
`allow_all`; under `allow_all` they proceed without a prompt. Catastrophic
targets are rejected directly in every mode. New static files under the working
directory or OS temporary directory are treated as creation. An exact,
non-recursive `rm -f` of a statically named path that is currently absent below
the OS temporary directory is treated as no-op cleanup. There is no tool
argument that lets a model approve its own risky call, and an explicit
permission deny always wins.

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
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000
  }
}
```

`includeInstructions: false` removes both the trigger policy and catalog from
model prompts. The `skill` tool still supports paged `list` and `search`
discovery. `load` and `read_resource` return content-bound continuation cursors;
the caller must read through `complete: true` before applying the instructions.

`maxContextTokens` applies only to the compact catalog. Fully selected Skill
bodies share a separate aggregate prompt budget. Its default is ten percent of
a known model context, with a 2,000-token floor and a 32,000-token ceiling; an
unknown context uses 8,000 approximate tokens. `maxSelectedContextTokens`
overrides the derived value but remains capped at 32,000 tokens. If one or more
selected bodies do not fit, loading or restoring the session fails before a
provider request rather than silently dropping instructions.

Use `zuno debug skill` after restarting to inspect raw discovery. Its object
explicitly reports `view.kind: "raw_discovery"`,
`agentFiltered: false`, and `extensionOverlayApplied: false`; the `skills` array
preserves same-name entries from different sources, while `summary` reports
source/described/unique counts and ambiguous names. Use
`zuno debug agent <name>` for the effective Agent-filtered view, including
metadata and selected-body budgets, rendered/omitted/truncated coverage, and a
bounded 50-entry preview. A generic prompt such
as "follow skill guidance" does not select every skill; a skill is loaded only
when its name is explicit or its description clearly matches the request.

Child turns run this discovery independently. Loading a Skill in the parent does
not inject its body into a delegated child. Use `agents.<name>.requiredSkills`
when a child role must receive a particular instruction set on every initial or
continued turn. Required names resolve only after profile and Agent visibility
filtering and must identify one source; missing and ambiguous names fail child
startup.

Zuno also compiles nine original first-party Skills into the
`zuno-orchestration` pack: `customize-zuno`, `develop-zuno`, `deepwork`, `codemap`,
`verification-planning`, `reflect`, `worktree`, `git-workflow`, and `ui-design`.
Each has a stable
`builtin://zuno-orchestration/...` source, content hash, provenance, allowed
Agent profiles, and required-tool declaration. The active profile and its
declared tool visibility filter the advertised set; selecting a Skill can never
widen the runtime capability snapshot.

These resources are compiled into the executable and published into the Skill
catalog when the first-party profile mounts; Zuno does not copy them into the
user configuration directory. `develop-zuno` helps choose among configuration,
Agent Markdown, a user-owned Skill or command, an `extension.json` package, and
a native Rust extension point. It grants no tools or authority.

An unambiguous Skill that does not collide with a real command is directly
invokable as `/<skill-name>`. Zuno resolves that exact advertised source and
loads its body before the next provider request. Same-named Skills from multiple
sources deliberately disable the ambiguous direct slash form; use the Skill
picker or the typed `skill` tool with an exact source instead. The `worktree`
Skill guides explicit, user-authorized `git worktree` creation, isolated work,
integration, and conservative cleanup with dirty-state and reachability checks.
It does not create worktrees by itself, grant Shell authority, or provide
product-owned leases, quotas, or automatic cleanup.

## Reusable workflows: Skills and Markdown commands

Use a Skill for reusable guidance, implicit trigger matching, scripts, references,
assets, or a workflow that must be fully loaded before action. A unique Skill is
already available explicitly as `/<skill-name>`, so a second command file is not
needed merely to add a slash entry.

Product- and organization-specific workflows remain user owned. For example,
define `dual-review` or `auto-release` under a global
`~/.config/zuno/skills/<name>/SKILL.md` or project `.zuno/skills/<name>/SKILL.md`
when that policy is wanted. Zuno supplies the discovery, source identity,
permission filtering, resource loading, and direct slash entry; it does not ship
either policy body or assume a reviewer, release process, remote authority, gate
set, or publication target. The generic built-in `balanced-review` council is only
a reusable multi-seat synthesis primitive; it is not either named workflow.

Use a Markdown command only for a literal prompt template or argument macro. Zuno
loads `command/**/*.md` and `commands/**/*.md` recursively from its global config
directory and every project `.zuno` config directory. Frontmatter supplies command
metadata; the body supports `$ARGUMENTS` and positional parameters. Commands never
grant tools or bypass Agent permissions, and a real command wins a same-name Skill
collision. Built-in `/init` and `/init-deep` remain native commands because the
generic command host can execute their templates. Zuno does not register a
built-in `/review`; a user may provide a `review` Skill or command with the exact
semantics their project needs.

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
