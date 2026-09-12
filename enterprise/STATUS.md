# Enterprise implementation status

Original baseline: `v0.10.29`, `d1212860dba6a6b420ce81d444ace58feaf0adb5`.

| Phase | State | Evidence required before completion |
| --- | --- | --- |
| P0 | Complete | PR #176 merged into preview; native CI run 34553054820 succeeded |
| P1 | In progress | Principal propagation implemented; ownership migration validated; local bounded driver and scoped session application validated; local runtime Job/lease port validated; Memory persistence/authority ports validated; backend assembly remains |
| P2 | In progress | PostgreSQL, generic OAuth2/OIDC, BFF, scoped private Memory, organization policy and HITL integrated; shared/automatic Memory and full fault acceptance remain |
| P3 | In progress | Independent control/gateway/two-Worker executable loop validated; full fault/operational acceptance remains |
| P4 | In progress | Child dispatch, persistent waits, workspace forks, Job cancellation and durable Workflow implemented; approved merge and distributed Council remain |
| P5 | In progress | Public enums, transactional history/frames, live progress, generated SDK and session Web workbench implemented; remaining Web features and full ACP/TUI adapters remain |
| P6 | Pending | Fault injection, native artifacts, preview publication |

Publication remains disabled until the first runnable root-task preview has
completed its acceptance gates. This file records actual completion, not planned
capabilities.

## Workspace

- Integration branch: `codex/enterprise-preview`.
- Current phase branch: `codex/enterprise-p4-workflow`.
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
- Authorization PR #187 merged at `1afbd60c20af138f5df8751511f78856aaa72702`; repaired CI 34592465239 passed all required native and PostgreSQL checks.

## Shared turn-state boundary

- The ordinary loop now uses an asynchronous `TurnPersistence` provider for history, prompt receipts, model attempts, ordered step/usage commits, tool results, input consumption and driver checkpoints.
- The local adapter preserves existing SQLite semantics and checks session ownership before provider I/O. Provider event/backoff updates are atomic; tools cannot overwrite another invocation or change a settled result.
- Validation: 449 database tests and 400 engine tests passed, including async acknowledgement rejection, retry-window expiry, owner isolation, result-batch rollback and immutable invocation receipts. Documentation/release contracts: 99 passed. Workspace check, Clippy, formatting and diff checks passed.
- Remote transport, PostgreSQL turn records, current lease enforcement and actual Worker/gateway execution are not implemented by this extraction. Publication remains disabled.

## PostgreSQL kernel persistence

- State-port PR #188 merged at `eb86601310493a6991b54bd3a29ef6bedf17eb7c`; CI 34599935025 passed, including Linux/Windows gates and PostgreSQL on Linux amd64/arm64.
- `PostgresTurnPersistence` uses current leases and organization policy for actual shared-kernel state operations. Driver and native Job checkpoints/settlement share one transaction; no separate same-meaning task system is introduced.
- The real TLS suite executes one kernel turn through two Worker lease identities, discards the first caller's committed result, reconstructs its checkpoint from the Job, and verifies one tool call, two provider steps, preserved usage and old-lease rejection.
- Fault injection verifies expiration during state writes, checkpoint/slot rollback and current membership revocation. Formats 1–3 upgrade without rebuilding; a captured format-3 authorization fixture preserves policy, membership, audit, budget and lease data across failed/successful migration.
- Validation: the PostgreSQL 18 TLS/kernel/fault suite passed; database/engine regression 850 passed, CLI rendering/recovery checks 12 passed, and documentation/release contracts 99 passed. Workspace check, Clippy, formatting and diff checks passed. Completed assistant content is immutable in both providers.
- This is database/kernel evidence in one test process. HTTP state transport, independent Workers, configuration/environment resolution, remote steering/attachments/compaction writes and durable distributed waits remain unfinished. No enterprise endpoint or release is enabled.

## Authenticated Worker transport

- PostgreSQL kernel PR #189 merged at `97fd2af22c48375be39412b635de2c42e9868ad0`; CI 34605507014 passed, including all CI-process platforms, Linux/Windows gates and PostgreSQL on both Linux architectures.
- `zuno-worker` implements HTTPS-only claim, renewal and remote turn persistence without a PostgreSQL dependency. Service tokens may rotate through a bounded private-file source; POST retries and redirects are disabled.
- `WorkerAuthority` rejects delegated users and unlisted service identities/apps. Short-lived HMAC grants bind workload, application and the complete execution lease; retained-key rotation is explicit. Database authorization and lease checks remain authoritative.
- The internal router supplies real claim/renew/state handlers. Protocol version/size/record validation is separate from the future public activity protocol. Remote session coordinates do not expose the control-plane directory.
- The isolated script passes real HTTPS/PostgreSQL kernel continuation through two lease identities, certificate/user/redirect refusal, no POST retry and stale-lease rejection. This is one-process network evidence, not independent Worker or Docker execution.
- Validation: 424 engine/identity/Worker tests, 154 Server tests and 99 documentation/release contracts passed. Workspace check, Clippy, formatting and diff checks passed. The normal `zuno-worker` dependency graph contains neither `zuno-postgres` nor `zuno-server`.
- Independent Worker startup, model/configuration/environment assembly, Docker operations, distributed waits, public HTTP/ACP/Web integration and enterprise release acceptance remain incomplete. Publication stays disabled.

## Rootless environment backend

- Worker transport PR #190 merged at `3651fc94f293920616915cde2674c7b308091815`; CI 34610725736 passed, including HTTPS/PostgreSQL on Linux amd64/arm64 and shared native gates.
- `zuno-environment` provides persistent workspace volumes, per-operation Docker containers, a separate single-owner ledger, immutable start admission, late receipt inspection, cancellation, streamed output cursors, verified snapshots, fork isolation and tombstone release.
- `OrganizationOperationAuthority` reuses existing Job and approval stores. The real PostgreSQL contract verifies no execution before HITL approval and rejects changed commands/resources/leases. There is no production allow-all default.
- Local rootless Docker 29.7.1 passes actual read-only-root, network-none, memory/CPU/PID limits, command-once recovery, output paging, cancellation, snapshot/fork and release tests. Ledger/archive tests: 5 passed. PostgreSQL/HTTPS contracts, 99 docs/release contracts, 11 preview publisher tests, workspace check, Clippy, fmt, diff and actionlint passed.
- Preview CI/release requires a separate rootless Docker gate on Linux amd64 and arm64. Both native architectures passed in CI 34622569086.
- Both local validation modes passed: reusing the dedicated task daemon and starting/stopping a fresh isolated rootless daemon. The host needed only `uidmap`/`libsubid4`; the existing rootful Docker service was not restarted. The task daemon uses a separate socket/data/exec directory and the user systemd D-Bus.
- Gateway service authentication, Worker/Agent tool registration, durable HITL waits, watchdog/cumulative quotas, remote artifact storage and interrupted-fork/retention management remain unfinished. These backend tests do not enable an enterprise runtime command or release.

## Durable invocation waiting

- Environment PR #191 merged into preview at `510290b5ff688cb5c1197773c130b32b3281f3bd`; CI 34622569086 passed all gates, including rootless Docker and PostgreSQL on Linux amd64/arm64.
- Typed pending results retain the original call and tool-phase cursor. Completion publication and consumption are separate facts; consumption, original result and next checkpoint commit together without an intermediate started marker.
- SQLite wait replacement/rollback tests passed. The real PostgreSQL suite passed competing claims, independent sessions, early completion, timer wakeup, paused-parent retention, owner isolation, consumption failure/takeover and format-4 migration rollback/preservation.
- Engine/database regression, final dispatch/alias tests, 52 CLI production/permission tests, PostgreSQL/HTTPS wait contracts, 99 documentation/release contracts and 11 preview publishing tests passed. Workspace check, Clippy, fmt and diff checks passed.
- This phase's remote CI remains pending. The complete enterprise runtime, child/Workflow producers, public approval/control APIs and Web remain unregistered; publication stays disabled.
- Main was refreshed at `1518f0ef` (package version `0.10.30`). Its session, permission and automatic Memory fixes will enter preview through a separate synchronization PR; this phase still targets the existing preview baseline.


## Main synchronization candidate

- Wait PR #194 merged into preview at `d179d736f3ac9ec3cfe373339be2ee68e4710875`; CI 34631734711 passed all native gates.
- This synchronization imports main `1518f0ef5448a7016f67e74f9ddd026501b645be`, package version 0.10.30, including questions, foreground scheduling, context usage, input execution receipts and automatic Memory repairs.
- Stable SQLite core format 14 and preview overlay format 1 are independent. Exact unpublished preview 13/14 fixtures migrate without losing owned state. Forty-seven migration tests passed.
- Main context/Memory/foreground tests use the same asynchronous persistence port; provider start and assistant completion retain atomic context accounting. Tool continuation uses a non-billing admission hook.
- Core database/engine/Memory/learning/session-control regression passed 1,226 tests. CLI library and ACP/foreground/serve entry points passed 773 tests; documentation/release contracts passed 100, and preview publishing contracts passed 11.
- PostgreSQL format-6 and authenticated HTTPS tests passed, including the captured format-5 migration, context CAS/start rollback and input applied/completed receipts. Workspace check, Clippy, formatting and diff checks passed. Remote synchronization CI remains pending.
- The preview remains disabled. With this incorporated stable baseline, the first version request will use 0.10.31-preview.1, subject to later main synchronization and runnable acceptance.

- Synchronization PR #195 first CI (34643334370) passed Linux, PostgreSQL and Docker gates but exposed a Windows clipboard fixture timing race. A 300ms caller-start delay reproduces its 250ms harness watchdog failure. The fixture now synchronizes caller startup, waits for the intended hostile branch, and releases its fake blocked threads afterward; 51 focused tests pass. Production clipboard deadlines are unchanged. Repaired native CI is pending.

## Browser login implementation

- Main synchronization PR #195 merged at `410faf43f089228a03ea50138d79cb2ac06c594e`; repaired CI `34646255325` passed all gates. The merge retains main ancestry.
- The generic OIDC BFF now composes code/PKCE, nonce/ID-token validation, the existing API verifier, encrypted one-time transactions and opaque browser sessions. Exact issuer-native OAuth client binding remains distinct from the namespaced application identity used by organization policy.
- PostgreSQL format 7 adds tenant-bound browser state and audit. The isolated PostgreSQL 18 suite passed capacity, competing consumption, wrong binding/tenant, expiry, user separation, logout/audit rollback and captured format-6 migration rollback/preservation.
- Real HTTPS code exchange and two independent BFF instances passed with a signed generic OIDC issuer fixture. The browser does not receive OAuth tokens, and token POSTs do not redirect or replay. This is fixture/network evidence, not a live Entra tenant or rendered Web application.
- Focused failing tests exposed cross-client acceptance, comparison of raw versus namespaced client IDs, and an unbound callback overwriting the active login cookie. All three have passing regressions; red/green evidence is retained in the task's ignored validation directory.
- Validation passed: 215 identity/HTTP tests; isolated PostgreSQL and two HTTPS contracts; 100 documentation/release tests; 11 preview publisher tests; workspace check, Clippy, formatting and diff checks. Remote CI for this phase remains pending.
- Documentation impact: both browser guides, identity/PostgreSQL/wait guides, preview navigation and both workspace inventories are updated. Preview docs remain tag artifacts; the stable site is not published by this branch.
- Complete enterprise launch, current resource authorization integration, distributed tool execution, Memory backend, Workflow/Council and Web remain outstanding. No enterprise CLI entry point or release is enabled.

## Stable 0.10.31 synchronization

- Main `5619205d60aef572e484646dd9ab0563e1ef8066` adds only the 0.10.31 release-version update to the previously imported source. The merge keeps all preview crates and authentication changes; every first-party package advances together and third-party lock entries remain unchanged.
- `preview.json` records that exact source baseline, with publication disabled. The next eligible first preview becomes `0.10.32-preview.1`, subject to later synchronization and runtime acceptance.
- Workspace check, Clippy, 100 documentation/release tests and preview channel validation passed. This synchronization is prepared separately from browser PR #196, which remains in CI.

## Approval continuation

- Browser PR #196 merged at `93c46ef3fd0742cfc9cd61edfd0a2946694d8d7a`; CI `34652125193` passed every gate. Stable-version synchronization PR #197 uses `c490e257df7442ab26946e8f90e35133643b6ec0` and remains in CI.
- Typed wait outcomes distinguish actual tool results from approval readiness. The latter preserves the original pending part and provider metadata, restores only the undispatched preparation cursor, and does not spend a tool execution or issue another model request.
- PostgreSQL commits approval answers, decision receipts, readiness facts and wakeups in one transaction. Registration observes an earlier answer; duplicate answers/facts do not duplicate consumption. Legacy result facts remain immutable and readable; ambiguous legacy approval-as-result facts fail closed.
- Two focused failing tests demonstrated premature tool completion and a missing approval wakeup before their fixes. The engine dispatch suite and real PostgreSQL/HTTPS contracts now pass, including a full kernel continuation through three lease identities, failed wakeup rollback, original-result consumption, unchanged earlier effects and preserved budgets.
- Validation passed: 456 engine tests, native PostgreSQL and both HTTPS contracts, 100 documentation/release tests, workspace check, Clippy, formatting and diff checks.
- Public approval endpoints and production gateway/Worker dispatch remain pending. The enterprise runtime and preview release remain disabled.

## Authenticated gateway transport

- Approval continuation PR #198 merged at `6d8b54f71554c6d4be1f7c7b4ad04880369821ec`; CI `34656279425` passed every gate.
- A separately authenticated gateway redeems request-scoped, short-lived tickets from the control plane. Tickets bind Worker identity, lease, gateway and request; actual execution still checks current organization policy, assignment and operation approval.
- The Worker client, control-plane routes and Docker execution router now implement bounded typed acquire/get/prepare/submit/inspect/output requests. Gateway credentials contain no database or model access. Configuration resolution requires the Job's exact installed snapshot.
- The real PostgreSQL/TLS/Docker fixture passed HITL refusal/approval, request/service/environment boundaries, output recovery, stale revision rejection and expired-lease refusal. A separately approved file read verifies one actual execution. Fixture corrections included the exact lease column, private directory mode and current environment revision.
- The Docker runner now requires the combined gateway execution fixture on Linux amd64/arm64. Local validation passed 218 related tests, the complete rootless Docker/PostgreSQL/HTTPS runner, 100 documentation/release tests, 11 preview publisher tests, workspace check, Clippy, formatting, diff and actionlint checks. Worker normal dependencies still exclude PostgreSQL and the server crate. Remote CI remains pending.
- Independent Worker/role startup, production Agent tool assembly, public application/approval APIs, cancellation propagation, Memory, Workflow/Council and Web remain outstanding. Preview publication remains disabled.

## Gateway CI watcher follow-up

PR #199 CI `34659258061` passed Linux, PostgreSQL and both Docker architectures.
Windows reported all 1,000 burst paths without loss, but one path legitimately
arrived in two debounce windows. The old test incorrectly required one total
event per path across all windows. A controlled later write reproduced the
1,001-event failure.

The native fixture now checks full coverage, bounded storage, coalescing and
preservation of a later-window update. A deterministic 1,000-path test separately
requires exactly one event per path within a window. Production watcher timing
and delivery behavior are unchanged. Documentation impact is limited to this
test-contract explanation; no user configuration or behavior changed.

## Durable external operation results

- Gateway PR #199 repaired CI `34661231045` passed all gates and merged at `5fe39c9d`. The user's Linux-enterprise/personal-Windows boundary is implemented separately in platform PR #200.
- Gateway ledger format 2 persists pending/captured/acknowledged results and refuses environment cleanup before acknowledgement. PostgreSQL format 8 atomically records execution admission with approval checks, preserves admitted attempts and accepts matching late terminal facts after a Worker lease expires.
- Verified results publish matching operation-wait facts through the existing runtime. Receipt, event and wakeup share one transaction; parent consumption remains separate. Changed output, wrong gateways and unadmitted attempts are rejected.
- The full rootless Docker/PostgreSQL/HTTPS runner passed, including simulated response loss after control-plane commit, retry without duplicate events, old-lease receipts and both old-format migration/rollback fixtures. Workspace check and Clippy passed. Captured output is bounded and explicitly truncated.
- Documentation/release checks, preview contracts, formatting and diff checks passed. Additional PostgreSQL tests verify early completion, receipt/readiness rollback and exactly one wait-completion fact.
- Full Worker/command-dispatch consumption and a production delivery supervisor remain pending. This does not enable an enterprise runtime command or release.

## Platform boundary amendment

The user clarified that enterprise services require only Linux amd64/arm64;
personal TUI/ACP and their shared runtime retain Windows support. Enterprise
server modules and dependencies now require an explicit feature. Personal
Windows CI selects the non-enterprise workspace surface and enterprise-only PR
paths may skip Windows; shared or unknown changes remain conservative. The
two-Linux-target preview artifact matrix remains unchanged. Remote CI is pending.
Local verification passed: personal dependency graph (including build/dev edges),
personal workspace check/Clippy, default and enterprise server builds, explicit
enterprise PostgreSQL/HTTPS tests, 100 documentation/release tests, Python CI
contracts, preview publisher tests, actionlint, formatting and diff checks.
Native Windows validation will run on this shared-manifest change; enterprise-only
changes can then skip that personal regression according to the path classifier.

PR #200's first Windows scheduler run exposed CRLF translation in the Python-to-Bash
argument stream (`--workspace` carried a trailing carriage return). A simulated
Windows stdout regression reproduced the exact bytes before the fix. The selector
now emits the argument protocol through binary stdout with LF delimiters; Python
CI tests and personal dependency verification pass. Native CI is being rerun.

## Bounded Worker execution

- Platform PR #200 passed CI `34663672877` and merged at `27df523e71d4da6c053f37dab849e8ded842bb3c`. Operation-result PR #201 passed CI `34665036201` and merged at `63b1930fcc8a1c8074411dbc2fe5ea242cba77b5`.
- `WorkerRuntime` composes the shared bounded driver with compatible configuration claims, bounded slots, monotonic grant lifetimes, renewal during initialization and execution, and bounded drain. Worker protocol 4 rejects old claims before acquiring work and carries the input's stable admission timestamp.
- A focused real HTTPS/PostgreSQL regression reproduced loss of the initial lease during configuration loading. Renewal now covers the complete advance preparation. A second regression showed the missing configuration-routing contract; the claim query now leaves mismatched definitions for a compatible Worker.
- Two runtime instances exercise the same kernel without duplicate input, provider requests or tool execution. Fault injection rejects renewal before dispatch, and the finish route refuses optimistic success. This remains same-process integration evidence; standalone enterprise services and root-tool assembly are still required.
- A retained driver event sender reproduced a false lease-loss report after a committed checkpoint. The host now uses nonblocking observations and a bounded final event drain, independent of sender/profile lifetime.
- Documentation impact: both Worker guides, PostgreSQL/wait guides and preview navigation are updated. Preview documents remain isolated tag artifacts, and no stable site publication or release is enabled.

PR #202's amd64 Docker gate exposed renewal racing the final checkpoint response.
A controlled 500ms delay after checkpoint commit reproduced the false lease-loss
report locally. Claim-local serialization now protects the boundary POST and
retires renewal only after its validated acknowledgement. The controlled
PostgreSQL/HTTPS regression passes; the native CI rerun remains required.

The rerun also exercised a checkpoint response crossing the old grant deadline.
A 1.5s post-commit delay under a 1s lease reproduced `LeaseExpired` while the Job
was already `Completed`. Final submission now closes new state/gateway admission
and permits only a bounded acknowledgement wait. The target regression passes
without extending database execution authority or replaying the final POST.

## Public enterprise application

- `EnterpriseApplication` connects verified delegated-user API tokens and BFF cookies to shared session creation/listing, atomic Job admission, public Job/input-version reads and policy-checked approval decisions. Public DTOs exclude Worker credentials, leases, configuration and private replay checkpoints.
- PostgreSQL user-facing transactions now hold current organization policy/member checks through commit. A targeted old-revision/revoked-member regression failed before this change; refused session/input/Job actions now leave no new session facts.
- Optional Agent/model selection is admitted atomically and included in request deduplication by both SQLite and PostgreSQL. Omission preserves existing session selection and old request digests. A focused SQLite regression reproduced ignored explicit selection before the implementation.
- Real PostgreSQL/HTTPS fixtures cover two users, CAS/idempotency, configured input, cross-owner refusal, approval application restrictions, revocation and BFF application access. Standalone enterprise processes, production Worker/tool assembly, activity projection and remaining P3–P6 acceptance are still outstanding.
- Documentation impact: both application guides, identity/state/wait/runtime references and preview navigation are updated. Publication remains disabled and personal platform commitments remain unchanged.

PR #203's shared Windows arm64 CI-tool test found a transient stderr sharing
violation after cancellation. Ignored temporary-directory cleanup hid the leftover
capture until its parent's cleanup failed. An injected sharing violation reproduced
the leftover directory. Cleanup now retries only sharing violations within the
existing cleanup budget and reports a persistent failure instead of success.
All 41 local CI-tool tests pass (three platform-specific tests skipped); native
Windows rerun remains required. Both pipeline guides document the behavior.

## Independent executable roles and gateway tools

- Worker PR #202 passed CI `34668613673` and merged at `4f47b2ade957cc2da68ab510812f3d5b6abd36a9`. Application PR #203 passed CI `34669238534` and merged at `cee0f3fed19c05d44d79d9816fe65646dddd1eb0`.
- `DeferredExecution` now records tool handoff before submitting an external effect. Checkpoint schema 4 distinguishes unsubmitted approval waits from submitted operation waits; schema 3 is retained as unsubmitted state. Worker protocol 5 rejects incompatible claimants before acquiring work.
- Focused failing tests demonstrated a missing handoff and failed schema-3 takeover. The repaired tests verify submitted result consumption without replay, refusal of approval-after-submission, unbounded-driver refusal, legacy takeover, and unchanged budgets.
- `GatewayToolDispatcher` provides the explicit argv `environment_command` capability, preserving the existing personal shell contract. A shared-kernel/HTTPS/Docker test crosses approval, command submission, receipt delivery and five Worker claims with one actual command execution.
- The new `zuno-enterprise` executable composes real control-plane, Worker, gateway, migration and identity roles. Definitions are immutable, credentials are separate, models reuse native factories, the gateway has a receipt supervisor, and service shutdown drains bounded work.
- A separate executable test starts one control plane, one gateway and two Worker processes against real TLS/RSA, the native compatible model transport, PostgreSQL and rootless Docker. Two users complete separate approvals and Jobs, both Workers participate, and exactly two command operations are recorded. The final native run also passed SIGTERM draining for all four processes.
- Public Job views now expose safe typed wait targets so clients can discover approval IDs. They still omit grants, checkpoints and private replay material. Full final-message/activity projection, workspace provisioning, enterprise Memory, distributed children/Workflow/Council, React/ACP and P6 acceptance remain outstanding.
- Documentation impact: both deployment guides and typed templates, Worker/wait/state/application/runtime references and workspace inventories are updated. No release is enabled; documentation remains an isolated preview artifact.
- Validation passed: enterprise configuration/budget tests, shared dispatch tests, real Docker/PostgreSQL/HTTPS/independent-process contracts, workspace check and Clippy, 100 documentation/release contracts, CI-tool and preview publisher tests, personal dependency isolation, formatting and diff checks. The explicit crate roster and both inventories now contain 55 crates. Remote CI remains required before integration.

PR #204's independent-process CI reported uncertain Jobs on both Linux
architectures. A controlled real-Docker regression reproduced receipt observation
changing an active `Starting` operation to `Uncertain` before Docker start
completed. Direct inspection now waits for the environment's submit boundary;
background delivery skips it and continues other work. Observation refreshes the
ledger under that boundary, preserving conservative recovery after a restart.

Repeated native validation exposed a second cause: the authority returned an
extended lease after renewal, while the gateway compared the entire lease for
equality and rejected it. A deterministic renewal-between-authorization test
reproduced `Forbidden`. The client now accepts only a non-shortening expiry with
the same complete execution identity. The executable fixture renews every 100ms
to exercise this boundary directly; persistent diagnostics retain ledger, Docker
and driver facts if a future failure occurs.

CI run 34675496698 passed both Linux Docker lanes, including the independent
process fixture. The personal Windows test job timed out in `goal_uncertain`
after its first CLI launch; the suite's captured log had no child diagnostics.
The fixture now captures finite CLI output into files, bounds process waiting
independently of pipe EOF and reports child diagnostics before a timeout panic.
The Linux Goal test, workspace check and Clippy passed. Windows verification of
this follow-up remains pending; the earlier timeout is not counted as a pass.

CI run 34676688599 subsequently passed all gates at `9d558b7b360289dc9d6a428bc4f3ee855d573ac7`, including personal Windows and both Linux Docker lanes. PR #204 merged only into `codex/enterprise-preview` as `21ddba66790c00868ab0ac0d3beb1bf05576a727`; no release was created.

## Logical Memory storage boundary

- `MemoryService` now supports validated logical document keys independently of optional local file projection. Shared candidate/revision/evidence/undo behavior is preserved; logical mode refuses file imports and never treats a document key as a host path.
- The persistence contract uses Memory domain errors. External backends can report denial, conflict and temporary unavailability directly while the SQLite provider preserves its existing database errors. Learning retry classification remains typed.
- A focused regression first failed because logical storage required local paths. It now passes proposal/apply/snapshot/undo and foreign-document refusal with no projection; local Memory and automatic-learning regressions also pass.
- PostgreSQL persistence, organization authorization, scope-wide caches and enterprise maintenance producers still require implementation. This foundation is not an enabled enterprise Memory feature.

## PostgreSQL private Memory and Worker integration

- Preview PostgreSQL format 9 installs scoped Memory policy/documents/candidates/revisions/evidence/provenance/retirement, learning Jobs, maintenance watermarks, request receipts and audit with forced RLS. The exact format-8 fixture first failed at the missing upgrade boundary, then passed rollback/preservation with the real migration.
- The shared `MemoryService` now runs against a private PostgreSQL provider inside one bounded data-owner transaction. Public API/BFF and authenticated Worker calls share its capacity and state; commands cannot select another owner or document path.
- Private generation defaults off. Explicit user consent controls foreground model updates and learning commits; session overrides narrow it. Scope, CAS, request replay, audit rollback, source validity, independent support, retraction, learning lease expiry and unscoped RLS reads have real PostgreSQL coverage.
- A timed SQL write reproduced early capacity release while a cancelled transaction still ran. The data owner now awaits bounded explicit rollback before releasing its slot; the same real PostgreSQL regression passes without a candidate or receipt being committed.
- A separate authorization regression reproduced a normal allowed API app granting Memory consent. Consent and manual review now require the current organization approval-app allowlist; ordinary API apps may read and stage candidates without granting execution or Memory authority.
- Manual candidate review carries a state digest covering the exact record and owner. A regression reproduced applying an edited proposal through a stale review; apply/edit/reject/undo now compare the reviewed digest before mutation, while identical committed request retries retain their original receipt.
- Worker protocol 6 installs real async Memory read/update transport and a context refresher. Typed refresh failures preserve recovery classes and prevent model requests with stale state. The personal tool schema is reused; a missing confirmation around an update retains uncertainty.
- The full rootless-Docker fixture passed with four real executable processes, two users, eight provider requests, private Memory read/update, refresh before the next request, revocation during command-approval waits and checkpoint continuation. Both Workers participated; each approved command ran once. This is local Linux evidence; native arm64 CI and release acceptance remain required.
- Preview documentation packaging now includes tracked guides, example configurations and licenses from the certified Git commit. A regression reproduced the old four-file-only archive; the corrected archive excludes untracked files and local edits.
- Background extraction producers, organization-shared Memory/approval, distributed children/Workflow/Council, richer workspaces, public transcript/live projection, React/ACP clients and remaining P6 acceptance are not complete. Publication remains disabled.

## P4 child storage boundary in progress

- A real PostgreSQL regression confirmed that the original root-only foreign key could not retain a parent session while executing in a separate child session.
- The format-10 migration separates these bindings and keeps the native Job identity. The real PostgreSQL suite passed the child execution-slot regression and frozen format-9 rollback/preservation test, including private Memory; all five authenticated HTTP suites also passed.
- Child dispatch/host integration, completion production and consumption, background delivery, Workflow/Council and environment merge remain unregistered pending their complete implementations and tests.

## P4 native child transactions and host seam

- Added scoped child intent/definition grants, idempotent staging, atomic foreground activation with the parent checkpoint, immediate background admission, native completion envelopes and a durable parent outbox. Next-step report admission reuses the root transaction; quiet and stopped-parent behavior retain their boundaries.
- Real PostgreSQL tests passed admission rollback, separate child slots, duplicate notifications/consumption and Worker replacement. The HTTPS fixture runs the native TaskTool through another child Worker and replacement parent, with one persisted original result and no extra model requests.
- ChildTurnHost now distinguishes a returned result from Pending. A focused test rejected the earlier foreground-running result. Two HTTP regressions exposed the user-only input materializer and missing resumable child identity; both were fixed and the complete HTTP fixture passed.
- Source-aware materialization checks delegation and completion records. Real regressions now pass for retiring unadmitted intents after parent cancellation and refusing two staged resumptions of the same child session. Task unit/integration, personal child-host, PostgreSQL/HTTPS and workspace Clippy checks passed.
- Standalone executable target catalog, workspace snapshot/fork, model-option binding, full cancellation/merge and Workflow/Council assembly remain pending; the executable does not yet advertise remote task.

## P4 executable child workspace assembly

- PR #208 passed CI 34682562179 at `2ed8158b142cdcd4abf41382c0f79f37bd60d1a4` and merged only into preview as `f2315ddf60bde4c13ebbc25bf6e9aa2d5755caec`.
- Gateway ledger 3 records fork intent and nonce ownership before Docker writes. Exact retries preserve later child changes; interrupted unpublished copies cannot be acquired, and another ledger's similarly named volume is refused. Real fork retry/restart and failed-publication tests passed.
- PostgreSQL format 11 gates child activation on an assigned gateway's immutable workspace receipt. Parent/child identities, current lease, source/target specs and receipt deduplication are checked. Truthful late receipts do not restore expired authority. Exact format-10 migration preserves prior rows and explicitly backfills inherited depth under RLS.
- Worker protocol 7 and gateway protocol 2 expose only preparation of a server-resolved staged child. Executable definitions with pinned child digests now mount native task and prepare parent snapshots/child forks. `--definition-ref` produces validated references without starting services or reading credentials.
- The real four-process test passed two users, parent/child jobs and workspaces, fourteen model requests, four human command approvals, Memory isolation/revocation and SIGTERM. Child changes leave the parent intact. Snapshot/fork currently requires one assigned Docker gateway; cross-gateway transfer, approved merge, full cancellation and Workflow/Council remain outstanding.

## Durable task-tree cancellation

- Workspace PR #211 (head `4ebc99c29c44edb1660c721d7e62af5be0176fd8`) is running native preview CI.
- Authenticated API/BFF cancellation fences active descendants and old Worker leases atomically, preserves completed/uncertain evidence and suppresses late nextStep wakeups.
- Gateway service cancellation survives lease revocation and uses immutable admissions, fair durable polling and the existing terminal receipt acknowledgement path.
- PostgreSQL format 12 preserves exact format-11 fixtures and rolls back failed DDL atomically. Native Docker/PostgreSQL/HTTPS/independent-role tests pass. Follow-up PostgreSQL tests cover rollback, competing cancellation, previously admitted automatic continuation and bounded/fair gateway batches. Final workspace check, Clippy, docs, formatting and diff gates pass.
- English/Chinese control, application, persistence and runtime docs are updated. Only preview docs archives are affected. Publication remains disabled.

## Public activity and generated client contracts

- Workspace PR #211 merged at `34c18f25` with CI `34685893939` green. Cancellation PR #212 merged at `e66465ae346f62d69d5d4cc0763cb510432e1353` with CI `34686887132` green.
- Six public enum groups are implemented independently of Worker/gateway DTOs. Real adapters supply invocation action/source; the kernel persists it. Public protocol 1 and Worker protocol 8 are distinct; compatible legacy display omissions do not widen execution declarations.
- PostgreSQL format 13 writes bounded public items and immutable frames atomically with original state. Fixed-snapshot history, contiguous replay, immediate queued-input display, capsule deduplication and logical positions are implemented behind authenticated API/BFF handlers. Message-to-Job association prevents cross-turn reuse of approval/wait/operation state; approval invalidation and lease loss update the public state after authoritative changes.
- Rust generates Schema, TypeScript and bundled standalone validators. Ten SDK recovery/security tests and generation-drift checks pass. Native Docker/PostgreSQL/HTTPS/independent-role tests and additional history/migration/rollback contracts pass; final workspace check, Clippy, docs, formatting, workflow and diff gates pass.
- One broad test build stopped because the shared filesystem filled. The task-owned incremental cache was removed after verifying no compiler remained active; source, binaries and evidence were preserved. Subsequent local verification disables incremental caching.
- LiveFrame production, React Web, full ACP/TUI integration, P4 approved merge/Workflow/Council and remaining enterprise acceptance continue. Publication remains disabled. English/Chinese guides and preview navigation are updated; stable docs deployment remains outside this preview branch.

## Live progress transport

- Activity PR #213 merged at `4acc059175137d109954c38fa31dded44b6307d0`; CI `34690283755` passed all required gates including generated client contracts.
- Worker-side coalescing, private authenticated writes, active-only public snapshot reads and SDK generation handling are implemented. Original message/Job identity, lease and sequence fence publication. Checkpoints/retries/execution changes clear drafts; no encrypted fields or authoritative usage enter live frames.
- PostgreSQL format 14 has exact format-13 migration/rollback coverage. PostgreSQL/HTTPS and 12 SDK tests pass, including observation before provider completion. Final native Docker/PostgreSQL/HTTPS/independent-process, workspace check/Clippy, docs/release-surface and formatting gates pass.
- React Web, full ACP/TUI, approved workspace merge, Workflow/Council, remaining Memory producers and P6 work continue. Preview publication remains disabled.

## React session workbench

- Live progress PR #214 passed CI 34691457384 and merged only into preview at `8806d2a67d0e29f28396dee09dffa0d08e52407c`.
- Public application DTOs now generate Schema/TypeScript/standalone validators. The SDK adds actual session, admission-receipt, Job/control and approval methods. Older history remains readable inside a bounded window while live cursors advance.
- React implements owned sessions, input admission, folded thinking, typed invocation details, explicit approvals, reconnect recovery, focus-managed mobile drawers, dark mode and reduced motion. Uncertain submissions retain their original input and ID; stale account/session responses cannot replace current content.
- Control-plane `webAssetsDirectory` serves a bounded immutable bundle only with OIDC configured. JSON login navigation and verified browser-context binding preserve the BFF authority. The preview workflow packages the client job's tested bundle with both native Linux binaries.
- Seven Chromium fixture scenarios and seventeen SDK tests pass. Real TLS/PostgreSQL/Docker/four-process verification also passes with Chromium login, private history, explicit command approval and continued model output. The identity/model issuer is a fixture. Workspace check/Clippy, formatting, 100 documentation/release contracts and preview workflow checks pass. Remote preview CI remains required before integration.
- The linked Figma workbench uses existing design-system controls and was compared with the rendered browser. Temporary capture content was removed. EN/ZH Web/deployment/auth/API guides and preview entry links are updated; generated client contracts are committed together. Stable docs publication does not apply to this preview-only delivery.
- Remaining Web management/run-graph/Memory features, full ACP/TUI, approved child merge, Workflow/Council, remaining Memory producers and P6 remain active. No preview release is enabled by this batch.

## Durable Workflow backend

- Web PR #216 merged only into preview at `b1c9e9af70076e8490284b2cd42eb101270aef8b` after CI 34695923551. The user subsequently paused UI delivery and requires Penpot App design first. Experimental UI changes remain separate from this backend batch.
- Shared DAG decisions preserve logical waiting capacity and refill independent branches. Node dependency outputs are sealed with their source digests into actual input; local and enterprise hosts share the renderer. Workflow/Council host ports preserve typed Pending and uncertain results.
- PostgreSQL format 15 coordinates native child Jobs without allocating a Worker attempt to the group. Foreground activation, original wait and checkpoint commit atomically; callbacks and completion consumption remain deduplicated. Cancellation/revocation use existing tree fencing and gateway stop delivery.
- Worker protocol 9 and gateway protocol 3 support validated group workspace preparation. Ordinary commands remain bound to their execution session. Node workspace preparation rejects another parent, changed source and expired lease. The exact format-14 migration fixture retains session/message/Memory/activity/live values and verifies rollback.
- Public Workflow/node DTOs and the stable ViewWorkflow action expose no private plans, leases or credentials. SDK validation and response identity checks pass. App UI implementation is deferred under the updated plan.
- Final local validation passed: native rootless Docker/PostgreSQL/five HTTPS/independent control+gateway+two-Worker tests, 37 shared dispatch tests, local Workflow/Council tests, enterprise catalog tests, workspace check/Clippy, 100 documentation/release tests, 12 preview publisher tests, SDK 18 tests, generated-contract drift, formatting and diff checks. Native DAG validation withheld a slow approval until a dependent branch refilled, completed four explicitly approved commands and retained ordered outputs. Fixture providers are used.
- Remote exact-head CI remains required before integration. Distributed Council, approved merge, remaining Memory/ACP/operational work and the full P0–P6 objective remain unfinished. Preview publication stays disabled.

Workflow PR #218 initially exposed an arm64 stack overflow in the PostgreSQL contract. The shared turn-loop and optional coordination futures now have heap boundaries, as do independent test suites. Local PostgreSQL and all five HTTPS cases pass with 1.5 MiB test-thread stacks; CI retains its default stack limit. This is an allocation/layout fix with no API, database, permission or configuration change. The native arm64 rerun remains required.
