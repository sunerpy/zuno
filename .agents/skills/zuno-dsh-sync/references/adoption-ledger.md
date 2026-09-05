# DSH Adoption Ledger

This ledger records reviewed upstream behavior. It does not promise API compatibility with DeepSeek Harness.

## Baseline

- DSH: `dsh-v0.1.2-alpha.2` at `0a53fb55bea101816fa226bb964ae2bed71c343b`
- Zuno review point: `160456a9f3c3fcf2f46cf7d7e5eeab9e534a724a`
- Reviewed: 2026-08-31

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

Delegation admission and durable job-state hardening were classified `adapt`.
Zuno now records background native and product-agent work as `queued`, moves it
to `running` only after a fair process-local FIFO permit is reserved, and exposes
both states through the shared work projection and TUI. Cancellation removes
queued work without starting it. Restart reconciliation cancels jobs that never
started and marks already-running work `Uncertain`, preserving the non-replay
contract. Cross-process leases and per-parent/provider/model quotas remain later
work; the unchanged DSH baseline does not move.

Workflow scheduling hardening was classified `adapt`. Zuno keeps immutable DAG
templates and stable declaration-order results, but replaces wave/batch waiting
with a work-conserving scheduler that immediately fills a free `maxParallel`
slot from the next ready node. Parent cancellation is explicitly biased ahead of
same-tick completion and checked again before a workflow can publish
`Completed`. This strengthens native Rust execution semantics without adopting a
DSH runtime API; the unchanged DSH baseline does not move.

Cancellation and tool-boundary hardening were classified `adapt`. Foreground
native `task` delegation now receives the parent turn interrupt explicitly,
bridges it to the child cancellation token, aborts the live child control, and
waits for drain plus host shutdown before settling. The engine test suite now
proves configured parallel-call ceilings above the bound and two-sided
`Exclusive` barriers around both `ParallelSafe` and `IsolatedBackground` calls,
while keeping durable results in model order. No DSH API was copied and the
unchanged baseline does not move.

## 2026-08-27 no-delta review

The recorded baseline and locally available upstream head both resolve to
`b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` (`dsh-v0.1.1-rc.2`). Refreshing the
external checkout could not update `.git/FETCH_HEAD` from this restricted
workspace, so this comparison is explicitly cache-derived. No new commit was
observed and the baseline JSON was not advanced.

The three-mode Shell vocabulary was classified `adapt`. Zuno exposes
`read-only`, `workspace-write`, and `danger-full-access` as native typed policy,
but adds a trusted configuration-source ceiling, Agent capability narrowing,
fail-closed platform probes for confined modes, durable execution-authority
schema version 2, and an explicit native backend for full access. Confined
backend failure never selects full access. Approval, background execution,
cancellation, logs, usage, and process lifecycle remain one shared path.

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

## 2026-08-31 targeted learning review

`dsh_delta.py` fetched `origin/master` successfully and compared the recorded
`b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` baseline with
`0a53fb55bea101816fa226bb964ae2bed71c343b` (`dsh-v0.1.2-alpha.2`). The full
delta contains 1,313 commits and 6,808 changed files. This review classified
only the feedback and Skill-maintenance changes needed by Zuno's user-learning
flywheel; the baseline JSON was not advanced because the remaining material
delta has not been reviewed.

| DSH idea | Classification | Zuno action |
| --- | --- | --- |
| Per-assistant-message feedback sidecar with persisted-target validation and compare-and-set revisions | `adapt` | Keep a typed `FeedbackService` and stale-revision rejection. Zuno additionally appends `learning.feedback.changed` to the durable session audit log instead of making feedback projection-only state. |
| Code-review Skill maintenance produces a complete candidate, diff, evidence manifest, independent review, gates, and a source-blob drift check | `adapt` | Store complete `SkillCandidate` content, diff, Experience evidence, source identity/digest, immutable cassette evaluation, explicit human review, and CAS apply. Zuno does not create a PR or rerun real side effects. |
| Direct or silent replacement of a Skill from learned output | `reject` | A candidate cannot apply automatically. Built-in/read-only sources create a differently named project companion, source drift becomes `stale`, and applied changes can only be removed through reviewed revocation or explicit undo. |

## 2026-09-05 dsh-v0.1.3-alpha.1 targeted memory review

`dsh_delta.py` fetched `origin/master` successfully and compared the recorded
`0a53fb55bea101816fa226bb964ae2bed71c343b` baseline
(`dsh-v0.1.2-alpha.2`) with
`d347e703908d0406b7a7ef80e3a0e594d86b2215`
(`dsh-v0.1.3-alpha.1`). The fresh range contains 750 commits and 3,995 changed
files. This review classified only memory-adjacent session-reference,
compaction, and context-provenance behavior; the baseline JSON is not advanced
because the remaining material delta is still unreviewed.

| DSH idea | Classification | Zuno action |
| --- | --- | --- |
| Explicit cross-session references with exact source session ids, captured sequence, byte budgets, retained/omitted counts, truncation facts, self-reference rejection, and nested-reference suppression | `adapt-later` | Add only through a typed session-reference capability and durable prompt section. It must remain explicit user recall, never an ambient memory writer, and every client must show completeness before presenting recalled text as context. |
| Recalled session text is fenced as untrusted, read-only background data and logged immediately after the citing message | `adapt` | Preserve the same provenance rule for external context used by learning eligibility: tools mark external context durably, the session policy records exclusion, and prompt receipts remain the authoritative account of bytes the model saw. |
| Recall UI associates labels only with the directly preceding citing message and renders unknown source shapes opaquely | `adapt-later` | Reuse this rule when Zuno gains session references; do not overload resident Memory or Experience retrieval with UI-local adjacency guesses. |
| Mutable or background long-term memory writer | `reject as absent` | Alpha.1 adds no such subsystem. Zuno keeps Experience extraction, reviewed Memory candidates, deterministic pattern mining, Skill evaluation, and at-most-once effects as its native learning path. |
| Compaction summaries remain durable context while resident prompt memory stays outside the summary stream | `already-covered` | `SessionMemory` freezes stable prompt sections, prompt receipts retain their exact source/digest, and tests prove both memory scopes survive repeated compaction without entering the summary request. |

## Next Review

Run the delta script against `origin/master`. Every changed file group must receive a decision before updating the JSON baseline.
