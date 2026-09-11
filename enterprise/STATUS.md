# Enterprise implementation status

Baseline: `v0.10.29`, `d1212860dba6a6b420ce81d444ace58feaf0adb5`.

| Phase | State | Evidence required before completion |
| --- | --- | --- |
| P0 | Complete | PR #176 merged into preview; native CI run 34553054820 succeeded |
| P1 | In progress | Principal propagation implemented; ownership migration validated; local bounded driver and scoped session application validated; local runtime Job/lease port validated; Memory persistence/authority ports validated; backend assembly remains |
| P2 | In progress | PostgreSQL session TLS/RLS verified locally; runtime Job/Memory, identity/approval and remote state remain |
| P3 | Pending | Two workers, Docker gateway, root-task takeover |
| P4 | Pending | Durable child/Workflow/Council waits, merges and cancellation |
| P5 | Pending | Web, SDK, remote ACP, common projections |
| P6 | Pending | Fault injection, native artifacts, preview publication |

Publication remains disabled until the first runnable root-task preview has
completed its acceptance gates. This file records actual completion, not planned
capabilities.

## Workspace

- Integration branch: `codex/enterprise-preview`.
- Current phase branch: `codex/enterprise-p2-postgres`.
- The primary checkout and pre-existing worktrees remain owner-controlled.

## Validation

- Preview publisher tests: 11 passed.
- Documentation and existing release-surface contracts: 99 passed.
- `cargo fmt --all --check`: passed.
- `cargo check --workspace --all-targets --offline`: passed.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: passed.
- Actionlint and README formatting: passed.
- P0 merged commit: `c85c03cfe9871d2c19cd57b1d8cb2223882af28e`.
- P1 SQLite ownership and migration: 426 tests passed.
- P1 engine/type/tool suite: 496 tests passed (one documented doctest ignored).
- P1 English/Chinese documentation contracts: 48 passed.
- P1 shared workspace check, Clippy, formatting and diff checks: passed.
- Ownership PR #177 merged at `b56a2aeeeb32b3891ad0830ea4d129e8fb33432e`; CI run 34557423275 succeeded.
- Bounded driver: 689 engine/LLM/tool tests and 48 documentation contracts passed.
- Bounded driver workspace check, Clippy, formatting and diff checks: passed.
- The driver remains a local SQLite boundary; it is not a remote worker or an environment lease.

No enterprise runtime or preview release has been certified yet. Publication
stays disabled in `preview.json`.

## Application and checkpoint follow-up

- Driver PR #178 merged at `7ae2b792f650bab9723784d9d23fdf0ddf614110`; CI run 34559371761 succeeded.
- `zuno-application` supplies the scoped facade and async session persistence port; SQLite supplies its real provider.
- Application/SQLite regression: 435 passed; bounded driver follow-up: 9 passed.
- Documentation/release contracts: 99 passed; shared workspace check, Clippy, fmt and diff checks passed.
- Checkpoint schema 2 includes time between advances in the turn wall-clock allowance. No checkpoint format has been released in this channel yet.
- Job scheduling/leases, input-version CAS, Memory ports, PostgreSQL, Entra, the gateway and Web are still required.

## Native runtime Job store

- Application PR #179 merged at `102971efdcddfda1d2615015a3ca47222631e8ee`; CI run 34563112608 succeeded.
- `RuntimeStore`, `SqliteRuntimeStore` and native `JobDispatcher` provide atomic root input/Job admission, separate input CAS, fair owner scheduling, session leases, execution attempts, checkpoint release and fenced settlement.
- SQLite format 14 preserves prior data and widens the native Job constraint through a guarded transactional replacement. Schema validation preserves case and whitespace inside SQL string literals.
- Database/application tests: 446 passed. Tool/TUI tests: 1,888 passed. Documentation/release contracts: 99 passed.
- Workspace check, Clippy, fmt and diff checks passed.
- Lease expiry remains conservative: the in-flight Job becomes uncertain; other sessions may continue. External-operation reconciliation and automatic remote takeover are not implemented by this store.
- No enterprise binary, deployment, preview tag or release has been published.

## Preview CI follow-up

PR #180's first run (34566618169) exposed a cold event-initialization race and a
Windows Goal suite timeout. A 12-publisher memory-database test reproduces the
initialization failure. Event initialization now runs once and excludes pool
writers; 68 Server tests pass. The Goal fixture selects Bash/PowerShell explicitly
and uses native diagnostic syntax; its Linux end-to-end run passes. Windows
verification is pending the next preview CI run. No release is enabled.

## Memory persistence and authority

- `MemoryService` accepts one `MemoryPersistence` provider plus an explicit `MemoryAuthority`; the local provider keeps all stores on one pool.
- Existing candidate, revision, evidence, maintenance and undo semantics are retained. Candidate read/edit now enforce scope ownership; model tool provenance uses immutable call coordinates.
- Memory/Learning tests: 179 passed. Focused Memory tool tests: 4 passed. Documentation contracts: 48 passed.
- Workspace check, Clippy, formatting and diff checks passed after rebuilding task-owned first-party artifacts.
- File projection remains local. Organization authorization at commit, managed namespaces and the external data owner/worker API are not certified by this port alone.

## PostgreSQL session persistence

- Memory PR #181 merged at `01300d8b911bf266bac9508b3702cc293a5e0cf9`; CI run 34572195521 succeeded.
- Runtime PR #180 follow-up CI 34570476046 succeeded, including the Windows Goal test.
- `PostgresSessionPersistence` implements the same session application port using a separate preview schema, restricted runtime role, verified TLS and transaction-local RLS identity.
- The real PostgreSQL 18 TLS contract passed locally: role limits, TLS refusal, RLS, connection reuse, isolation, idempotency, paging, rollback and schema-integrity checks.
- Preview publishing contracts: 11 passed. Documentation/release contracts: 99 passed. Shared workspace check, Clippy, fmt and diff checks passed. Actionlint passed.
- PostgreSQL 16 CI passed on Linux amd64/arm64 in run 34576215561. Its shared supply-chain gate identified the SQLx CA data package; `webpki-roots` now has an explicit per-package data-license exception, like `webpki-root-certs`.
- PostgreSQL Job/Memory storage and engine integration, Entra, HITL, gateway, distributed waits and the Web are still required. Publication remains disabled.
