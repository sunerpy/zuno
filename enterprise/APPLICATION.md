# Enterprise application API

`EnterpriseApplication` assembles session, Job and approval handlers over
`AgentApplication`, `JobDispatcher` and the PostgreSQL organization store. The
host installs logical workspaces with an immutable configuration reference and
validated Agent/model selection. Requests cannot choose an owner, private
directory, Worker lease or configuration snapshot.

External delegated-user clients use `/api/v1` with a verified API access token.
Cookies are not credentials there. Browsers use `/app/api/v1` through
`EnterpriseBrowser::authenticate_routes`, HttpOnly cookies, an exact Origin and
`x-zuno-csrf: 1` on writes. Internal Worker/gateway authentication stays separate.
ID tokens and ACP permission replies cannot authorize application operations.

A valid login does not enroll a user in the organization. PostgreSQL session
operations, input/Job admission and public Job/version reads recheck membership,
policy revision, actor kind and application in their data transaction. Policy and
member locks remain held through commit, preventing a revocation race. Organization
bootstrap remains an explicit schema-owner operation.

| Method and path, relative to either prefix | Result |
| --- | --- |
| `GET /workspaces` | Installed workspace IDs and titles |
| `POST /sessions` | Idempotent owned session creation |
| `GET /sessions` | Bounded owned-session pagination |
| `GET /sessions/{session}` | Owned session summary |
| `GET /sessions/{session}/history` | Bounded public history at a fixed snapshot |
| `GET /sessions/{session}/frames` | Contiguous committed activity after a cursor |
| `GET /sessions/{session}/live` | Current authorized transient snapshot |
| `GET /sessions/{session}/input-version` | Exact CAS version |
| `POST /sessions/{session}/turns` | Atomic input and Job admission |
| `GET /sessions/{session}/requests/{request}` | Original authorized admission receipt after an uncertain response |
| `GET /jobs/{job}` | Public Job identity, phase and input version |
| `GET /jobs/{job}/workflow` | Authorized Workflow state, typed nodes, dependencies and public waits |
| `POST /jobs/{job}/cancel` | Durable tree cancellation, old-lease fencing and operation stop intent |
| `GET /approvals/{approval}` | Authorized approval presentation |
| `POST /approvals/{approval}/answer` | Idempotent human decision |
| `POST /workspaces/{workspace}/memory` | Typed private Memory requests, when the backend is installed |

Turn requests contain `requestId`, `expectedInputVersion` and `text`. Versions are
canonical decimal strings to preserve JavaScript precision. Repeating an identical
request retrieves its admission; stale new requests and changed replays conflict.
Agent/model selection commits with the input, Job and events and participates in
deduplication. SQLite and PostgreSQL share that contract. Omitting a selection
retains the session selection and older request digests. Admission does not mean
that a provider has already applied the input.

Session paging accepts `limit` (1–100) and an optional complete pair
`beforeUpdatedAt`/`beforeSessionId`. Foreign resources return not-found. Unknown
fields, owner overrides and invalid IDs fail before resource mutation. Responses
are not cacheable.

Approval answers contain `requestId` and `answer` (`approve` or `reject`). The
existing atomic service checks current role, requester/designated-approver audience,
expiry, policy and the trusted approval application. Ordinary API clients and ACP
bridges must not join the approval-app allowlist merely to suppress HITL. A decision
is neither tool completion nor a Worker credential.

Public Job DTOs expose typed waiting targets so a client can discover its approval
ID. They contain no checkpoints, leases, grants, configuration, private
replay blocks or arbitrary stored results. Public history and frame reads use the
separate [activity protocol](ACTIVITY.md). Live stream endpoints are not mounted
before their producer and authorization adapter exist.

Cancellation accepts `requestId`, `expectedTurnId` and a bounded `reason`.
Receipts distinguish logical stopped Jobs from pending external operations.
Public Job reads include `stopRequested` and current `pendingOperations`;
see [durable cancellation](CONTROL.md) for races, restart and completion semantics.

Run `python3 scripts/check_enterprise_postgres.py` for real TLS/PostgreSQL
verification: two users, CAS/replay conflicts, configured input, foreign-resource
denial, approval restrictions and revocation. The BFF fixture uses real application
routes across two replicas, including CSRF and logout. SQLite tests verify selected
input and changed replay refusal.

Standalone roles and gateway command assembly are documented in [deployment](DEPLOYMENT.md).
Full P3–P6 fault acceptance is still required. Publication remains
disabled; personal HTTP/TUI/ACP platform behavior is unchanged.

See [中文](APPLICATION.zh.md), [authorization](AUTHORIZATION.md),
[browser login](BROWSER.md), [Memory](MEMORY.md) and [status](STATUS.md).
