# Organization-owned Memory

Shared Memory uses explicit tenant-owned spaces through `SharedMemoryStore`.
Private Global/Project Memory keeps its existing ownership, evidence and learning
pipeline. No shared synthetic user is created.

An organization administrator creates a space for an installed workspace and
assigns active organization members as readers, contributors or reviewers.
Administrators retain audit visibility; model recall always requires explicit
space membership. A contributor submits proposed entries, a different reviewer
approves them, and approval applications enforce the existing same-origin BFF
or authenticated API boundary.

Changes retain complete before/after content, document revision, space-policy
revision and a state digest. The Memory entry validator supplies the same threat,
locator, deduplication and final character-limit rules as private Memory.
Applying a proposal commits the document, revision, proposal state, idempotent
receipt and audit in one transaction. Undo requires the current document to
match the approved after-state. Stale content or permissions require a new
proposal. Configuration changes invalidate outstanding proposals.

## API and SDK

| Route | Operation |
| --- | --- |
| `GET /api/v1/workspaces/{workspace}/memory/spaces` | Bounded authorized space page |
| `GET /api/v1/memory/spaces/{space}` | Current document and caller role |
| `PUT /api/v1/memory/spaces/{space}` | Administrator configuration with revision CAS |
| `POST /api/v1/memory/spaces/{space}/changes` | Submit a bounded proposal |
| `GET /api/v1/memory/spaces/{space}/changes/{change}` | Read authorized before/after review |
| `POST /api/v1/memory/spaces/{space}/review` | Apply, reject or undo with state CAS |

The generated SDK provides `sharedMemorySpaces`, `sharedMemorySpace`,
`configureSharedMemory`, `proposeSharedMemory`, `sharedMemoryChange` and
`reviewSharedMemory`. Logical revisions remain decimal strings. The client
verifies response namespace/workspace/change identity. App/UI delivery remains
paused.

Configuration supplies a title, workspace, enabled flag, character limit and
explicit member list. Creation uses expected revision `"0"`. Later updates
replace the member list and require the current policy revision. At most 32
spaces may belong to a workspace, with 256 members per space, 256 pending
proposals and 32 edits per proposal. A document is limited to 32768 characters;
proposal requests are limited to 64 KiB and list pages to 512 KiB.

## Recall and recovery

Worker protocol 12 adds a separate shared snapshot request before each provider
request. Current space membership, enabled state and the user's inherited
`useMemories` choice determine recall. Revocation replaces the previous shared
context; a resumed checkpoint cannot keep injecting a withdrawn document.
Shared context is limited to 64 KiB of complete documents. Omitted space IDs are
explicitly reported in the durable prompt instead of silently truncating notes.

PostgreSQL preview format 25 adds space, membership, candidate, revision, request
and audit tables with forced RLS. Namespace advisory locks serialize competing
writers without locking private Memory or unrelated spaces. The exact format-24
migration preserves private sessions, messages, Memory and MCP operation data.

This delivery supports explicitly proposed organization notes. Private automatic
extraction remains private. Automatic promotion of private evidence into shared
Memory and shared learning maintenance are not registered capabilities; they
require explicit sharing authorization, source invalidation and review integration.

Shared Memory grants do not grant access to the author's private sessions,
credentials, attachments or private learning records. Disabling a space stops
recall but preserves its history for authorized administration.

## Explicit private evidence sharing

`SharedEvidenceStore` grants access to one bounded, verified excerpt, not the
author's private session. `POST /api/v1/memory/spaces/{space}/evidence` requires
the source owner in an approved human review application, a contributor/reviewer
role, the evidence ID and its exact digest. The private source must still exist
and match. Grants expose the excerpt and whether it is a user statement or a
successful operation; private session/input/operation locators are not returned.
`GET` on the same route pages the authorized grants.

`POST /api/v1/memory/spaces/{space}/evidence/{grant}/revoke` binds the author's
request ID and expected grant revision. Withdrawal and its audit/request receipt
commit together, including after the author leaves the space. A revoked grant is
never reactivated by retry; sharing again requires a new explicit request.

A proposal may bind exact after-state entries to grant IDs using `evidence`.
Grant capture and organization approval are separate decisions. The change digest
also covers the complete before/after support mapping. Applying and undoing
content commit its support and revision atomically. Undo may restore old support,
but cannot restore the authority or source that support depended on.

Public documents retain reviewed entries and list currently `suppressed` text.
Model recall omits an entry when none of its independent supporting grants remains
valid. Source forgetting, changed source bytes, withdrawn sharing, departed
authors and current organization authorization affect the next recall immediately;
a background maintenance run is not required. Existing manual notes and explicit
independently reviewed manual restoration do not inherit withdrawn evidence.
Previously shared excerpts and review history remain auditable to authorized
readers; revocation stops their reuse as active evidence.

A space allows at most 1,024 grants. One changed entry may reference up to 16 grants
and one proposal up to 32 evidence bindings. Each excerpt is at most 2,048 bytes.
The SDK provides `shareMemoryEvidence`, `sharedMemoryEvidence` and
`revokeSharedEvidence`. PostgreSQL preview format 28 adds forced-RLS grants, support
and owner audit tables; the format-27 migration retains Skill installations and
all earlier durable state. Automatic shared model maintenance remains separate
work; private automatic extraction does not silently publish evidence.
