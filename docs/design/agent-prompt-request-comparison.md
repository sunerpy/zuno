# Agent prompt and request comparison

Status: implemented design audit for Prompt and Workflow Generation 2,
2026-08-28; real-provider E2E status is recorded separately.

This document has three deliberately separate evidence layers:

1. the current Zuno implementation at the audited repository revision;
2. the implemented Generation 2 contract and its remaining external validation;
3. a pinned historical capture of what an older Zuno `build` turn presented to
   the model.

The detailed Chinese audit and implementation contract is
[prompt-workflow-v2.zh-CN.md](prompt-workflow-v2.zh-CN.md).

## Pinned design sources

The Generation 2 audit uses immutable public revisions:

| project | revision used | public source |
| --- | --- | --- |
| Zuno | `b1429cd54f38475f6f1ff74f7adb2fe6dcb586ec` plus the Generation 2 implementation worktree | this repository |
| Codex | `a73bf25d17805b4169ba2a2dc4329a010a3bb120` | [commit](https://github.com/openai/codex/commit/a73bf25d17805b4169ba2a2dc4329a010a3bb120) |
| OpenCode | `1be9fd55a9326d5e7b09786195e5669e311e61b4` | [commit](https://github.com/anomalyco/opencode/commit/1be9fd55a9326d5e7b09786195e5669e311e61b4) |
| oh-my-opencode-slim | `9dbf2de015aec093e44273e6411c1392705b2f4d` | [commit](https://github.com/alvinunreal/oh-my-opencode-slim/commit/9dbf2de015aec093e44273e6411c1392705b2f4d) |
| oh-my-openagent | `64d89819ef1fde81712630f8e5d798be9e4e8867` | [commit](https://github.com/code-yeongyu/oh-my-openagent/commit/64d89819ef1fde81712630f8e5d798be9e4e8867) |

The upstream repositories are design evidence, not compatibility targets. Zuno
does not copy their hidden identity, product branding, complete role prompts, or
project-specific workflows.

The later historical capture answers a separate question: what did an older
Zuno build present to the model when the `build` agent received this user
request?

> 按照skill指导 对本项目进行完整的优化设计，即将发布到github公开仓库了

That historical reconstruction remains pinned to these older source revisions:

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

## Current implementation and Generation 2 boundary

Zuno already carries prompt provenance in a provider-neutral `PromptEnvelope`
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

The Generation 2 worktree introduces six stable runtime policy identifiers:

- `runtime.intent`;
- `runtime.execution`;
- `runtime.editing`;
- `runtime.verification`;
- `runtime.delegation`;
- `runtime.persistence`.

The Generation 2 implementation now renders these sections after the registry tool
snapshot is locked and after `prepare_request` has reduced it to the final
provider-visible subset. The sections are carried as volatile developer
contexts, so capability changes do not mutate the cacheable static role and
instruction prefix. The durable receipt records both the typed section
provenance and exact pre-/post-hook system/developer projections.

`PromptAssembly::push` rejects duplicate ids and performs a stable canonical
semantic sort. `render` joins that stored canonical vector. Equal-lane sections
retain producer order, while runtime sections inserted at the provider boundary
still sort ahead of user instructions, work state, routing, Skills, and memory.

### Provider mapping

The provider-neutral mapping is:

| semantic content | OpenAI Responses representation |
| --- | --- |
| Kernel + selected agent role | top-level `instructions` |
| Collaboration mode (`Plan` or `Work`) | independent `developer` input |
| Enabled runtime policy section | one independent `developer` input per stable section id |
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
    {"role": "developer", "content": "<runtime.intent>"},
    {"role": "developer", "content": "<runtime.execution>"},
    {"role": "developer", "content": "<runtime.editing when applicable>"},
    {"role": "developer", "content": "<runtime.verification>"},
    {"role": "developer", "content": "<runtime.delegation when applicable>"},
    {"role": "developer", "content": "<runtime.persistence when applicable>"},
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
receipt schema version 3 records `collaboration.mode` and each runtime policy
section as distinct typed blocks, together with the actual provider projection
and post-hook digest.

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
2. `ZUNO_CONFIG_DIR/AGENTS.md`, when that profile supplies one;
3. project directories from the worktree root to the current directory;
4. in one directory, `AGENTS.local.md` replaces `AGENTS.md`;
5. nearer project files are appended later and therefore have higher priority.

The profile layer never hides the base global file. On the first ordinary
configuration discovery, Zuno materializes an original starter global
`AGENTS.md` only when the file is absent; existing user content is never
rewritten. The starter keeps durable cross-project rules concise and delegates
detailed Git and worktree procedure to the built-in `git-workflow` and
`worktree` Skills.

`CLAUDE.md`, OpenCode directories, and other products' global instruction files
are never implicit fallbacks. Explicit `instructions[]` entries remain
available when compatibility is intentional.

### Prompt diagnostics

The CLI accepts:

```sh
zuno debug prompt
zuno debug prompt --session <session>
zuno debug prompt --session <session> --step <non-zero>
zuno debug prompt --show-sensitive
zuno debug agent <name>
zuno debug sandbox --mode workspace-write --network deny --check
```

Model-visible bodies are redacted unless `--show-sensitive` is present.
`debug prompt` resolves a requested step through
`session.provider.request.1.promptReceiptID` and then loads that exact durable
receipt. The trace map stores `projection digest → receipt event id`, so prompt
sequence A → B → A correctly reuses and references A's receipt.

`debug agent` uses the same read-only config, extension, model, reasoning,
permission, Skill, delegation, parent-authority, and sandbox-policy resolution
as a real `TurnPlan`. It does not create a session, start a provider, or reveal
credentials. It does reuse the formal MCP runtime: enabled servers are connected,
their exact provider-visible schemas are checked against role policy, allowlists,
and parent schema authority, and every transport is closed before return.
Historical final tool schemas must still be read from the matching provider
request/receipt rather than inferred from current configuration.

The receipt also does not yet persist a complete redacted manifest of the final
provider HTTP body and tool-schema wire order.

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

At the historical revision, `PromptAssembly::render` preserved insertion order
and joined section contents with exactly two LF bytes (`\n\n`). The `order`
values below were assigned from that stored order when the durable receipt was
emitted; that historical render did not sort them. The resulting system string
was 49,419 bytes. Current Generation 2 code instead applies the stable
canonical semantic sort described above.

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

Only after it completed did the model invoke `shell`. The model used the source
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

Codex's documented customization surface includes a user-maintained global
`AGENTS.override.md` or `AGENTS.md`, followed by project guidance from root to
cwd. Its public worktree documentation describes product-managed isolated
checkouts, while reusable procedures belong in Skills. These public contracts
are design inputs; Codex's private product/system instructions are neither a
portable global file nor copied into Zuno. See
[AGENTS.md guidance](https://developers.openai.com/codex/guides/agents-md),
[customization](https://developers.openai.com/codex/concepts/customization/),
and [worktrees](https://developers.openai.com/codex/app/worktrees/).

Structured skill selections and text mentions are resolved separately. Plain
names only match when unambiguous; canonical and logical discovery paths are
supported. Catalog advertisement and full-body loading are distinct operations.

The important difference is not simply prompt length: base policy, developer
context, user-scoped project guidance, selected skills, tools, and the actual
user message remain distinguishable typed items.

## OpenCode comparison

OpenCode's public session layer makes commands, Skills, MCP instructions, model
environment, attachments, compaction, and tool discovery visible from one
runtime. Its current `SystemPrompt.provider` also selects complete prompt files
from model identifiers, while Skill and MCP text are assembled as strings after
permission filtering.

Zuno adopts capability discoverability and explicit compaction recovery. It
adapts those features into native components, typed prompt sections, durable
events, and a locked effective capability snapshot. It rejects model-name
switching of the complete role prompt and does not pursue OpenCode wire,
configuration, hook, or database compatibility.

## oh-my-opencode-slim comparison

Slim makes specialist responsibilities, task rejection, task status, wake
gates, and result coordination explicit. That is useful evidence for bounded
delegation and parent/child ownership. Its dedicated `designer` role and
hook-managed task-session board remain product choices, not defaults Zuno needs
to copy.

Zuno adopts the explicit task contract and role boundaries. It adapts result
coordination to SQLite-backed jobs, inbox admission, and one durable wake rather
than plugin hooks or polling. It rejects a default built-in Designer route:
`ui-design` remains a Skill that a configured Agent may load.

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

## Adopt, adapt, reject

| source | adopt | adapt | reject |
| --- | --- | --- | --- |
| Codex | small role prompts, layered instructions, runtime sandbox and approvals, on-demand planning, configurable subagents | retain Zuno section provenance and durable receipts while mapping to provider-native roles | hidden identity, complete copied prompt text, product-private workflow |
| OpenCode | discoverable commands, Skills, MCP, model environment, attachments, compaction recovery | expose them through typed components and the final capability snapshot | model-name replacement of the whole prompt; string assembly as runtime authority |
| oh-my-opencode-slim | bounded task protocol, explicit specialist boundaries, parent/result coordination | use durable JobStore and FIFO inbox instead of hook-owned boards | forced Designer routing, long orchestrator policy, default polling |
| oh-my-openagent | model-capability matching, goal persistence, current-intent reclassification | use short verified capability fragments and typed continuation state | thousand-line model-family prompts, repeated hook continuation, Markdown state as authority |

DeepSeek Harness remains a separate control-plane influence: Goal, Plan, Todo,
Job, background execution, and queue state are typed runtime records, and prompt
sections are projections of that state. Adopting new DSH behavior still requires
the repository's dedicated upstream-sync review.

## Architecture differences

| concern | Zuno Generation 2 target | Codex | OpenCode | Slim | OMO |
| --- | --- | --- | --- | --- | --- |
| Prompt core | concise role plus stable typed runtime sections | compact base plus layered context | complete model-selected prompt files plus assembled context | role prompts plus plugin policy | large model-family orchestrator prompts |
| Capability truth | locked runtime snapshot and receipts | runtime tools, sandbox, approvals | runtime registry with permission-filtered Skill/MCP text | configured agents and hook-managed task state | dynamically rendered agents, categories, tools, and Skills |
| Delegation | typed `DelegationContract`, durable report metadata, completion barrier | configurable subagents and runtime lifecycle | task/subtask parts in session runtime | explicit task tools and wake gates | category routing and delegated Skill selection |
| Persistence | SQLite Goal, Plan, Todo, Job, inbox, prompt receipts | rollout/session state | session messages and compaction | plugin task-session board | OpenCode state plus continuation hooks |
| Model specialization | small fragments only after capability verification | model/session instructions | entire provider prompt selected by model name | configurable role model | complete prompt family selected by model |
| Designer | `ui-design` Skill, no mandatory built-in Agent | optional user customization | user/plugin Agent | built-in Designer | category/Agent routing |
| Audit | section source, digest, effective capabilities, provider projection | prompt/input and rollout diagnostics | inspectable assembled runtime | plugin task/status inspection | plugin prompt inspectable; final wire remains OpenCode-owned |

Generation 2 therefore combines Codex-like typed request boundaries,
OpenCode-like discovery, Slim-like bounded delegation, DSH-like durable
work-state authority, and selected OMO model-fit lessons without becoming
compatible with any of those products.

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
3. **Completed:** create a missing Zuno-owned starter global `AGENTS.md`
   without overwriting user content, keep it active across
   `ZUNO_CONFIG_DIR` profiles, and require an explicit `instructions[]` entry
   for another product's instruction file.
4. **Completed:** reduce initial catalog weight while keeping source identities
   and complete search/list escalation.
5. **Completed:** `zuno debug prompt --session <id> [--step <non-zero>]`
   defaults to redaction and resolves each step through
   `session.provider.request.1.promptReceiptID`.
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

Generation 2 now has the typed `DelegationContract`, host-generated
`TaskReportMetadata`, logical-task deduplication, evidenced uncertain-Job
reconciliation, one completion barrier, bounded same-snapshot
`runtime.work_state`, and both debug commands. The remaining acceptance boundary
is the final workspace gates and the recorded real-provider E2E matrix described
in the Chinese audit.

## Source map

Zuno:

- `crates/zuno-engine/src/prompt.rs`
- `crates/zuno-engine/src/loop.rs`
- `crates/zuno-cli/src/cmd/turn.rs`
- `crates/zuno-provider-openai/src/request.rs`

Codex:

- `codex-rs/core/prompt_with_apply_patch_instructions.md`
- `codex-rs/core/src/client_common.rs`
- `codex-rs/core/src/client.rs`
- `codex-rs/core/src/agents_md.rs`
- `codex-rs/core/src/context/user_instructions.rs`
- `codex-rs/ext/skills/src/catalog_prompt.rs`
- `codex-rs/ext/skills/src/fragments.rs`
- `codex-rs/ext/skills/src/host_prompt.rs`
- `codex-rs/skills/src/selection.rs`

OpenCode:

- `packages/opencode/src/session/prompt.ts`
- `packages/opencode/src/session/system.ts`
- `packages/opencode/src/session/compaction.ts`
- `packages/opencode/src/tool/task.ts`

oh-my-opencode-slim:

- `src/agents/orchestrator.ts`
- `src/agents/designer.ts`
- `src/agents/task-rejection.ts`
- `src/hooks/task-session-manager/`
- `src/tools/task-status.ts`
- `src/tools/task-result.ts`

oh-my-openagent:

- `docs/guide/agent-model-matching.md`
- `packages/omo-opencode/src/agents/sisyphus-agent-factory.ts`
- `packages/omo-opencode/src/agents/sisyphus/claude-opus-5.ts`
- `packages/omo-opencode/src/agents/dynamic-agent-category-skills-guide.ts`
- `packages/omo-opencode/src/agents/builtin-agents/sisyphus-agent.ts`
- `packages/omo-opencode/src/agents/sisyphus-runtime-prompt-reconciler.ts`
- `packages/omo-opencode/src/plugin/system-transform.ts`
