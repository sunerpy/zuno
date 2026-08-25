# DSH Adoption Ledger

This ledger records reviewed upstream behavior. It does not promise API compatibility with DeepSeek Harness.

## Baseline

- DSH: `dsh-v0.1.1-rc.2` at `b150a551b8d465e31e418e1b2eaf5e79bbb7d28e`
- Zuno review point: `45d1d0c`
- Reviewed: 2026-08-22

## Already Covered

| DSH idea | Zuno evidence | Decision |
| --- | --- | --- |
| Replaceable composition and reversible registration | Native scoped services, transactional profile activation, and profile tests in `zuno-runtime` and `zuno-harness` | Keep extending the native Rust model; do not restore the retired OpenCode plugin bridge. |
| Durable subagent delivery and parent wake | Durable inbox/jobs plus `reportDelivery: nextStep | quiet`, `task`, and `job` tests | Preserve job/session id separation and wake through admitted input. |
| Consumer-owned concurrent web search | Batch `web_search` over a single-query provider with cancellation and deterministic merge tests | Keep concurrency and presentation outside providers. |
| Goal continuation after transient failure | Persistent exponential backoff, restart recovery, typed failure policy, and CLI integration test | Continue strengthening coverage; never replay uncertain tool side effects. |
| Dynamic Cordis package lifecycle | `zuno-extension` process registry, typed lifecycle tools, active-composition generation, and TUI rebuild | `adapt`: preserve define/run/stop/undefine and restart-loss semantics; reject JavaScript/Cordis ABI compatibility. |
| Static profile/bundle loading | `zuno.extension/v1` manifests under config-root `extensions/<id>/extension.json` | `adapt`: use the same validated agent/workflow/skill package as the dynamic path; compiled services remain Rust profiles. |
| Codex and Claude Code product subagents | `zuno-product-agent`, configured static tools, generic durable jobs, `job_cancel`, and the shared TUI subagent projection | `adapt`: retain native product protocols, authentication, configuration, and model choice; reject the TypeScript API as a compatibility target. |
| Reflection, memory review, and durable work state | `memory_propose`, `MemoryCandidate`, `WorkStateProjection`, reflection request/outcome events, and `/memory` | `adapt`: preserve isolated post-delivery learning and live work tracking, but require an auditable review path, explicit promotion policy, typed uncertainty, and no automatic code/prompt/agent/skill rewrites. |
| Goal-tool authority, cancellable questions, and prompt guidance | Typed goal statuses and blocked audit, composer-owned `question` dialog, durable steer/next-step inbox, concise role prompts, and prompt golden tests | `adapt`: keep direct authority and terminal evidence in typed runtime policy, surface cancellation as denial, and use short role/tool guidance instead of copying DSH's TypeScript prompt assembly. |

## 2026-08-22 no-delta review

`dsh_delta.py` resolved both the recorded baseline and current `origin/master` to `b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` (`dsh-v0.1.1-rc.2`). There were no unreviewed commits, so the baseline JSON was not advanced.

The Codex and Claude Code product-subagent design was reviewed from the already-pinned release and classified `adapt`: Zuno uses native Rust providers, durable `JobSubject::ProductAgent` state, `ToolReplayPolicy::Never`, process-tree cancellation, and frontend-neutral `ToolUiIntent::Subagent` projection.

The TUI/runtime/memory upgrade was also classified `adapt` against the pinned
release and current design sources: Zuno uses frontend-neutral activity/work
projections, durable reflection events, candidate review, and at-most-once file
application/undo. No upstream commit was added, so the baseline remains
`dsh-v0.1.1-rc.2`.

The native lifecycle-kernel review was classified `adapt` against the same pinned
release. Zuno now uses side-effect-free component preparation, owned deferred
effects, fallible/uncertain cleanup, child-first shutdown, old-before-new profile
replacement, scope-local extension transactions, active-consumer leases, and
frontend-neutral lifecycle snapshots. It deliberately does not embed Cordis or
load executable JavaScript/Rust plugins in-process. The upstream delta remained
empty, so the baseline is unchanged.

## 2026-08-23 no-delta review

`dsh_delta.py` again found no commit after `dsh-v0.1.1-rc.2`, so the recorded
baseline remains `b150a551b8d465e31e418e1b2eaf5e79bbb7d28e`.

Runtime-loadable plugin behavior was classified `adapt`. Zuno keeps trusted
compiled Rust `Component`s for first-party typed services, adds a Wasmtime
Component Model host for statically installed tools with explicit
workspace/network/environment grants and resource budgets, and provides a
contained `host.full` process protocol for APIs outside WASI. Both runtimes are
profile effects with initialize/invoke/shutdown negotiation, reverse cleanup,
transactional publication, cancellation, and `Uncertain` non-replay. Rust dylibs
and the JavaScript/Cordis ABI remain rejected.

Custom agent/workflow behavior was also classified `adapt`: extension agents use
the same native model, permission, file, network, environment, child-session, and
strict-HITL paths as built-ins, and `subagent`/`all` agents enter the exact `task`
target roster. Workflows invoke that public delegation path rather than acquiring
a private loop.

Goal and human-input guidance was classified `adapt` against the same pinned
release. Zuno keeps explicit goal-creation authority, truthful complete/blocked
audits, cancellable questions, and durable steer-versus-next-step delivery, but
expresses those invariants through native Rust types and concise agent/tool
prompts. The upstream delta is still empty, so the baseline does not move.

Durable memory cadence and consolidation were classified `adapt` from the pinned
DSH reflection design and Codex's two-stage memory writer. Zuno counts delivered
assistant messages in SQLite, leases each selected reflection as a non-replayable
job, and supplies the exact resident snapshot to an isolated reviewer. It rejects
direct model ownership of a mutable memory workspace: consolidation remains an
audited, deduplicated, and undoable add/replace/remove candidate. The upstream
delta remains empty and the baseline does not move.

## 2026-08-25 no-delta review

`dsh_delta.py` resolved both the recorded baseline and current `origin/master`
to `b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` (`dsh-v0.1.1-rc.2`). There were
no unreviewed commits, so the baseline JSON remains unchanged.

The first Cordis-semantics implementation slice was classified `adapt`. Zuno
keeps native typed executable services and adds a descriptor-only named
capability plane with validated identity, contracts, provenance, scope-local
generations, parent/child shadowing, atomic publication, withdrawal before
cleanup, and stale-generation detection. It does not adopt the Cordis JavaScript
ABI, a Rust dylib ABI, or `cordis-rs`'s synchronous lifecycle model. Product
descriptor publication, dependency-closure reconciliation, and the async
event/policy bus remain later work; the DSH baseline does not move.

The Phase 1 product-adoption follow-up was also classified `adapt`. Extension
Tool objects remain native typed services while their exact provider schema
digests are projected as named contracts. The immutable orchestration snapshot
publishes Agent Profile, Workflow Template, and Skill descriptors in the same
profile transaction; same-name Skills use source-derived isolation scopes.
Provider Attempt persistence now reuses the same Tool schema identity function.
MCP/remote-host projection and affected dependency-closure reconciliation remain
later work, and the unchanged DSH baseline does not move.

## dsh-v0.1.1-rc.1

| Change | Classification | Zuno action |
| --- | --- | --- |
| DeepSeek vision model and multimodal request path | `adapt-later` | First define a generic attachment/image capability with durable admission, limits, provider projection, and TUI rendering. |
| Bubblewrap `/proc/<pid>/root` confinement fix | `watch` | Audit whether Zuno invokes Bubblewrap or exposes an equivalent process-root escape before declaring applicability. |
| Multiline `ask_user_question` answers | `adopt-now` | Add a TUI interaction test for multiline editing, wrapping, and explicit submit/newline bindings. |
| Subagent conversation header navigation | `adapt-later` | Expose parent/child session relationships through a frontend-neutral projection, then improve TUI navigation. |
| Markdown table and cache-ratio presentation fixes | `watch` | Revisit when Zuno renders structured Markdown tables and token-cache telemetry in the TUI. |
| Turn error remains visible after same-turn retries exhaust | `adopt-now` | Verify the final terminal error survives provider retries and durable goal handoff in both CLI and TUI. |

## dsh-v0.1.1-rc.2

| Change | Classification | Zuno action |
| --- | --- | --- |
| Canonical image normalization with automatic resize and format conversion | `adapt-later` | Build it behind the generic attachment capability, with deterministic limits and durable source facts. |
| DeepSeek Files API upload reuse | `adapt-later` | Add only after provider-neutral attachment projection exists; cache identifiers must not become the durable source of truth. |
| Inline fallback when Files resolution fails | `adapt-later` | Keep fallback typed and observable; do not hide authentication or permanent protocol errors. |
| Separate Files and streaming timeouts | `already-covered` as a design rule | Keep independent timeouts for independent operations when the image provider is implemented. |
| Permission-default change introduced before rc.1 and reverted before rc.2 | `reject` | Do not copy transient release-line behavior. Review the final permission invariant instead. |

## Next Review

Run the delta script against `origin/master`. Every changed file group must receive a decision before updating the JSON baseline.
