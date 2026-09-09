# HTTP API and OpenAPI

`zuno serve` exposes the same sessions, tools, durable events, and runtime used by
the TUI and ACP adapter. It is an in-process client surface, not a second agent
implementation.

This page owns the shipped HTTP contract. The live OpenAPI document is the
machine-readable inventory; pages for sessions, attachments, retention, and
permissions explain behavior that a schema alone cannot express.

## Start the server

Pin a port when another process needs a stable address:

```sh
zuno serve --hostname 127.0.0.1 --port 4096
curl http://127.0.0.1:4096/health
```

Without flags, `server.hostname` and `server.port` apply. If those keys are also
unset, the listener uses `127.0.0.1` and an operating-system-assigned port. The
startup output reports the bound address.

`GET /health` returns plain text:

```text
ok
```

`GET /api/health` returns JSON:

```json
{"healthy":true}
```

See [`zuno serve`](/cli/serve) for flag precedence, logging, and process lifetime.

## Authentication and exposure

A server without `ZUNO_SERVER_PASSWORD` may bind only to addresses that all
resolve to loopback. Zuno refuses startup if any resolved address is non-loopback.
Set a non-empty password before exposing the listener:

```sh
ZUNO_SERVER_USERNAME=zuno \
ZUNO_SERVER_PASSWORD='replace-with-a-secret' \
  zuno serve --hostname 192.0.2.10 --port 4096

curl --user 'zuno:replace-with-a-secret' \
  http://192.0.2.10:4096/api/location
```

`ZUNO_SERVER_USERNAME` defaults to `zuno` only when it is absent. When Basic Auth
is enabled, every route requires valid credentials. Failed requests return `401`
and `WWW-Authenticate: Basic realm="Secure Area"`.

`--browser-auth` is a separate loopback-only mode. Startup prints one URI with a
single-use token. `GET /auth/browser` is the only route allowed to exchange that
token without existing credentials. A successful exchange redirects to `/health`
and installs a signed, authority-bound, 30-day
`HttpOnly; SameSite=Strict; Path=/` cookie. Cookie-authorized unsafe methods must
also send an exact matching `Origin`. The bootstrap query is removed before
request logging.

The Basic username and password are withheld from model-composed Shell commands.
Authentication protects the listener; it does not widen an Agent's tool,
permission, or sandbox authority.

## OpenAPI document

The running process publishes OpenAPI 3.1 at:

- `GET /openapi.json`
- `GET /doc`
- `GET /api/doc`

All three return the same JSON document. They follow the same authentication rule
as the rest of the server. The document version is the running Zuno package
version, so fetch it from the process a client will call rather than copying one
from another release.

```sh
curl --silent http://127.0.0.1:4096/openapi.json > zuno-openapi.json
```

The route inventory is exact: an operation is added only when a handler exists.
The current document does **not** fully type every body. It records 32 reviewed
body-schema gaps, including SSE, WebSocket, raw-file, catalog, learning, prompt,
and history responses. Seven operations are intentionally bodyless. A generated
client must preserve unknown JSON fields and hand-model any operation whose
`200` response or request body has no schema.

Zuno does not ship the old source-tree `generate` command. Generate a client from
the live `/openapi.json` document with the tool of your choice, then test it
against the same Zuno version.

## Endpoint inventory

`{sessionID}`, `{requestID}`, `{providerID}`, `{integrationID}`, and `{ptyID}` are
path parameters. `*path` is a wildcard path below the active project directory.

### Process and catalog

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/health` | Plain-text process health |
| `GET` | `/api/health` | JSON process health |
| `GET` | `/api/location` | Active directory and project identity |
| `GET` | `/api/event` | Live process-wide SSE notifications |
| `GET` | `/api/agent` | Resolved Agent catalog |
| `GET` | `/api/command` | Resolved command catalog |
| `GET` | `/api/skill` | Resolved Skill catalog |
| `GET` | `/api/reference` | Resolved reference catalog |
| `GET` | `/api/model` | Resolved model catalog |
| `GET` | `/api/provider` | Provider list |
| `GET` | `/api/provider/{providerID}` | One Provider |
| `GET` | `/api/integration` | Integration list |
| `GET` | `/api/integration/{integrationID}` | One Integration, when present |

Catalog and location responses carry the active directory context. A missing
optional backend may return `503 backend_unavailable`; Zuno does not register a
placeholder operation merely to return that error.

### Filesystem

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/fs/read/{*path}` | Read one file as content-type-dependent bytes |
| `GET` | `/api/fs/list` | List a directory under the active project |
| `GET` | `/api/fs/find` | Find entries under the active project |

Paths cannot escape the active session directory. `read` refuses files larger
than 32 MiB with `413 file_too_large`. A `find` response includes `truncated`;
when it is `true`, no match means only that the bounded traversal did not find
one before it stopped. See [Headless runs](/guide/headless) for traversal and
concurrency limits.

### Sessions and turns

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/session` | List sessions |
| `POST` | `/api/session` | Create a session |
| `GET` | `/api/session/active` | List process-active sessions |
| `GET` | `/api/session/{sessionID}` | Read one session |
| `GET` | `/api/session/{sessionID}/context` | Read current context items |
| `GET` | `/api/session/{sessionID}/history` | Read durable history |
| `GET` | `/api/session/{sessionID}/message` | Read paginated messages |
| `GET` | `/api/session/{sessionID}/learning` | Read paginated Experience, queue/deadline/error state and latest recall selection |
| `GET` | `/api/session/{sessionID}/memory-policy` | Read session Memory policy |
| `PUT` | `/api/session/{sessionID}/memory-policy` | Revision-guarded Memory policy update |
| `GET` | `/api/session/{sessionID}/event` | Replay and follow durable session events over SSE |
| `POST` | `/api/session/{sessionID}/agent` | Switch Agent at a safe host boundary |
| `POST` | `/api/session/{sessionID}/model` | Switch model at a safe host boundary |
| `POST` | `/api/session/{sessionID}/prompt` | Admit a prompt and optional files |
| `POST` | `/api/session/{sessionID}/compact` | Request durable compaction |
| `POST` | `/api/session/{sessionID}/wait` | Wait for the session host to settle |
| `POST` | `/api/session/{sessionID}/interrupt` | Interrupt a live turn; idle sessions are unchanged |
| `POST` | `/api/session/{sessionID}/revert/stage` | Stage a snapshot revert |
| `POST` | `/api/session/{sessionID}/revert/clear` | Clear the staged revert |
| `POST` | `/api/session/{sessionID}/revert/commit` | Commit the staged revert |

The learning route accepts `offset` (default `0`) and `limit` (default `100`,
clamped to `1..100`). `experiencePage` reports `total` and `nextOffset`; `queue`
contains counts, due time and recent job diagnostics; `retrieval` describes the
latest selected ids, query digest, estimated token cost or skip reason. Reading
this projection does not run a model or rebuild an index.

A Memory policy update accepts `useMemories`, `generation` (`enabled` or
`disabled`), and `expectedRevision`. `excluded` is host-owned and cannot be
requested. Stale revisions, an excluded session, or a live turn that owns the
session return `409` rather than overwriting current state.

`POST .../prompt` uses the same durable inbox as TUI and ACP. A successful HTTP
response is not a private client-side turn; events and projections remain visible
to every attached client.

### Human questions and permissions

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/permission/request` | List pending durable permission requests across the active location |
| `GET` | `/api/session/{sessionID}/permission` | List pending permission requests for one session |
| `POST` | `/api/session/{sessionID}/permission/{requestID}/reply` | Settle one permission request |
| `GET` | `/api/question/request` | List pending durable questions across the active location |
| `GET` | `/api/session/{sessionID}/question` | List pending questions for one session |
| `POST` | `/api/session/{sessionID}/question/{requestID}/reply` | Atomically settle a question and admit its model-visible answer |
| `POST` | `/api/session/{sessionID}/question/{requestID}/reject` | Reject a pending question |

Pending rows survive process restart. Live channels only notify consumers. A
second reply to an already settled request cannot admit duplicate input.

### Session maintenance

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/session/prune` | Preview an archive or delete selection |
| `POST` | `/api/session/prune` | Apply a confirmed archive or delete mutation |

Preview before mutation. Deletion has explicit confirmation and derived-learning
cleanup choices; the full contract is in [Session retention](/session-retention).

### Pseudo-terminals

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/pty` | List PTY sessions |
| `POST` | `/api/pty` | Create a PTY session |
| `GET` | `/api/pty/{ptyID}` | Read PTY metadata |
| `PUT` | `/api/pty/{ptyID}` | Update PTY state, including size |
| `DELETE` | `/api/pty/{ptyID}` | Stop and remove a PTY |
| `POST` | `/api/pty/{ptyID}/connect-token` | Mint a scoped, single-use connection token |
| `GET` | `/api/pty/{ptyID}/connect` | Upgrade to the PTY WebSocket stream |

The connection token is not a replacement for HTTP authentication. It narrows one
WebSocket attachment to one PTY after the request has already crossed the server
authentication boundary.

## Server-sent events

`GET /api/event` is live and process-wide. Its first event is
`server.connected`; it does not replay durable history. If the subscriber falls
behind, Zuno emits `server.stream.lagged` with `action: "reconnect"`.

`GET /api/session/{sessionID}/event` is the durable stream. On connect it replays
stored session events, crosses a captured live boundary without duplicates, and
then follows new events. Each normal SSE message has:

```text
event: message
id: ses_...:42
data: {"id":"evt_...","type":"...","durable":{"aggregateID":"ses_...","seq":42,"version":1},"data":{...}}
```

Save the SSE `id` and send it back as `Last-Event-ID` after reconnect. The cursor
format is `<sessionID>:<non-negative-sequence>` and is valid only for the session
that minted it. Invalid or cross-session cursors return `400
invalid_event_request`; an unknown session returns `404 not_found` instead of an
empty stream.

If a live session subscriber lags, the terminal `server.stream.lagged` event
includes `lastCursor` and closes that stream. Reconnect with the last confirmed
cursor to resume from SQLite. Both streams send `heartbeat` keep-alive frames and
set `Cache-Control: no-cache` and `X-Accel-Buffering: no`.

```sh
curl -N \
  -H 'Accept: text/event-stream' \
  -H 'Last-Event-ID: ses_01abc:42' \
  http://127.0.0.1:4096/api/session/ses_01abc/event
```

## JSON and error conventions

Many successful JSON responses use a `{ "data": ... }` envelope. List and
location endpoints may add their own pagination or directory fields; use the live
OpenAPI schema where one is bound and preserve unknown fields.

Most failures use:

```json
{
  "error": {
    "code": "invalid_request",
    "message": "the actionable detail"
  }
}
```

Common codes include `invalid_request`, `forbidden`, `not_found`, `conflict`,
`backend_unavailable`, `database_error`, `filesystem_error`, `file_too_large`,
and `event_stream_failed`. Do not branch on the message text.

Two compatibility cases use a tagged body instead: a missing Provider returns
`_tag: "ProviderNotFoundError"`, and a missing required query key returns
`_tag: "InvalidRequestError"`. A client that calls those operations must accept
that shape as well as the normal envelope.

## Deliberately absent surfaces

- There is no unauthenticated non-loopback mode.
- Historical `zuno serve` options that were removed from the parser are listed
  only in the [`zuno serve`](/cli/serve) retirement record.
- There is no unscoped `/event` alias; use `/api/event`.
- Saved `always` permissions are process/session state, not durable permission
  rows, so there is no saved-permission list or revoke endpoint.
- No operation is registered until a real handler exists.

## See also

- [`zuno serve`](/cli/serve)
- [Headless runs](/guide/headless)
- [Sessions and turns](/guide/sessions)
- [Images and file references](/reference/attachments)
- [Permissions and sandboxing](/guide/permissions)
- [Client interface architecture](/design/client-interfaces)
