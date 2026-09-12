# Public activity and client recovery

The public protocol in `zuno-types::activity` is separate from the Worker and
gateway protocols. It contains six independent groups:

| Type | Responsibility |
| --- | --- |
| `SessionItem` | Messages, visible thinking, invocations, approvals, Plan/Goal, compaction, artifacts and background work |
| `InvocationAction` | File, process, Web, Memory, Agent, Workflow, Council or generic tool activity |
| `InvocationSource` | Built-in, MCP, extension, provider, external Agent or explicitly unknown provenance |
| `InvocationState` | Queued, waiting, running, succeeded, failed, denied, cancelled or uncertain |
| `ContentBlock` | Text, code, diff, terminal, structured data, image or resource reference |
| `UiAction` | View, approve/reject, answer, interrupt or request an authorized resume |

Dynamic names are validated data. A tool named `read` does not acquire built-in
provenance or automatic approval. Actual adapters declare action/source metadata,
the shared kernel freezes it with the tool declaration and records it on each
invocation. Presentation is independent of side-effect, replay and permission
policies. Legacy declarations without presentation remain readable; their absence
may be filled by the same installed Worker adapter, while changed schemas or
explicit provenance remain incompatible.

## Committed records

PostgreSQL format 13 adds owner-scoped `activity_session`, `activity_item` and
`activity_frame`. Source message/part writes, public item updates and immutable
frames commit together. A projection failure rolls back the source write.
Identical public state does not allocate another sequence. The public sequence
is logical, per session and independent of physical rows or private event cursors.

Each item retains its first position and its latest revision. Frames include that
position, so an update to an older item can be placed correctly even when the
client has only loaded recent history. Message/part parent IDs preserve grouping. The data owner binds each message to its original Job, so repeated provider call IDs in later turns cannot borrow another invocation's approval, wait or operation state. Legacy backfill only records unambiguous input/request origins; missing proof remains conservative.
Queued input appears immediately under the same public message ID that later
becomes consumed or cancelled; its text is not duplicated by materialization.
Automatically generated child reports have a distinct message origin.

The shared projector selects public fields explicitly. It never copies prompt
receipts, model credentials, Worker leases, configuration snapshots, encrypted
reasoning, replay signatures or arbitrary tool metadata. Provider-approved visible
thinking is folded by default; a capsule cannot duplicate an identical visible
summary. Uncertain operations remain uncertain. Typed waits and execution facts
come from the data owner, rather than from model-authored labels.

Normalized usage uses the core accounting conversion and exact decimal strings.
Unknown accounting remains absent. Large text/argument previews are bounded and
marked as truncated/omitted; a client preview does not replace the durable source.

## HTTP and BFF

Both `/api/v1` and `/app/api/v1` expose:

| Endpoint | Query |
| --- | --- |
| `GET /sessions/{session}/history` | `limit` 1–100, optional `before` and `through` |
| `GET /sessions/{session}/frames` | `limit` 1–100, `after` (default `"0"`) |

All counters are canonical decimal strings. Each page rechecks current membership,
application policy and ownership. Foreign sessions are not found. Responses are
uncacheable and bounded to 1 MiB.

The first history page chooses a `through` snapshot. Older pages reuse that
boundary with the returned `before` position. Their contents remain unchanged
even while a tool finishes. New frames are read after `through`, in contiguous
sequence order. Frame pages advance only through the last included frame; a slow
reader can stop and resume without retaining a server-side queue.

`CommittedFrame` is authoritative public history. `LiveFrame` has a separate
generation and sequence and cannot carry billing totals or encrypted data.
Live progress uses the separate authenticated `/sessions/{session}/live` snapshot
read. Streaming transport, Plan/Goal/artifact projections and their action handlers
are not advertised before their adapters exist.

## TypeScript SDK

`enterprise/sdk` is a private source package. Rust generates the JSON Schema,
TypeScript discriminated unions and standalone bundled validators. Validators
require neither runtime schema compilation nor `eval`. Generation drift is a
preview CI gate.

```sh
cd enterprise/sdk
npm ci --ignore-scripts
npm run check:generated
npm test
```

`ActivityClient` supports the same-origin BFF or an explicitly configured HTTPS
bearer API. It never follows redirects with credentials, copies an HTTP error
body into an exception, or accepts a Worker endpoint. `watch` uses bounded,
interruptible frame polling; reconnect starts from the last committed cursor.

`ActivityState` applies a complete page atomically, detects gaps/conflicting
duplicates and keeps old history pages from overwriting newer revisions. Its
bounded item window can reload history without moving the stream cursor.
Merging an old page requires the original snapshot boundary.

The native tests cover final output from real Worker execution, queued inputs,
owner isolation/revocation, snapshot pagination during updates, transactional
projection rollback, visible/private reasoning separation and frozen format-12
migration with failed-DDL rollback. SDK tests cover wire rejection, exact counters,
deduplication, gaps, snapshot races, bounds, credential routing and cancellation.

See [中文](ACTIVITY.zh.md), [application API](APPLICATION.md),
[SDK source](sdk/README.md), [platforms](PLATFORMS.md) and [status](STATUS.md).

## Replaceable live snapshots

Workers publish coalesced visible text/thinking and invocation labels through
`/internal/worker/v1/live`, using workload authentication plus their current Job
grant. The data owner verifies the lease at admission and commit, binds the
original pending assistant message, and rejects changed/reordered snapshots.
Only the latest bounded snapshot is retained for a Job; it never enters model
history or usage accounting.

`liveMillis` on a Worker defaults to 500, accepts 100–5000, and can be null to
disable publication. The local buffer keeps at most 16 items/32 KiB raw text and
coalesces network work independently of model execution. Retry rollback replaces
the draft; checkpointed content immediately hides it. Signatures and encrypted
provider events have no live projection.

PostgreSQL format 14 adds the scoped transient row and clears it atomically when
Job phase/attempt changes. Public reads require current owner policy, the active
lease and a pending originating message; updates older than 30 seconds disappear.
`ActivityClient.live` and `LiveActivity` keep snapshots separate from committed
state, wait for required history and reject older/retired generations. A client
should key draft text/thinking separately from invocation IDs and clear live
content when no current snapshot exists.

The frozen format-13 migration retains messages, Memory and committed frames
through injected DDL failure. HTTPS tests observe a real pending provider draft
and verify it disappears after completion; SDK tests cover generation replacement,
retry clearing and fresh progress after a silent interval.
