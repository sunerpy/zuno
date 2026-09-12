# Authenticated execution gateway

The control plane, Worker transport and Docker gateway use a private typed
protocol. The gateway keeps the Docker socket and its own receipt ledger; its
control-plane access uses a workload credential, not a database connection.
See [implementation status](STATUS.md) for launch and acceptance progress.

## Request and execution authority

1. An authenticated Worker presents its current Job grant to the control plane.
2. The control plane checks the database lease and current organization access,
   then resolves the Job's exact installed configuration.
3. A short-lived gateway ticket binds the verified Worker, complete lease,
   selected gateway and complete request digest.
4. The gateway redeems the ticket using its own separately allowlisted workload
   identity. The control plane rechecks the lease and assignment.
5. Preparing a command creates or reads its durable approval. A request ticket
   cannot approve it.
6. The Docker backend asks the control plane to verify current operation approval
   before execution. Operation IDs and receipts retain their existing immutable
   argument and recovery rules.

`GatewayTicketAuthority` remains in the control plane. Ticket lifetime is
configured between one and thirty seconds and cannot exceed the verified Worker
grant. The gateway receives no ticket-signing key. A Worker-state grant and a
gateway-request ticket are distinct types and signing purposes.

Gateway service authentication rejects users and unlisted applications/subjects.
The service identity selects its gateway ID; a request cannot choose that mapping.
Tenant and environment checks remain mandatory after signature validation.

## Configuration and persistent environments

`ConfiguredGateways` maps up to 128 installed immutable configuration references
to tenant, gateway, HTTPS endpoint, image digest and resource bounds. It requires
the exact configuration ID, version and SHA from the Job. An unavailable snapshot
fails instead of using current defaults for an old Job.

The environment has the session's opaque identifier in its own typed namespace.
Docker volume names additionally bind the owner. A configuration cannot supply
a control-plane host directory. The existing gateway rejects a changed
specification for an existing environment; image changes require an explicit
environment migration, not a silently empty workspace.

Command preparation binds the environment's current revision. A completed write
advances that revision. Read the current environment before proposing another
operation; an earlier approval or revision cannot authorize a different operation.

## Private protocol

`GatewayRequest` is protocol version 2, with bounded tagged commands:

| Command | Behavior |
| --- | --- |
| `acquire` | Acquire the data-owner-selected session environment |
| `get` | Read and validate that environment |
| `prepare_child_workspace` | Prepare only the server-resolved staged child workspace |
| `prepare_command` | Resolve the environment and obtain its durable approval |
| `submit_command` | Submit after fresh lease and approval checks |
| `inspect` | Read the original operation receipt within the assigned environment |
| `output` | Read bounded output using an offset and authenticated prefix digest |

These are private Worker/gateway messages. Public Web activity remains a separate
projection. Worker HTTP exposes no arbitrary environment destruction, fork target
selection or administrative cancellation. Child workspace preparation uses its
own admitted relation and receipt; it does not authorize child commands.

The control plane provides `/internal/worker/v1/gateway-ticket`,
`/internal/gateway/v1/resolve`, `/prepare` and `/authorize`. The execution gateway
provides `/internal/execution/v1/request`. Hosts register these real handlers only
after assembling their providers. The standalone executable installs these services;
validated child target configuration adds the scoped workspace preparation path.

Worker-to-control calls use the Worker token plus its Job grant. Gateway-to-control
calls use the gateway's workload token. Worker-to-gateway calls send only the
request-bound ticket in `x-zuno-gateway-ticket`; it is never passed into a command
container or returned as a tool result.

HTTP clients require HTTPS, refuse redirects and disable automatic POST retries.
Gateway frames are capped at 1 MiB; output requests permit up to 64 KiB of data.
An empty output page may return offset zero with the SHA-256 digest of an empty
prefix, and that cursor remains valid for polling.

Gateway operation lookup checks the environment before exposing receipts/output.
Control-plane callbacks validate the observed environment against the installed
Job configuration and continue using the existing organization approval store.
Neither a deserialized lease nor an environment object is authentication.

## Validation

Run `python3 scripts/check_enterprise_postgres.py` for the control API, identity,
lease and approval contracts. It uses temporary TLS credentials and restricted
PostgreSQL roles.

Run `python3 scripts/check_enterprise_docker.py` for the actual execution path.
It additionally runs the PostgreSQL/HTTPS contracts with
`ZUNO_GATEWAY_TEST_REQUIRED=1`, so a missing rootless Docker socket fails that
gate. A task-owned existing daemon may be supplied with
`ZUNO_ROOTLESS_DOCKER_SOCKET`.

The fixture submits a command before and after HITL, repeats its logical
submission, reads its output, then separately approves reading the persisted file.
It also checks service swaps, changed requests, foreign environment IDs, changed
environment facts and lease expiration. Fixture tokens are not live enterprise
IdP evidence. Native Linux amd64/arm64 CI remains required before certification.

See [中文](GATEWAY.zh.md), [rootless backend](ENVIRONMENTS.md),
[identity](AUTHENTICATION.md) and [approval continuation](WAITING.md).

`GatewayToolDispatcher` now implements `environment_command` over this transport.
Its approval wait precedes execution; its operation wait follows a durable handoff.
The executable gateway runs the receipt-delivery supervisor. The native runner
also starts a control plane, gateway and two independent Worker binaries; see
[deployment](DEPLOYMENT.md).
