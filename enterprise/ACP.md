# Enterprise remote ACP bridge

This Linux service connects an editor's ACP stdio transport to the enterprise
public HTTPS API. The control plane, Workers and gateway retain ownership of
sessions, input, approvals, model work and execution. The bridge delegates no
editor filesystem, terminal or MCP access and holds no database or Worker grant.
Personal `zuno acp` keeps its independent platform support.

## Configuration

```json
{
  "stateDirectory": "/var/lib/zuno-enterprise-preview/acp-alice",
  "service": {
    "kind": "acp_bridge",
    "api": {
      "endpoint": "https://agent.example/api/v1/",
      "accessTokenFile": "/run/secrets/alice-access-token",
      "rootCertificate": null
    },
    "workspaceId": "workspace",
    "localDirectory": "/home/alice/project",
    "maxSessions": 32,
    "pollMillis": 500
  }
}
```

Run `zuno-enterprise --config /absolute/bridge.json`. Stdout contains only ACP
JSON-RPC; diagnostics use stderr and the private state directory. The configured
local directory validates the editor's cwd mapping; it does not synchronize files
or authorize remote access to that path. Each process pins one authenticated actor.
Token-file refresh must preserve tenant, owner, actor kind and client application.
A login client supplies the API OAuth2 access token; ID tokens are not API credentials.

## Implemented protocol

Initialize, session creation/load/resume/list/close, text prompts and cancellation
have real handlers. Attachments, client MCP, model switching, steering and deletion
are not advertised. Enterprise definitions select models and tools. Session resume
reattaches the session; `_zuno/observe` watches an existing Job without new input.

Standard ACP messages, visible reasoning summaries, tool status and Plans are
projected from public durable facts. `_zuno/activity` preserves full typed
`CommittedFrame` data, including all public enum variants, authorized actions,
replacement and removal. `_zuno/live` carries replaceable progress independently.
Loading returns the newest 100 records with fixed `through`/`before` coordinates;
older records remain available through paged `_zuno/history` requests.

| Extension | Parameters and behavior |
| --- | --- |
| `_zuno/history` | sessionId, through, before; return an older history page |
| `_zuno/job` | sessionId, jobId; inspect an owned Job |
| `_zuno/request` | sessionId, requestId; inspect durable admission |
| `_zuno/observe` | sessionId, jobId; observe the same Job until terminal or paused |

Each extension requires the session to be open on the authenticated connection.

## Approvals and recovery

An ACP permission reply cannot approve enterprise work. The bridge displays
persistent approval facts; an authorized review application must answer through
the enterprise API. Editor allow-always, filesystem and terminal capabilities do
not widen organization policy. Commands remain in the enterprise environment.

Admission reads the input version and submits with CAS and a stable request ID.
Clients may supply `_meta.zuno.requestId` for later cross-connection inspection;
the default ID is stable only within the current connection. POSTs are never
mechanically repeated. A lost response is inspected through its original receipt;
an unconfirmed error includes the request coordinates and does not assert rejection.
Cancellation binds the current Job/turn, including late admission after explicit
withdrawal. Disconnect detaches observation and does not cancel accepted work.

This is a backend bridge delivery. App/UI work remains paused and future design
starts in Penpot.
