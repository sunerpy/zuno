# ADK design references and stable-main synchronization

The enterprise preview first synchronizes v0.10.31 with stable v0.10.37, then uses
ADK concepts to review the remaining architecture. Zuno retains its Rust shared
kernel; ADK is a design source, not a compatibility target. App/UI remains paused
and future design starts in Penpot.

## Exact references

- Zuno v0.10.37: `4762790adaab0a9af8c93152c1b6136446e4dbc8`.
- ADK Go v2.4.0: `4e57df40cbd35f055ba654b03ffb0e509fdd754a` (`google.golang.org/adk/v2`).
- Documentation was fetched through the configured `adk-docs-mcp`, starting with its source catalog and `https://adk.dev/llms.txt`; hashes are in `adk-reference.json`.
- Documentation support badges can lag the Go implementation: the Resume page labels Python/Kotlin while the pinned Go Workflow reconstructs HITL state from events; the compaction page omits Go while this source already contains `session/compaction`. Go behavior must be verified against its pinned source/tests.

## Adoption ledger

| Reference | Decision and Zuno consequence |
| --- | --- |
| Injected Session/Memory/Artifact services and separate Runner/Agent roles | Adopt through HarnessProfile, RuntimeBackendBundle, AgentApplication and bounded AgentDriver; no client-owned loop |
| Shared session-service conformance suite | Adopt and extend to atomic admission, CAS, completion consumption, compaction, failure and restart for SQLite/PostgreSQL/state API |
| Persist non-partial events before yield | Adopt with CommittedFrame/LiveFrame and transactional state/event/outbox commits |
| App/user/session/temporary state scopes | Adapt into typed ownership; string key prefixes are not authorization boundaries and temporary state cannot own approvals or budgets |
| Invocation/agent-call/step hierarchy and branch isolation | Adapt to Zuno's explicit Session/Turn/Job/Attempt/Invocation identities; visibility and authority stay independent |
| Event-derived Workflow reconstruction with invocation/interrupt IDs | Adapt to existing persistent waits/checkpoints/consumption; never guess the current run by scanning unscoped history |
| RerunOnResume versus handoff | Permit re-entry only for declared safe/idempotent work; commands/edits/MCP use original operation receipts and retain uncertainty |
| Versioned artifact service | Adopt with immutable digests, scoped authorization, bounded streams and retention leases; use staged upload/commit across object storage and metadata |
| Skill source interface for metadata/instructions/resources | Adopt to replace embedded-body-only limitations with versioned logical resource access; package installation/execution still passes through approval and gateway |
| Session ingestion and user memory search | Adapt while retaining evidence, revocation, independent model/maintenance authorization and organization review |
| Lifecycle plugins | Adopt through typed components/hooks; persist the actual post-hook request and protect compaction/approval/completion facts from silent hook removal |
| Append-only compaction, rolling summary and raw tail | Adopt with stable event-cursor coverage/digests (not timestamp-only identity), exact-value preservation and recovery tests; compaction cannot discard authoritative history or turn a completed answer into a failed task |
| Authentication separate from authorization | Adopt while retaining issuer/audience/app/tenant/resource/policy/HITL/lease checks; user-ID equality alone is insufficient |
| Timestamp stale checks and automatic schema migration | Do not copy: use CAS, database-time leases, epochs and guarded marker-last migrations with exact historical fixtures |
| Mutating in-memory session before database commit | Do not copy: a storage failure must not leave an apparently committed projection |

## Plan changes

1. Finish stable-main synchronization before new feature work: core SQLite format 15 and preview overlay 1 stay independent; input gates and batch receipts move through the existing persistence boundary. Worker protocol 14 returns whether input was consumed. Validate migrations, native Goal/Plan/input/ACP and the enterprise process fixture before integration.
2. Establish backend conformance tests before adding adapters. Include duplicate request IDs, stale epochs, revocation, reordered receipts, commit failure and restart. Test public and Worker protocols separately.
3. Complete the Skill source/resource and artifact services with version/digest/authorization carried through retrieval, installation, rollback and retention.
4. Shared automatic maintenance produces reviewable changes using the existing Job/budget/event machinery and only explicitly shared valid evidence. Organization approval remains required; no synthetic user or second scheduler.
5. Test backup/restore, rolling upgrades, retention and aggregate resource limits against real external facts. A restored database that is behind an already executed operation must reconcile it, never infer replay safety from successful event reconstruction.
6. At each phase record current main and preview SHAs. Update unpublished feature work against preview; integrate stable-main changes through sync PRs without rewriting merged or released history.

After incorporating v0.10.37, the next eligible initial preview is
**0.10.38-preview.1**. Publication remains disabled until acceptance is complete.

Source pointers at the pinned ADK Go commit: `runner/runner.go`, `runner/run_node.go`,
`agent/context.go`, `session/service.go`, `session/database/service.go`,
`session/sessiontestsuite/service_suite.go`, `memory/service.go`, `artifact/service.go`,
`workflow/persistence.go`, `workflow/config.go`, `plugin/plugin.go`,
`session/compaction/compaction.go`, `tool/skilltoolset/skill/source.go`,
`server/authn/authn.go`, `server/authz/strict.go`.
