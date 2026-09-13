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
