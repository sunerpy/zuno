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

`GatewayRequest` is protocol version 8, with bounded tagged commands:

| Command | Behavior |
| --- | --- |
| `acquire` | Acquire the data-owner-selected session environment |
| `get` | Read and validate that environment |
| `prepare_files` | Bind a typed read/list/literal-search query to current organization approval |
| `query_files` | Read the approved immutable workspace revision under a current lease |
| `prepare_child_workspace` | Prepare only the server-resolved staged child workspace |
| `prepare_command` | Resolve the environment and obtain its durable approval |
| `submit_command` | Submit after fresh lease and approval checks |
| `inspect` | Read the original operation receipt within the assigned environment |
| `output` | Read bounded output using an offset and authenticated prefix digest |
| `preview_workspace_merge` | Compare a verified completed descendant with the parent workspace |
| `prepare_workspace_merge` | Persist the immutable offer and obtain human approval |
| `submit_workspace_merge` | Admit gateway-owned background work under current approval |
| `inspect_workspace_merge` | Query the original merge receipt |

Approval content uses a separate read-only protocol and ticket purpose at
`/internal/execution/v1/workspace-merge/read`. Gateway redemption rechecks the
viewer's current approval access through the control plane. It carries no Worker
lease and cannot be substituted for an execution ticket. Public downloads stream
through `/approvals/{approval}/merge/content`; details and limits are in
[workspace merge](WORKSPACES.md#approved-workspace-merge).

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

## Data-owner cancellation

The gateway supervisor also polls authenticated `internal/gateway/v1/cancellations`. This service-only path returns immutable admissions for stopped Jobs and stays valid after Worker revocation. It cannot start new operations. Docker stop and its actual terminal receipt remain separate; completed facts are preserved and unknown outcomes remain uncertain. See [control](CONTROL.md).

Gateway protocol 8 routes child preparation to the target gateway and separately
identifies the authorized source, including an existing Workflow group workspace.
Ordinary commands still require the executing session.

Snapshot exchange uses `/internal/execution/v1/snapshot` with a distinct
`x-zuno-snapshot-ticket`. Control-plane `/internal/gateway/v1/snapshot/`
`ticket`, `resolve`, `complete` and `fact` handlers bind routing, current
authorization and the immutable source receipt. The 1 MiB JSON frame bound still
applies; only the authenticated archive body uses the separate 512 MiB limit.
The source's service token is never sent to its peer.
See [workspace transfer](WORKSPACES.md#transfer-between-gateways).

## Workspace read operations

`WorkspaceFileReader` and `WorkspaceFileAuthority` separate data access from
approval. The Docker provider reads a verified immutable archive of the assigned
environment revision; it does not run caller-provided commands or extract files
onto the gateway host. Snapshots are reused for a revision and survive gateway
restart. The current lease and approval are checked before and after reading,
including cache hits. Changed operation arguments cannot reuse an old approval.

`workspace_read` accepts a logical relative `path`, decimal byte `offset` (default
`"0"`) and `maximumBytes` (1–65536). It returns a UTF-8 window and exact next byte
offset; binary files and links return metadata, and symbolic links are not
followed. `workspace_list` pages immediate children with `after` and `limit`
(1–200), bounded to 64 KiB of entry metadata. `workspace_search` matches literal,
case-sensitive `text` within a logical path, with at most 100 matches, 8 MiB scanned,
256 KiB per file and 64 KiB rendered text. Binary/large files are counted as skipped;
long-line previews retain the matched text. All paths remain inside the logical
workspace and the existing 512 MiB/50,000-entry snapshot limits apply.

Only applications in `autoReadApps` can receive automatic approval for these
built-in reads. Other allowed applications wait for the existing authoritative
HITL decision. This whitelist does not approve Shell or arbitrary MCP operations.
Control endpoints are `/internal/gateway/v1/files/prepare` and `/authorize`;
they accept only assigned gateway service identities. Both roles must support
gateway protocol 8; the state/Worker protocol is 13.

## Reviewed file edits

`workspace_edit` creates, replaces or deletes regular UTF-8 files through a
copy-on-write candidate. Every existing file requires the SHA from `workspace_read`;
creation requires `expected.kind: "absent"`. Null content deletes, while an empty
string creates/replaces with an empty file. Parent directories must exist. Links,
hardlink aliases, duplicate paths, NUL content and stale hashes are refused.
Up to 32 files and 256 KiB of new text are accepted, with a 512 KiB complete review
envelope. No shell command or host path is generated from file content.

Every edit requires human approval, including applications on the read whitelist.
`GET /approvals/{approval}/edit` exposes the exact before/after text under the
current approval-viewer policy. Approval binds all content, paths, expected hashes,
base snapshot and environment revision. A changed review needs a new decision.
The gateway builds and verifies an unpublished candidate, then atomically changes
the volume pointer, revision and receipt. The original workspace stays active until
publication succeeds.

The gateway-owned edit executor persists admission, retries delivery after restart
and preserves a committed receipt after response loss. Cancellation can win before
publication; a legitimate late committed receipt is retained without resuming a
cancelled Job. Worker approval/operation waits use the shared checkpoint and
exactly-once completion-consumption path. Private protocol commands are
`preview_edit`, `prepare_edit`, `submit_edit` and `inspect_edit`; control endpoints
live under `/internal/gateway/v1/edit/`. App/UI remains paused.

## External MCP

Gateway protocol 8 supports `prepare_mcp`, `submit_mcp` and `inspect_mcp` when a
real owner-bound `McpConnectionProvider` is configured. The separate MCP journal
preserves uncertain outcomes and never replays a submitted external call.
See [MCP.md](MCP.md) for configuration, approval and cancellation limits.
