# Enterprise MCP operations

The gateway runs remote MCP calls with explicit owner credentials. The Worker
receives immutable tool declarations and operation receipts; command containers
receive neither MCP credentials nor a connection session.

## Configuration

An Agent definition may contain `mcpTools` (maximum 64). Each binding includes:

- `connection`: gateway-owned connection name.
- `server` and `tool`: validated display names and the upstream tool name.
- `endpoint`: exact HTTPS MCP endpoint, without user information, query or fragment.
- `revision`: positive resource/credential assignment revision.
- `definition`: the complete reviewed MCP `tools/list` declaration, including
  `name`, `description`, `inputSchema`, optional `outputSchema` and annotations.

The declaration is part of the existing configuration digest. Changing the
endpoint, declaration or assignment revision produces a new configuration.
The model receives a stable derived tool name and the original argument schema.
It cannot select credentials, URLs or a different tool in its arguments.
Completion-only profiles cannot install MCP tools.

Gateway service configuration supplies `mcpConnections` and `mcpParallelism`
(default 2, range 1–16). Each connection has `owner` (tenant/principal),
`connection`, `revision`, `endpoint`, `accessTokenFile`, optional
`rootCertificate`, and `timeoutMillis` (100–120000). Assignments must be unique
per owner/connection and match the immutable endpoint and revision exactly.
The token file must be absolute and contain a target-specific bearer token.
Do not use a Zuno access token or service credential as the MCP token.

The configured adapter reloads the token before establishing each connection.
Credential rotation remains the deployment's responsibility; it can be replaced
through `McpConnectionProvider`. This adapter does not perform interactive
OAuth, refresh tokens, start stdio servers or negotiate legacy SSE.
Entra/OIDC login to Zuno and MCP target authorization are separate boundaries.

## Approval and execution

Every external MCP tool requires human approval, including tools advertising
`readOnlyHint`. That annotation is untrusted metadata, not a permission grant.
Approval binds the owner, Job, invocation, arguments, target, revision and full
declaration. New admission checks current organization policy and the active
Worker lease.
Immediately before `tools/call`, the gateway rechecks the original admitted
attempt, current policy, approval validity and cancellation. Releasing the parent
Worker slot does not revoke an already admitted operation.

`GET /api/v1/approvals/{id}/mcp` exposes the full declaration, endpoint and exact
arguments to authorized reviewers. `EnterpriseClient.mcpReview()` validates the
response identity. Public activity uses `InvocationSource::Mcp`,
`ExecutionLocation::External` and `UiAction::ViewMcpCall`. No UI is delivered by
this change.

The gateway creates a separate MCP session for each operation. It refuses
redirects, uses configured TLS roots, and verifies the current complete tool
declaration before calling it. Catalog discovery is limited to 256 tools and
512 KiB; declarations to 32 KiB; arguments to 64 KiB; results to 256 KiB.
An upstream declaration change fails before `tools/call`. Failed preparation
does not silently retarget or retry the external call.

## Recovery

The gateway's independent `mcp.sqlite` journal uses format 1 and an exclusive
process lock. PostgreSQL preview format 24 atomically records approval admission,
execution attempt, completion and wait notification through the existing runtime.
Format 23 migration preserves existing sessions, messages, Memory and edit rows.
Gateway protocol 8 adds the real MCP prepare, submit and inspect operations;
the existing Docker journal remains format 5.

| Boundary | Recovery |
| --- | --- |
| Queued, no external call started | Recheck current authorization before execution |
| Complete result recorded; delivery response lost | Redeliver the same receipt; never call the tool again |
| Gateway restarted after entering `tools/call` | Preserve `Uncertain`; no automatic replay |
| Response lost, malformed or beyond the result bound | Preserve `Uncertain` for authoritative inspection |
| Cancelled before invocation | Persist a tombstone, preventing a later submission |
| Cancelled during invocation | Retain cancellation intent and the truthful bounded result when received |

MCP does not provide a generic authoritative operation query or idempotency
guarantee. Cancellation cannot claim that an already submitted remote effect
stopped. An uncertain result requires external-state inspection; this adapter
does not register an automatic resolution command.

Gateway data must be retained with the PostgreSQL state during recovery.
Deleting the journal loses local at-most-once evidence and is not an approved
recovery procedure. Enterprise gateway execution is supported on Linux
amd64/arm64; personal MCP/TUI/ACP platform support is unchanged.
