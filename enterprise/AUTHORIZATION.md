# Organization authorization and durable approvals

The identity adapter verifies who called the API. Organization policy, resource
access, human approval and execution authority remain separate checks.
`zuno-permission::enterprise` owns the policy evaluator;
`zuno-application::OrganizationAuthority` publishes the storage port;
`PostgresOrganizationStore` provides atomic state-owner operations.

This is a storage/service implementation. HTTP/BFF integration, gateway resource
resolution, durable driver waits and wakeups are still required. It does not
register a runnable enterprise server, gateway or approval endpoint.

## Three application lists

- `allowedApps` controls organization API admission after authentication.
- `autoReadApps` is a subset eligible for minimal native file/Memory reads.
- `approvalApps` identifies trusted applications that may submit human decisions
  or administrator changes, normally the owned BFF.

A regular API application cannot approve its own requests using a delegated user
token. Commands, changes, network effects and external tools still require HITL,
even when the calling app is on the automatic-read list. Remote MCP annotations
and UI presentation are not permission evidence.

Resource denial, explicit rules and unavailable/unenforced file/process isolation
are hard denials. Ordinary operations require the requester; sensitive operations
and automation requests require a different current human Approver/Administrator.
Administrators cannot bypass isolation or approve their own sensitive request.

The evaluator consumes trusted `PreparedEffectFacts`, which intentionally has no
wire deserializer. Gateways/registered handlers must resolve actual resource
access, versions and isolation; Worker or model-supplied booleans are not facts.

## Binding and execution

Each approval binds the native Job/session/turn, logical invocation/operation,
argument digest, resolved resource/version digest, requester and policy revision.
Worker attempts and epochs are not part of this stable approval binding.

Admission validates the current session lease and organization state, then commits
the decision and event together. Repeating an identical operation reuses its
approval; changed arguments/resources under that operation ID conflict. Human
answers require current membership, a trusted approval app, matching policy,
correct audience and an unexpired pending request. Answers and idempotency
receipts commit with their audit event. Concurrent duplicate answers write once.

Before an external operation, the authenticated gateway calls `check_execution`
with its current facts. The state owner rechecks the binding, current requester
and approver rights, policy, approval expiry and lease. It uses the database's
lease deadline, not the deadline copied into a request. A valid approval survives
Worker replacement; it cannot authorize a stale lease or changed operation.

`CheckedApproval` is a checked state-service response, not a bearer execution
credential. A gateway must not accept a Worker-supplied copy as proof. Transport
service authentication and operation admission/receipts remain part of gateway
integration.

Expired, rejected and invalidated decisions do not silently become new grants.
An expired request currently requires a newly prepared invocation after user
input; no automatic approval-renewal API is advertised. The store records answers
without independently resuming a paused or cancelled Job. Durable wait linkage,
wakeup and original tool-result consumption belong to the driver integration.

## Setup and administration

`bootstrap_organization` requires the schema-owner database credential and creates
an organization once. Re-running it never restores a revoked administrator or
overwrites policy. Supply the administrator's stable identity explicitly; the
first browser visitor is never made an administrator.

`update_member` and `update_policy` require an active human administrator using
an approval application. They use expected-revision CAS and commit membership or
policy, the new revision, request receipt and organization audit together. Audit
failure rolls the change back. A duplicate request returns its original revision
without applying the change twice. Revoked actors cannot use receipt replay to
regain access.

Organization policy/audit have tenant RLS; members, approvals and answer receipts
have owner RLS. End users and Workers have no SQL credentials. Public controllers
must authenticate first and enforce these service methods; a serialized scope or
lease is not authentication. A fixed, read-only helper resolves an opaque approval
ID to routing coordinates without exposing parameters. PUBLIC execution is denied.

## Persistence and verification

PostgreSQL preview format 3 preserves formats 1 and 2 with guarded atomic forward
migration. Fixtures cover original session/input/event/receipt rows and a native
Job checkpoint with budget values, input version, attempt history and lease epoch.
The marker is written after DDL, backfills, constraints and permissions.

Run `python3 scripts/check_enterprise_postgres.py` for real verified TLS/RLS tests
on isolated databases. It exercises atomic admin changes, duplicate answers,
application boundaries, designated approval, expiry/revocation, changed bindings,
Worker handoff and both old-format migrations. Policy tests run with
`cargo test -p zuno-permission`.

See [identity adapters](AUTHENTICATION.md), [PostgreSQL](POSTGRES.md),
[中文](AUTHORIZATION.zh.md) and [implementation status](STATUS.md).
