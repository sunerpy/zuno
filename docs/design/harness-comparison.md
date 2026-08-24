# Harness design comparison

Status: 2026-08-23.

This document records which ideas Zuno adopts from other agent harnesses. The source projects are references, not runtime compatibility targets.

## Sources

- [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
- [OpenAI Codex](https://github.com/openai/codex)
- [oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent)
- [pi-agent in pi-mono](https://github.com/badlogic/pi-mono)
- [OpenCode](https://github.com/sst/opencode)
- [Claw Code](https://github.com/Michaelliv/claw-code)

## DeepSeek Harness pre-releases

The local upstream checkout and GitHub tags were compared at these exact commits:

| tag | commit | released | useful changes |
| --- | --- | --- | --- |
| `dsh-v0.1.0-rc.8` | `141eb6fef83422698aef7a981029e843e8161534` | 2026-08-19 | Baseline for the current plugin architecture, durable session events, capability roles, and client projections. |
| `dsh-v0.1.1-rc.1` | `528c682e061696f5a160f363f236ecbf53cbd006` | 2026-08-21 | Exhausted same-turn retry remains visible in conversation history; stable session snapshot envelopes; multiline question answers; subagent continuation cleanup; authentication alignment; web session and static-route fixes. |
| `dsh-v0.1.1-rc.2` | `b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` | 2026-08-21 | One canonical image admission/request pipeline, deterministic image normalization and encoding, durable attachments, DeepSeek Files fallback, and a deliberate rollback of an unsafe permission-default change. |

The release sequence reinforces four rules for Zuno:

1. Persist the failure users must understand, even when a lower retry layer has already exhausted its attempts.
2. Make client state a stable projection of durable events rather than a collection of UI-local guesses.
3. Put normalization, limits, fallback, and transport ownership in one capability implementation.
4. Revert a questionable security default instead of preserving it for compatibility during pre-release development.

## Project lessons

### DeepSeek Harness

The strongest idea is "everything is a plugin." In Zuno the concrete ABI is a
native Rust `Component`: it prepares typed services and deferred effects, receives
an exact asynchronous disposer for every started effect, and participates in
transactional `HarnessProfile` replacement. A complete capability has an
interface, one or more providers, and a consumer. Session events own durable
facts; live events only wake active work. Model-visible input must be logged.

Zuno adopts:

- reversible component registration and scoped service lookup;
- profile and bundle composition rather than loop conditionals;
- durable session events, inbox admission, and client projections;
- traceable prompt sections;
- capability interface/provider/consumer separation;
- explicit configuration for deployment-varying choices.

Zuno adapts:

- Cordis plugins become Rust components and typed services;
- `cordis.yml` composition becomes `HarnessProfile` plus Zuno config;
- Cordis' define/run/stop/undefine lifecycle becomes validated
  `zuno.extension/v1` packages with committed versus desired revisions,
  active-consumer leases, exclusive transition reservations, process-local and
  static lifetimes, and no JavaScript ABI;
- DSH's runtime plugin ergonomics become static WASI Component Model packages
  with explicit workspace/network/environment grants, or contained
  `host.full` processes when a guest needs unrestricted host APIs. Rust dynamic
  libraries remain rejected because unload safety and ABI compatibility cannot
  be proven;
- worker-thread orchestration becomes Tokio tasks with durable SQLite coordination.
- product-owned Codex and Claude Code subagents become native Rust protocol
  providers, static Zuno tools, and durable `ProductAgent` jobs rather than a
  TypeScript compatibility API.

### Codex

Codex demonstrates the value of a focused Rust core, explicit sandbox and approval decisions, durable thread state, and a goal store that survives ordinary conversation churn. Zuno adopts a separate goal database, typed terminal states, persistent recovery metadata, and user-input priority.

Zuno extends the goal idea with cross-turn exponential backoff. Provider retries remain bounded inside one request, while an active goal can schedule another turn indefinitely for typed recoverable failures. Unknown side-effect outcomes are never blindly replayed.

Codex also keeps its durable harness contract distinct from shorter role and
mode instructions. Zuno follows that separation: native agent prompts state
ownership, evidence standards, and negative boundaries, while permissions,
replay, input admission, goal state, and lifecycle remain typed runtime policy.
The TUI similarly distinguishes live steering from queued follow-up work and
projects human waits as approval or answer states.

Codex's memory writer separates rollout extraction from workspace consolidation.
Zuno adapts the learning goal but keeps a narrower mutation boundary: durable
delivered-turn admission selects an isolated reviewer, the reviewer receives the
current resident snapshot, and every consolidation is still an atomic
`MemoryCandidate` add/replace/remove operation. Zuno therefore retains source
provenance, deduplication, promotion policy, undo, and uncertain non-replay
instead of allowing a model to rewrite a memory directory directly.

### oh-my-openagent

oh-my-openagent contributes a useful orchestration lesson: a bounded roster is easier for a primary agent to route than a large collection of overlapping personas. Each Zuno specialist therefore carries both a responsibility and a negative delegation boundary. `orchestrator` is the only recursive coordinator; `build` is a direct no-child mode, while `deep`, `fixer`, and `general` can implement but cannot fan out.

Zuno keeps agent identity, permissions, delegation boundaries, and prompt policy as data so validation and rendered prompts cannot drift apart. It adapts OMO's explicit positive and negative role boundaries, but rejects unconsumed XML reply envelopes: user-facing agents return concise natural Markdown, while structured output is reserved for typed runtime consumers.

Zuno also rejects model-specific prompt inflation and universal mandatory
delegation. The useful invariant is explicit child ownership and parent
integration, not repeated all-caps policy or a rule that every implementation
must fan out. Prompt word-count tests keep the primary role contracts concise.

### pi-agent

pi-agent's small loop and extension-first package structure show how much functionality can remain outside the core turn state machine. Zuno keeps the default `AgentDriver` intentionally narrow and makes benchmark, evaluation, workflow, and remote drivers replaceable profile services.

The lesson is composability, not JavaScript compatibility. Zuno tools and providers remain native Rust traits.

### OpenCode

OpenCode is a useful product reference for provider breadth, terminal workflows, command discovery, and dense interactive feedback. Zuno adopts ergonomic ideas only after expressing them through its own events, commands, configuration, and permissions.

Zuno rejects OpenCode plugin ABI, JavaScript hooks, path fallback, database handoff, HTTP compatibility routes, old tool argument forms, and single-query `web_search` compatibility. The batch form is the only Zuno API.

### Claw Code

Claw Code is a useful Rust terminal-agent reference for process ergonomics, focused command surfaces, and lightweight deployment. Zuno uses those ideas when evaluating TUI latency, status visibility, and binary packaging, while retaining its own session, component, permission, and event models.

## Adoption decisions

| area | decision | Zuno implementation |
| --- | --- | --- |
| Everything is a plugin | adopt | Side-effect-free `Component::prepare`, typed services, deferred `EffectScope`, lifecycle diagnostics, `ProfileBundle`, transactional `HarnessProfile` |
| Agent-authored temporary extensions | adapt | Process-local immutable package registry plus `extension_define/run/stop/undefine/inspect`; committed/desired revisions publish only after old host leases are quiescent and the candidate starts |
| Restart-persistent extension bundles | adapt | Static `.zuno/extensions/<id>/extension.json` packages using the same schema and merger; `zuno plugin add/update/remove/list` owns transactional filesystem installation |
| Runtime-loadable executable plugins | adapt | WASI Component Model tools with explicit grants and budgets, plus a contained `host.full` process fallback; both are deferred effects with reverse shutdown and `Uncertain` outcomes, while Rust dylibs and the JavaScript/Cordis ABI remain rejected |
| Custom agent/workflow capabilities | adapt | Extension agents enter the native `task` roster and use native file, network, environment, permission, strict-HITL, model, and child-session paths; workflows delegate through the normal `task` tool |
| Capability roles | adopt | interface/provider/consumer ownership in separate crates or modules |
| Model-visible means logged | adopt | durable inbox, tool results, retry notices, `session.prompt.assembled` |
| Stable client projections | adopt | cursor replay plus snapshots shared by TUI/server/ACP/future GUI |
| Goal persistence | adopt and extend | separate goal DB, typed recovery reason, persisted exponential backoff |
| Concise agent and tool prompt contracts | adapt | Evidence-oriented role contracts, explicit delegation ownership, bounded edit/write fallback, goal terminal audits, cancellable question semantics, and byte-pinned tool descriptions |
| Durable memory extraction and consolidation | adapt | Per-session delivered-turn cadence, isolated small-model review with the resident snapshot, and audited add/replace/remove candidates instead of direct model-owned file rewrites |
| Tool replay after failure | adapt | `Never` by default; explicit `Safe` only for read-only/idempotent tools |
| Codex and Claude Code product subagents | adapt | Native app-server/stream-json providers, static configured tools, durable jobs, explicit cancellation, and uncertain non-replay |
| Bounded specialist roster | adopt | `orchestrator`, `build`, `plan`, `deep`, `fixer`, `general`, `explorer`, `librarian`, `oracle`, `looker` |
| Team model presets and orchestration Skills | adapt | Schema-generated Agent/category routes frozen per turn, plus an original static `zuno-orchestration` Skill pack; native permission, scheduling, persistence, and lifecycle remain the only runtime authorities |
| Provider-specific batch search | reject | concurrency belongs in the shared consumer above single-query providers |
| UI-owned agent behavior | reject | clients render events and submit commands; they do not run a private loop |
| Cross-project compatibility | reject | Zuno-native config, data, commands, tools, events, and extension interfaces |

## Next comparison cycle

Run `python3 .agents/skills/zuno-dsh-sync/scripts/dsh_delta.py` from the worktree, inspect the new tag range, and append only decisions that change Zuno's architecture or backlog. Do not copy a feature solely because it exists upstream.
