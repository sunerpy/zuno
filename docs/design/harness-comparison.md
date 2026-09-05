# Harness design comparison

Status: 2026-08-31.

This document records which ideas Zuno adopts from other agent harnesses. The source projects are references, not runtime compatibility targets.

## Sources

- [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
- [OpenAI Codex](https://github.com/openai/codex)
- [Codex GitHub Action guidance](https://learn.chatgpt.com/docs/github-action)
- [Codex non-interactive mode guidance](https://learn.chatgpt.com/docs/non-interactive-mode)
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
| `dsh-v0.1.2-alpha.2` | `0a53fb55bea101816fa226bb964ae2bed71c343b` | 2026-08-31 | Public-target web fetch hardening, safe search diagnostics, image lifecycle refinements, session-scoped ACP MCP, gated child model selection, headless reasoning progress, loopback browser authentication, and broad client/runtime refactoring. |

The full 1,313-commit and 6,808-file classification, including rejected and
watch-only groups, is recorded in the
[alpha.2 adoption ledger](dsh-alpha2-adoption-ledger.md).

The release sequence reinforces these rules for Zuno:

1. Persist the failure users must understand, even when a lower retry layer has already exhausted its attempts.
2. Make client state a stable projection of durable events rather than a collection of UI-local guesses.
3. Put normalization, limits, fallback, and transport ownership in one capability implementation.
4. Revert a questionable security default instead of preserving it for compatibility during pre-release development.
5. Treat public HTTP validation, DNS pinning, redirect handling, and proxy
   selection as one transport capability.
6. Keep attachment bytes in a content-addressed host object lifecycle and lower
   them to a provider request only at the provider boundary.
7. Freeze delegation authority in durable session policy instead of letting a
   later configuration edit rewrite an existing child.

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
- DSH's `read-only | workspace-write | danger-full-access` vocabulary becomes a
  typed native Shell authority. Zuno adds trusted-source provenance, Agent
  capability narrowing, fail-closed confined backends, durable authority
  snapshots, and an explicit native backend instead of treating full access as
  a fallback;
- worker-thread orchestration becomes Tokio tasks with durable SQLite coordination.
- product-owned Codex and Claude Code subagents become native Rust protocol
  providers, static Zuno tools, and durable `ProductAgent` jobs rather than a
  TypeScript compatibility API.

### Codex

Codex demonstrates the value of a focused Rust core, explicit sandbox and approval decisions, durable thread state, and a goal store that survives ordinary conversation churn. Zuno adopts a separate goal database, typed terminal states, persistent recovery metadata, and user-input priority.

Zuno follows Codex's key security split: approval decides whether an intent may
run, while confinement limits what the process can do. `allow_all` and TUI
automation therefore do not widen a confined sandbox. Zuno's explicit
`danger-full-access` mode intentionally selects both native host authority and
effective `allow_all`; it emits no approval prompts but still honors explicit
permission denies and catastrophic Shell denials. Every mode retains one process
lifecycle and durable authority path.

Zuno extends the goal idea with cross-turn exponential backoff. Provider retries remain bounded inside one request, while an active goal can schedule another turn indefinitely for typed recoverable failures. Unknown side-effect outcomes are never blindly replayed.

Codex also keeps its durable harness contract distinct from shorter role and
mode instructions. Zuno follows that separation: native agent prompts state
ownership, evidence standards, and negative boundaries, while permissions,
replay, input admission, goal state, and lifecycle remain typed runtime policy.
The TUI similarly distinguishes live steering from queued follow-up work and
projects human waits as approval or answer states.

At `openai/codex@f98649c`, the public CLI turn loop continues while the model
requires follow-up and does not use a fixed step counter; the public
configuration reference likewise exposes no mandatory turn-count ceiling.
Zuno adapts that behavior by leaving Agent `steps` unset by default. A user may
still configure a positive `steps` guard, after which Zuno permits one
tool-free finalization request and durably records why tool authority was
closed. This is a Zuno-native safety and reporting contract, not Codex
configuration compatibility.

The same comparison informs execution economy rather than another scheduler.
Simple isolated actions execute directly. The orchestrator batches independent
reads and checks, avoids re-reading unchanged state, and classifies a complete
commit set before changing the index. The `git-workflow` Skill carries the
longer repository procedure and runs shared gates after cohesive commit
batches. Runtime permissions, durability, and verification remain authoritative
if model guidance is ignored.

Codex's CI guidance also separates repository-owned prompts and read-only
analysis from credentialed writes, uses least-privilege permissions, retains
machine-readable output or patch artifacts, and treats the write job as a
separate gate. Zuno adapts that method in the on-demand `github-delivery` Skill.
The runtime contributes one additional durable invariant: a remote observer is a
typed background purpose. Its local process result can wake the parent, but only
a fresh query of the remote run/ref plus required job and artifact evidence can
complete the owning Plan gate. A green summary with skipped or absent required
work remains unverified.

The directly selectable `deep` mode also adapts the outcome-oriented discipline
in Codex's base instructions at `openai/codex@e9a446d`: inspect before guessing,
fix root causes with focused changes, continue until the requested outcome is
real, and validate from narrow checks toward the user-facing surface. Zuno keeps
those principles in a concise role prompt; sandbox, approval, durability, and
completion remain runtime contracts.

Codex's memory writer separates rollout extraction from later consolidation.
The `openai/codex` main snapshot `068336858475fd96e20e5b2590da869326822826`
was inspected through `codex-rs/memories/write/src/{start,guard,phase1,phase2,runtime}.rs`
and `codex-rs/config/src/types.rs`; the SHA identifies the reviewed tree rather
than a memory-specific introducing commit. In that tree each thread durably
freezes whether it may contribute, external-context tools can make that thread
ineligible, Phase 1 claims only idle interactive rollouts under bounded startup
and age limits, redacts secrets before model extraction, and Phase 2 runs under
a global lease with a separate model, restricted workspace, no network, no
delegation, a watermark, heartbeat, and artifact validation. Its defaults
include a six-hour idle floor, two rollouts per startup, and a 25-percent
remaining-rate-limit gate.

Zuno adapts the control and scheduling invariants, not Codex's mutable memory
workspace. Session policy separates using existing memory from generating new
learning; automatic extraction is durable, idle-delayed, bounded per worker
wake, and can be excluded after external context. Model-visible retrieval keeps
prompt receipts with stable source identities and digests. Resident Memory
remains an atomic reviewed `MemoryCandidate` add/replace/remove operation, while
slow consolidation remains evidence-backed pattern mining and separately
reviewed Skill evaluation. Zuno does not let a background Agent rewrite Memory
files directly. Zuno has not adopted Codex's 25-percent quota gate because the
provider layer does not yet expose one uniform typed remaining-quota snapshot;
that gate is `adapt-later`, not a current scheduling invariant.

OpenAI trace grading and dataset regression, DSPy's metric-oriented optimization,
and Warp's candidate-diff presentation inform the Skill path. Zuno narrows them
to an immutable cassette suite: baseline and candidate share one
`AttemptSnapshot`, cited failures must improve, protection cases cannot
critically regress, and the overall metric cannot decline. Passing evaluation
still does not apply the file.

LangMem's foreground/background distinction informs fast post-turn recording and
slow consolidation. Zuno does not adopt an ambient mutable memory agent: every
extraction job, Experience, pattern, Memory proposal, Skill diff, evaluation,
and filesystem effect has a durable typed identity.

DeepSeek Harness's message-feedback sidecar is adapted as a revisioned,
compare-and-set service, but Zuno additionally appends an immutable audit event
for every change. DSH's code-review Skill maintenance workflow also validates
candidate diffs against evidence and stops on source drift. Zuno keeps those
properties in the runtime while rejecting any silent Skill overwrite: review,
offline evaluation, and CAS-protected apply are separate states.

### oh-my-openagent

oh-my-openagent contributes a useful orchestration lesson: a bounded roster is easier for a primary agent to route than a large collection of overlapping personas. Each Zuno specialist therefore carries both a responsibility and a negative delegation boundary. `orchestrator` is the only recursive coordinator; `build` is a direct no-child mode, while `deep`, `fixer`, and `general` can implement but cannot fan out.

OMO's Hephaestus prompt at `sunerpy/oh-my-openagent@44c95e9` is the closest
reference for direct deep work. Zuno adapts its useful investigation loop:
establish or reproduce the behavior, test competing hypotheses, inspect one more
dependency when the first answer is suspiciously shallow, prefer the owning
root fix, and verify through the real surface. Zuno does not copy Hephaestus's
identity, model gating, mandatory fan-out, or model-specific prompt variants;
`deep` remains provider-neutral and cannot create children.

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
| GitHub, Actions, and release delivery guidance | adapt | On-demand `github-delivery` Skill, typed `remoteObserver` background purpose, authoritative run/ref refresh, strict required-job conclusions, least-privilege workflow guidance, and exact artifact/checksum evidence |
| Durable memory extraction and consolidation | adapt | Per-session delivered-turn cadence, isolated small-model review with the resident snapshot, and audited add/replace/remove candidates instead of direct model-owned file rewrites |
| Tool replay after failure | adapt | `Never` by default; explicit `Safe` only for read-only/idempotent tools |
| Public web fetch targets | adapt | Proxy-aware `PublicHttpClient`, all-address public validation, validated-IP proxy routing with original Host/TLS SNI, no proxy-to-direct fallback, and per-hop manual redirect validation |
| Credential-bearing search endpoints | adapt | Private wire URL, scrubbed diagnostic endpoint, URL-free causes, and sentinel leak tests |
| Durable normalized image objects | adapt | `zuno-attachment`, content-addressed private objects, admission/request policies, legacy inline read support, and late provider inlining |
| ACP session MCP | adapt | Validated stdio/HTTP declarations, isolated session `ProfileBundle`, atomic tool publication, and exact reverse teardown |
| Child model selection | adapt | Disabled-by-default exact allowlist frozen as a durable session policy digest |
| Headless reasoning | adapt | Explicit `--show-reasoning`, stderr-only stable blocks, no signed/encrypted reasoning, and no JSON combination |
| Loopback browser authentication | adapt | Explicit `--browser-auth`, one-time launch token, authority-bound signed cookie, Origin enforcement, and bootstrap-query redaction |
| Codex and Claude Code product subagents | adapt | Native app-server/stream-json providers, static configured tools, durable jobs, explicit cancellation, and uncertain non-replay |
| Bounded specialist roster | adopt | `orchestrator`, `build`, `plan`, `deep`, `fixer`, `general`, `explorer`, `librarian`, `oracle`, `looker` |
| Team model presets and orchestration Skills | adapt | Schema-generated Agent/category routes frozen per turn, plus an original static `zuno-orchestration` Skill pack; native permission, scheduling, persistence, and lifecycle remain the only runtime authorities |
| Provider-specific batch search | reject | concurrency belongs in the shared consumer above single-query providers |
| UI-owned agent behavior | reject | clients render events and submit commands; they do not run a private loop |
| Cross-project compatibility | reject | Zuno-native config, data, commands, tools, events, and extension interfaces |
| Provider Files API for images | reject | Keep `ImageRequestPolicy` as an extension point; do not add a provider-owned attachment lifecycle in this cycle |

## Next comparison cycle

Run `python3 .agents/skills/zuno-dsh-sync/scripts/dsh_delta.py` from the worktree, inspect the new tag range, and append only decisions that change Zuno's architecture or backlog. Do not copy a feature solely because it exists upstream.
