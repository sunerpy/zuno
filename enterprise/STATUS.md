# Enterprise implementation status

Baseline: `v0.10.29`, `d1212860dba6a6b420ce81d444ace58feaf0adb5`.

| Phase | State | Evidence required before completion |
| --- | --- | --- |
| P0 | Complete | PR #176 merged into preview; native CI run 34553054820 succeeded |
| P1 | In progress | Principal propagation implemented; ownership migration validated; local bounded driver and scoped session application validated; local runtime Job/lease port validated; Memory persistence/authority ports validated; backend assembly remains |
| P2 | In progress | PostgreSQL session TLS/RLS and OAuth2/Entra verifier contracts validated; runtime Job/Memory, login integration, organization policy/approval and remote state remain |
| P3 | Pending | Two workers, Docker gateway, root-task takeover |
| P4 | Pending | Durable child/Workflow/Council waits, merges and cancellation |
| P5 | Pending | Web, SDK, remote ACP, common projections |
| P6 | Pending | Fault injection, native artifacts, preview publication |

Publication remains disabled until the first runnable root-task preview has
completed its acceptance gates. This file records actual completion, not planned
capabilities.

## Workspace

- Integration branch: `codex/enterprise-preview`.
- Current phase branch: `codex/enterprise-p2-authorization`.
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

## General OAuth2/OIDC identity boundary

- User-approved amendment on 2026-09-11: Entra is one provider adapter behind a general OAuth2/OIDC boundary. The original full plan is retained; the amendment is documented in `AUTHENTICATION.md` and `AUTHENTICATION.zh.md`.
- `AccessTokenVerifier` produces a non-deserializable `VerifiedIdentity`. Generic RFC 9068 JWT, explicit provider JWT, RFC 7662 introspection, and Entra v2 implement the same interface.
- Entra shares JWT cryptography, bounded OIDC discovery/JWKS transport and the concurrent key cache. Its tenant, GUID, key-issuer, delegated/app-only rules stay in its adapter.
- Generic identities scope opaque subjects by issuer and user/workload kind; they do not inherit Entra's `tid`/`oid` assumptions. Introspection rechecks revocation each time and never falls back to a previously successful result.
- Identity tests: 15 passed, using ephemeral real RSA signatures. Documentation/release contracts: 99 passed. Shared workspace check, Clippy, fmt, dependency and diff checks passed.
- The HTTP login/BFF, organization authority, runtime authorization integration and live IdP validation are still required. No enterprise authentication endpoint or runtime entry point has been advertised.
- Documentation impact: both language guides and the workspace inventory are updated. These preview docs are packaged with future preview tags; the stable documentation site is not published by this branch.

## PostgreSQL runtime transactions

- PostgreSQL PR #182 merged at `3c82895cf6908df9df4b3378ed5368a73c71fb43`; native CI run 34578776963 passed, including Windows and PostgreSQL on both Linux architectures.
- `PostgresRuntimeStore` implements root admission, separate input CAS, fair owner scheduling, session leases, execution attempts, checkpoints and fenced settlement over native Job identities.
- An execution lease carries owner routing; SQLite and PostgreSQL check that owner against durable state. Shared `JobFinish` validation bounds result and error payloads.
- PostgreSQL format 2 atomically upgrades the exact format-1 DDL and representative data. A NOSUPERUSER/NOBYPASSRLS migration owner and injected mid-DDL failure verify data preservation and marker-last rollback.
- The real PostgreSQL TLS/RLS suite passed with competing claims, independent sessions, checkpoint handoff, stale/expired lease rejection, uncertain execution, atomic audit rollback and more than one page of inactive owners.
- Database/application tests: 447 passed, including 7 SQLite runtime-store tests. Documentation/release contracts: 99 passed. Workspace check, Clippy, formatting, dependency and HTTP-construction checks passed after synchronizing onto the preview baseline; all 11 preview-publisher tests passed.
- Materialization is simulated in the storage tests. Remote engine access, current organization authorization, durable child waits/completion consumption and external operation receipts remain outstanding; no enterprise runtime is registered or released.

- Identity PR #183 merged at `5660d000d2f65c9a9d6885d7ba033e2b87900249`; CI 34580934554 passed, including the unified HTTP-construction contract.

## Organization authorization and approval storage

- Runtime PR #184 merged at `abf3e4d2804325800c78fc5a54b0d714d7878ca7`; CI 34584034460 passed, including PostgreSQL 16 on Linux amd64/arm64 and native Windows checks.
- Organization policy separates API applications, automatic read approval applications, and trusted human approval applications. Commands/changes remain HITL; sensitive requests require another assigned human approver.
- `OrganizationStore`/`PostgresOrganizationStore` persist stable operation bindings, decision receipts, expiry/revocation checks and administrator changes with atomic audit/revision updates.
- PostgreSQL format 3 preserves formats 1 and 2. Real old-format fixtures retain native Job checkpoints, budgets, input versions and lease epochs. The isolated TLS suite passes, including concurrent answers/admin requests, application boundaries, changed bindings, Worker handoff, revocation and rollback.
- Policy/application/types/database regression: 564 passed. Documentation/release contracts: 99 passed. After baseline synchronization, workspace check, Clippy, formatting, dependency checks and all 11 preview-publisher tests passed.
- HTTP/BFF, authenticated gateway facts, driver waits/wakeups, operation receipts and distributed execution remain pending. This storage layer does not register enterprise entry points or turn a checked decision into a transferable execution credential.
- Scope review: English/Chinese authorization guides and preview entry links are updated. The stable documentation site and installation remain untouched.

- Authorization PR #187 first CI (34590730300) passed all runtime/database gates but found a Windows ARM fixture race: cancellation could observe an empty PID file. The process-tree fixture now publishes completed PID files atomically. This is test-only; production process supervision is unchanged.
