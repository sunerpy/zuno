# Build-agent prompt and request comparison

Status: current implementation plus a pinned historical capture, 2026-08-24.

The first section describes the current structured-prompt implementation. The
later capture answers a separate historical question: what did an older Zuno
build present to the model when the `build` agent received this user request?

> 按照skill指导 对本项目进行完整的优化设计，即将发布到github公开仓库了

It then compares that request architecture with OpenAI Codex and
oh-my-openagent (OMO). The comparison is pinned to source revisions:

| project | revision used |
| --- | --- |
| Zuno | `c1ae031137c940f5e32841c42cf54e6ea5e0c583` |
| Codex | `a8468330bb5f45e9f4d2ec630b01ea8c52908be3` (`origin/main`) |
| oh-my-openagent | `9c30f2e2011ab52443859bd3cada95ae3ccf978d` (`upstream/dev`) |

The checked-out OMO branch was
`44c95e976dfd13b911de7988872fc2302f2b1092`; its agent prompt files were
byte-identical to the pinned `upstream/dev` revision.

## Evidence levels

- **Exact historical evidence** comes from durable rows in
  `/config/.local/share/zuno/zuno-local.db`.
- **Source reconstruction** follows Zuno at the pinned commit.
- **Unavailable historical bytes** were neither persisted nor captured on the
  wire and must not be presented as exact.

The public, redacted evidence manifest is
[build-agent-public-release-request.redacted.json](fixtures/build-agent-public-release-request.redacted.json).
The complete 55,314-byte local export is intentionally gitignored at
`.omo/evidence/build-agent-public-release-prompt.raw.json`. It includes the
machine-level instruction path and complete installed-skill inventory, so it
must not be published with the repository.

## Current implementation after the upgrade

Zuno now carries prompt provenance in a provider-neutral `PromptEnvelope`
instead of treating the model prompt as one anonymous system string. It groups
ordered `PromptBlock` values into:

- kernel and agent role;
- typed collaboration mode;
- runtime policy;
- global and project instructions;
- work state and routing;
- selected Skill bodies and the bounded Skill index;
- memory.

Each block retains its stable id, source, semantic role, trust class, priority,
SHA-256 digest, byte count, local token estimate, and model-visible content in
the durable prompt receipt. The exact user message remains a normal user
message rather than being copied into an instruction block.

### Provider mapping

The provider-neutral mapping is:

| semantic content | OpenAI Responses representation |
| --- | --- |
| Kernel + selected agent role | top-level `instructions` |
| Collaboration mode (`Plan` or `Work`) | independent `developer` input |
| Runtime policy | independent `developer` input |
| Global `AGENTS.md` | independent `developer` input |
| Project `AGENTS.md` chain | one independent `developer` input per source |
| Goal, plan, todo, routing, and memory context | independent dynamic `developer` inputs |
| Selected Skill body | independent `developer` input |
| Bounded Skill index | independent `developer` input |
| User message | original `user` input, unchanged |
| Tool definitions | top-level `tools`, outside prompt text |

Chat-compatible, Anthropic, Bedrock, and Google adapters consume the same typed
`developer_context` boundary and map it to their native developer/system
representation. They do not receive a newly flattened source-less prompt.

The current Responses request therefore has this shape:

```jsonc
{
  "model": "<configured model>",
  "instructions": "<kernel + agent role>",
  "input": [
    {"role": "developer", "content": "<collaboration mode>"},
    {"role": "developer", "content": "<runtime policy>"},
    {"role": "developer", "content": "<global AGENTS.md>"},
    {"role": "developer", "content": "<project AGENTS.md>"},
    {"role": "developer", "content": "<selected Skill or Skill index>"},
    {
      "role": "user",
      "content": [{"type": "input_text", "text": "<exact user input>"}]
    },
    {"role": "developer", "content": "<dynamic Goal or memory context>"}
  ],
  "tools": ["<runtime ToolManifest schemas>"],
  "stream": true
}
```

Empty or disabled blocks are omitted. Configured reasoning, output, storage,
and include options remain provider fields rather than prompt prose. Prompt
receipt schema version 2 records `collaboration.mode` as a distinct runtime-trust
block instead of merging it into an agent role or user message.

### Current Plan and Work behavior

- `/plan` is the interactive boundary. From Work mode it opens a keyboard- and
  mouse-operable confirmation before selecting the read-only `plan` agent. From
  Plan mode it reviews the durable plan and offers the Start Work confirmation.
- `/start-plan` enters Plan mode directly. `/start-work` refuses to proceed
  without a durable plan and still asks the user to confirm that exact plan and
  revision before selecting `build`.
- The Plan agent uses a deny-by-default capability overlay. It may inspect with
  read, glob, grep, LSP, and web tools; ask questions; load Skills; and update
  typed goal, plan, and todo state. It cannot use shell or file-mutation tools.
- The selected collaboration agent is stored with the session. Explicitly
  reopening or switching to an existing session restores its Plan or Work mode
  without restarting the TUI.
- The model may recommend `/start-work`, but it cannot switch collaboration
  modes on the user's behalf. Durable Goal, Plan, Todo, Job, and queue state is
  authoritative; prose checklists are not execution state.

### Current Skill behavior

- Every visible, uniquely named Skill that does not collide with a real command
  is exposed as `/<skill-name>` in the slash catalog. For example,
  `/github-project-scaffold` loads that exact advertised source without a shell
  search.
- A bare `/<skill-name>` loads the complete Skill and publishes its loaded state
  without creating a model turn. `/<skill-name> <arguments>` loads it first and
  then sends the canonical slash request as the unchanged user input.
- Real commands retain precedence. Duplicate same-named Skill sources are not
  guessed or exposed as one slash command; they remain available through
  `/skills` and source-qualified `skill` operations.
- An explicitly named, uniquely resolved Skill is fully loaded by the host
  before the first provider request. This also works when the name is adjacent
  to Chinese text.
- Ambiguous same-named Skills and identifier substrings are not guessed.
- Loaded Skill identity is persisted in the prompt receipt and restored when a
  session resumes.
- The initial catalog contains metadata only. Its default budget is two percent
  of the model context, falls back to 8,000 characters when the context is
  unknown, and is capped at 10,000 tokens.
- Descriptions are shortened before source identities are omitted. Every
  omitted entry remains available through `skill search` or paged `skill list`.

This deliberately avoids a fixed count cap: a large installation retains
searchable coverage without recreating the earlier “skills did not fit” failure.

### Current AGENTS precedence

Zuno loads only native instruction locations implicitly:

1. `$XDG_CONFIG_HOME/zuno/AGENTS.md`;
2. project directories from the worktree root to the current directory;
3. in one directory, `AGENTS.local.md` replaces `AGENTS.md`;
4. nearer project files are appended later and therefore have higher priority.

`CLAUDE.md`, OpenCode directories, and other products' global instruction files
are never implicit fallbacks. Explicit `instructions[]` entries remain
available when compatibility is intentional.

### Prompt diagnostics

```sh
zuno debug prompt
zuno debug prompt <session>
zuno debug prompt <session> <turn>
zuno debug prompt <session> <turn> --show-sensitive
```

The command defaults to the latest durable prompt receipt and redacts
model-visible bodies. `--show-sensitive` reveals instruction, AGENTS, Skill,
memory, and post-hook prompt content, so its output must be handled as a secret.

One audit gap remains: the receipt records the structured prompt and post-hook
system content, but it does not yet persist a complete redacted manifest of the
final provider HTTP body and tool-schema wire order.

## Historical observed Zuno turn

The rest of this section is evidence from the pinned pre-upgrade session. It is
not a description of the current request builder.

| field | exact value |
| --- | --- |
| session | `ses_7c58818c66c84dd09eca6131d3128cb4` |
| title | `项目优化设计准备GitHub发布` |
| agent | `build` |
| provider | `myopenai` |
| model | `us.anthropic.claude-opus-5` |
| user message | `msg_d0683207e6a746e1b54b30f1de18283a` |
| prompt event | sequence `2`, `session.prompt.assembled.1` |
| prompt SHA-256 | `a82de30c12a5e6cf2cbf93b90287ef2e202bdb1cdd608bd53bcbaf7583c2e1fa` |
| hook transformed | `false` |

The first model step had no goal row and no `session_context_epoch`, so no
trailing dynamic `<system-reminder>` was added. Before model output, the
engine-visible history was one assembled `system` message followed by the exact
`user` message above. Tools were advertised separately, but their exact
historical JSON array was not persisted.

### Exact system assembly

`PromptAssembly::render` sorts by `order` and joins section contents with exactly
two LF bytes (`\n\n`). The resulting system string was 49,419 bytes.

| order | id | source | bytes | SHA-256 |
| ---: | --- | --- | ---: | --- |
| 0 | `agent.base` | `native:build` | 1,805 | `d30804c168a45672deb96dd45abaecee959936f080548614683b9db71bbc7fc2` |
| 1 | `agent.policy` | `zuno-agent::builtin:build` | 285 | `679e07d9922fc9fdbac77a2a37390bb0b6b14962240553a1e86588df8e696d48` |
| 2 | `extensions` | `zuno-extension::active-packages` | 435 | `dcc2a562a26c24e9503af47cd474a74fbb376f7d520ec66588e74f748e5b1c53` |
| 3 | `instructions.0` | user-level `CLAUDE.md` | 869 | `48379a2649c35bb693a046b226c71115b6624e221c626e2717bd07496236613e` |
| 4 | `instructions.1` | repository `AGENTS.md` | 5,070 | `5917a0e8d78419e73a38ac8ed2b8d7284bbe37fb51e0820ca7521acd24c775ca` |
| 5 | `skills.policy` | `zuno skill trigger policy` | 954 | `4a0403666ea19903c98bcaed998d5da79d18444f84d73ff0af78cab594a56cab` |
| 6 | `skills.index` | discovered skill index | 39,989 | `04a2df6fef4d3cc945324d5a8e1fc25791f2537cbd0a4afbb60404d22e6f84f1` |

The 12 separator bytes account for the difference between the section sum and
49,419 bytes. Two details matter:

- Zuno inherited a user-level `CLAUDE.md` CodeGraph instruction. That is
  cross-product configuration, not native Zuno guidance.
- `skills.index` consumed 39,989 of 49,419 bytes (about 81%). All identities
  remained visible, but 120 descriptions were shortened.

### Exact native `build` prompt

```text
You are Zuno's primary delivery agent. Own the requested outcome from understanding through verified completion.

Match the user's intent and authority. Answer, review, or diagnose without changing state unless a change is requested. For implementation, inspect the live repository, applicable instructions, user changes, and failure evidence. Fix the owning abstraction, preserve unrelated work, and prefer the smallest coherent design.

Use tools only for authoritative evidence or required work. A self-contained reasoning or writing question should be answered directly; do not call the shell or create throwaway files merely to think. Keep a plan for dependent work and update it as evidence changes. Treat a lost response around a side effect as uncertain and inspect state before retrying.

Prefer native read and search tools for repository evidence. For source changes, use apply_patch for localized or multi-file edits. Use write only for a new file or a full replacement after reading the current file, and verify the content before continuing.

Delegate only independent, bounded work with an explicit deliverable and scope boundary. Do not duplicate delegated discovery. Keep integration decisions, affected-caller review, and final verification in this agent.

Do not declare completion from intent, a plausible patch, or a narrow green test. Audit every explicit requirement against current evidence and verify the surface plus important interruption and recovery paths. Continue an active goal until completion is proven or the runtime records a real pause, block, limit, or permanent failure.

Communicate briefly. In the final response, lead with the outcome, name material changes and completed checks, and state remaining limitations plainly. Use natural Markdown, not harness markup.
```

The exact policy appended immediately after it was:

```text
**Don't delegate when:** the whole task is smaller than the briefing it would take • you already have the file path and need its contents • the answer is in this conversation • explaining the task costs more than doing it • the work is one edit you are already mid-way through.
```

The repository instruction body already lives in [`AGENTS.md`](../../AGENTS.md).
The historical copy and complete `skills.index` remain in the local raw artifact
rather than duplicating a source file or publishing workstation inventory.

### Exact user input

```json
{
  "role": "user",
  "agent": "build",
  "model": {
    "providerID": "myopenai",
    "modelID": "us.anthropic.claude-opus-5"
  },
  "parts": [
    {
      "type": "text",
      "text": "按照skill指导 对本项目进行完整的优化设计，即将发布到github公开仓库了"
    }
  ]
}
```

It entered `session_input` with delivery `next-step`, admitted sequence `0`, and
promoted sequence `1`.

### What the model did first

This run does not reproduce the earlier failure where an agent searched the
filesystem for a named skill. The first completed tool call was:

```json
{
  "tool": "skill",
  "input": {
    "action": "load",
    "intent": "Load public-release scaffolding skill",
    "name": "github-project-scaffold",
    "source": "/config/.config/zuno/skill/github-project-scaffold/SKILL.md"
  }
}
```

Only after it completed did the model invoke `bash`. The model used the source
advertised in `skills.index` directly.

## Historical source-reconstructed Responses request

For this provider, `surface: "responses"` uses the OpenAI Responses wire shape.
The first request can be reconstructed at the engine/provider boundary as:

```jsonc
{
  "model": "us.anthropic.claude-opus-5",
  "input": [
    {
      "role": "system",
      "content": "<the exact 49,419-byte joined system prompt>"
    },
    {
      "role": "user",
      "content": [
        {
          "type": "input_text",
          "text": "按照skill指导 对本项目进行完整的优化设计，即将发布到github公开仓库了"
        }
      ]
    }
  ],
  "tools": "<runtime registry; exact historical JSON was not persisted>",
  "stream": true
}
```

The implementation may also add configured `store`, `include`, `reasoning`,
`text`, and output-limit fields, then apply request parameters. Those fields and
the final tool-schema order are source reconstruction, not an exact historical
HTTP-body capture. This is the main audit gap: `session.prompt.assembled.1`
proves the exact system text but not the complete post-preparation request.

## Codex comparison

Codex keeps stronger role and type boundaries:

- `Prompt.base_instructions` maps to the Responses API `instructions` field.
- Conversation and runtime context remain separate `ResponseItem` values.
- Generic policy uses developer-role fragments.
- Project `AGENTS.md` guidance is a marked user-role contextual fragment.
- The skill catalog is a developer-role `<skills_instructions>` block.
- A selected skill body is a user-role `<skill>` fragment.
- Tool specifications are serialized independently from prompt text.

For Responses Lite, Codex moves base instructions into a separate developer
message and places additional tool definitions in `input`; the ordinary
Responses path uses top-level `instructions` and `tools`.

Codex discovers guidance by reading global `AGENTS.override.md` or `AGENTS.md`,
then walking from project root to cwd and choosing at most one override,
standard, or configured fallback file per directory. It concatenates root to
leaf within a default 32 KiB budget. See
[How Codex discovers guidance](https://learn.chatgpt.com/docs/agent-configuration/agents-md#how-codex-discovers-guidance).

Structured skill selections and text mentions are resolved separately. Plain
names only match when unambiguous; canonical and logical discovery paths are
supported. Catalog advertisement and full-body loading are distinct operations.

The important difference is not simply prompt length: base policy, developer
context, user-scoped project guidance, selected skills, tools, and the actual
user message remain distinguishable typed items.

## DSH and OpenCode design influence

DSH contributes the durable control-plane rule rather than another monolithic
prompt: Goal, Plan, Todo, Job, background execution, and queue state are typed
runtime records, and prompt blocks are projections of that state. A model's prose
cannot complete a work item, change a mode, or erase an uncertain side effect.
This keeps restart, cancellation, delegation, and client rendering independent
from model wording.

OpenCode contributes the discoverable command-and-Skill interaction: slash
entries are catalog data, real commands win collisions, and a selected Skill is
loaded before it is used. Zuno keeps those interactions typed through the TUI,
durable queue, and runtime host rather than expanding them into an anonymous
prompt string.

## oh-my-openagent comparison

OMO's Sisyphus takes a model-family prompt approach:

- `createSisyphusAgent` selects an entire prompt family from the runtime model.
- Claude Opus 5 gets a dedicated long prompt covering model behavior counters,
  intent classification, exploration, delegation, implementation, recovery,
  verification, and completion.
- Available agents, tools, categories, and skill names are rendered dynamically.
- User/project skills are labeled higher priority than built-in plugin skills.
- Full descriptions remain behind `skill`; delegated work receives selected
  skills through `task(load_skills=[...])`.
- `experimental.chat.system.transform` rebuilds and replaces the complete
  Sisyphus prompt when the TUI runtime model belongs to another family.

OMO owns a large plugin-supplied system component, not the complete provider
request. OpenCode still owns other system text, user messages, tool
serialization, and the final wire body. An exact OMO request therefore depends
on runtime OpenCode state, model, catalog, overrides, environment, and hooks.

For this request, OMO's Claude Opus 5 prompt would explicitly classify the
open-ended release-readiness intent, require assessment before changes, load a
loosely matching skill immediately, and apply category/delegation rules. Zuno
reached the same relevant skill with a smaller native contract plus a much
larger generic catalog.

## Architecture differences

| concern | Zuno historical capture | Codex | oh-my-openagent |
| --- | --- | --- | --- |
| Base behavior | concise native `build` text | model/session base instructions | long model-family Sisyphus prompt |
| Role split | static sections flattened into one `system` | top-level instructions plus typed developer/user items | plugin replaces system text; OpenCode owns the rest |
| Project guidance | copied into joined system | marked user context from an `AGENTS.md` chain | OpenCode guidance plus OMO additions |
| Skill catalog | full XML-like index in system | developer catalog with aliases and budget | compact names/source labels in Sisyphus |
| Selected body | `skill` tool result enters history | user `<skill>` context | `skill`, then delegation `load_skills` |
| Name collision | advertised exact source passed to tool | plain names must be unambiguous; paths supported | OMO/OpenCode registry owns resolution |
| Model specialization | shared concise contract | model base plus capability context | whole prompt rebuilt per model family |
| Dynamic state | optional trailing reminder | typed context/world-state fragments | reminder and continuation hooks |
| Tool schemas | separate, but not historically persisted | separate typed `ToolSpec` serialization | OpenCode registry plus OMO tools |
| Audit | exact system sections and hashes | prompt-debug/input plus rollout state | plugin prompt inspectable; complete wire outside OMO |
| Main weakness | role collapse and missing final manifest | many contextual item types | prompt inflation and duplicated model prose |

The current Zuno implementation no longer matches the historical Zuno column:
it has closed role collapse, cross-product instruction fallback, collaboration
mode ambiguity, and named-Skill preload gaps. Its production contract combines
Codex-like typed request boundaries, OpenCode-like discoverable Skill commands,
DSH-like durable work-state authority, and concise OMO-inspired role guidance.
The final provider-manifest gap remains.

## Historical assessment and current disposition

Keep the concise native `build` contract, ordered section IDs, byte counts,
hashes, and explicit skill loading. Do not copy OMO's complete prompt wholesale.

Disposition of the historical recommendations:

1. **Remaining:** persist a redacted final request manifest after provider
   preparation: ordered roles, content hashes/bytes, tool names and schema hashes
   in wire order, reasoning/output settings, body hash, and hook digest. Never
   persist secrets, authorization headers, signed URLs, or binary payloads.
2. **Completed:** preserve base instructions, developer policy, project
   guidance, selected Skill bodies, and the actual user message as typed items
   until provider encoding instead of flattening them all into `system`.
3. **Completed:** use a Zuno-owned global `AGENTS.md`; another product's
   instruction file requires an explicit `instructions[]` entry.
4. **Completed:** reduce initial catalog weight while keeping source identities
   and complete search/list escalation.
5. **Completed with native syntax:** `zuno debug prompt [session] [turn]`
   exposes the durable receipt and defaults to redaction.
6. **Completed as an architectural rule:** model-aware corrections remain small
   capability fragments rather than duplicated full prompt families.
7. **Completed:** Plan and Work are first-class collaboration blocks and
   user-authorized session modes, not prose conventions hidden inside one agent
   prompt.
8. **Completed:** unique, non-colliding Skills are directly invocable as slash
   commands; bare invocation loads only, while arguments start a typed turn.

The historical matching-Skill path worked in that run. Current host-side preload
is additionally covered by exact-name, ambiguity, Chinese-boundary, direct-slash,
and resume regression tests.

## Source map

Zuno:

- `crates/zuno-engine/src/prompt.rs`
- `crates/zuno-engine/src/loop.rs`
- `crates/zuno-cli/src/cmd/turn.rs`
- `crates/zuno-provider-openai/src/request.rs`

Codex:

- `codex-rs/core/src/client_common.rs`
- `codex-rs/core/src/client.rs`
- `codex-rs/core/src/agents_md.rs`
- `codex-rs/core/src/context/user_instructions.rs`
- `codex-rs/ext/skills/src/catalog_prompt.rs`
- `codex-rs/ext/skills/src/fragments.rs`
- `codex-rs/ext/skills/src/host_prompt.rs`
- `codex-rs/skills/src/selection.rs`

oh-my-openagent:

- `packages/omo-opencode/src/agents/sisyphus-agent-factory.ts`
- `packages/omo-opencode/src/agents/sisyphus/claude-opus-5.ts`
- `packages/omo-opencode/src/agents/dynamic-agent-category-skills-guide.ts`
- `packages/omo-opencode/src/agents/builtin-agents/sisyphus-agent.ts`
- `packages/omo-opencode/src/agents/sisyphus-runtime-prompt-reconciler.ts`
- `packages/omo-opencode/src/plugin/system-transform.ts`
