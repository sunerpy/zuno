# Enterprise private Memory

The control plane owns PostgreSQL Memory; Workers call the authenticated state
API. `MemoryService` still owns validation, character limits, candidate review,
apply, undo and rendering. Its `MemoryPersistence` and `MemoryAuthority` ports
are injected together. Personal profiles retain SQLite and local file projection.
The enterprise provider uses logical document keys and never imports or writes
host Memory files.

## Ownership and transactions

Every document, candidate, revision, evidence record, maintenance watermark,
learning Job, request receipt and audit row is scoped by tenant and principal.
`global` means the current user's cross-workspace Memory, not organization-wide
Memory. `project:<workspaceId>` is that user's Memory in one registered workspace.
A model or API command cannot supply another owner or a document path.

Session restrictions apply through the current ancestor chain. A newly created
child copies explicit parent policy, including automatic learning, and every read
or generation check still intersects current ancestor restrictions. Revoking a
parent therefore affects existing descendants. Missing, foreign, cyclic or
overdeep ancestry fails closed. Existing explicit child restrictions are preserved.

`PostgresMemoryBackend` is shared across the public application and Worker state
routes. It acquires bounded capacity before opening a transaction. Current
organization/app authorization, workspace membership, owner RLS and an owner
transaction lock protect the request. Worker requests additionally verify the
Job lease before work and before commit, using database time.

The synchronous shared Memory service runs in a bounded blocking task in the data
owner. All its persistence calls use that request's single PostgreSQL transaction.
Candidate/document/revision/provenance changes, request deduplication and audit
either commit together or roll back. No model or external network request waits
inside that transaction. Its deadline bounds processing even if the HTTP caller
disconnects; capacity remains held until the transaction settles.
After cancellation the owner awaits an explicit rollback before returning its
slot; rollback cleanup has a separate 12-second ceiling. Unconfirmed cleanup is a
storage failure and never a successful Memory reply.

## Consent, reading and forgetting

Enterprise defaults permit use of existing private Memory and disable generation.
Only the authenticated user through a configured trusted approval application can
change this policy or perform a manual apply/edit/reject/undo/forget operation.
An otherwise allowed API application can read and stage candidates, but cannot
grant consent or bypass review. A revisioned session override
can further restrict the owner's policy. A Worker cannot grant consent, approve
its own proposal, edit policy, import files or forget another source.

Manual changes stage a candidate for explicit apply. Foreground `memory_update`
requires current private-generation consent and uses the same revision and
character-limit checks. This consent is a bounded private data capability, not
permission to execute commands, modify a workspace, install Skills or write shared
organization Memory. Shared Memory writes and their organization approval workflow
remain unregistered.

`useMemories: false` removes Memory from future provider requests. Turning off
`generatePrivate` stops new model-generated changes and skips queued Memory
learning work; it does not erase existing notes. Running maintenance must recheck
consent and its exact lease before committing.

Evidence can cite an admitted user input or an authoritative successful gateway
operation. The service checks owner, workspace, source bytes and digest; clients
cannot submit a trusted verification flag. Recall revalidates source existence,
digest and forgetting state. Generation opt-out does not invalidate old evidence
for reading. A note with independent valid support remains available; one with no
valid support is suppressed immediately, before maintenance.

Explicit forgetting marks sources, rejects dependent pending candidates and
retracts unsupported derived entries atomically. It never reverses a replacement
to resurrect an older fact. Direct user reaffirmation becomes user-owned;
automatic maintenance cannot overwrite user-owned entries or resurrect explicitly
retired content. Maintenance batches preserve their candidate, evidence, document
revision, learning-Job settlement and watermark boundary.

## Application and Worker API

When the Memory backend is installed, both public application prefixes expose
`POST /workspaces/{workspace}/memory`. Browser access keeps the same BFF Origin and
CSRF requirements. External clients use a delegated API access token. The body is:

```json
{
  "requestId": "client-generated-stable-id",
  "command": {
    "kind": "propose",
    "change": {
      "scope": "project",
      "action": "add",
      "content": "Run cargo test before submitting a change.",
      "oldText": null,
      "reason": "Repository validation",
      "expectedRevision": null,
      "confidence": 1.0
    }
  }
}
```

Commands are typed: `read`, `read_entries`, `candidates`, `candidate`, `propose`, `apply`,
`reject`, `edit`, `undo`, `policy`, `set_policy`, `record_evidence` and `forget`.
Unknown fields are rejected. Request bodies are bounded to 64 KiB. Candidate
listing returns at most 512 records; entry queries support target, text and a
1–128 limit with a shared 32 KiB output budget.

`MemoryResponse.result` contains either `Ok` with a typed reply or `Err` with
`denied`, `conflict`, `unavailable`, `invalid_data` or an actionable `invalid` result. HTTP
authentication and malformed-envelope errors remain transport failures. Reusing
a mutation request ID with the same actor and arguments returns its committed
receipt; different arguments conflict. Reads are never replayed from a receipt.
Both replay and fresh requests recheck current authorization.

Candidate details and mutation replies return an opaque `stateDigest`. Manual
`apply`, `edit`, `reject` and `undo` requests supply `candidateId` and
`expectedState` from the exact candidate the user reviewed. The transaction
rejects a candidate changed after review; clients must present the changed
proposal instead of silently fetching a new digest and retrying approval.

Internal Worker protocol 11 adds `POST /internal/worker/v1/memory`, authenticated
by both workload identity and the current Job grant. It exposes only scoped
reads and foreground proposals. `memory_read` and `memory_update` retain the
personal tools' argument schema. An unconfirmed update becomes an uncertain
outcome, without mechanically replaying it.

Before **every** provider request, including checkpoint takeover, the Worker
refreshes Memory through the async service. An empty current result replaces old
checkpoint Memory. Revocation and unavailable state retain typed failures and
cannot cause a stale-context model request. The shared engine persists the actual
prepared prompt and its digest as usual.

## Configuration, migration and verification

The control-plane `memory` block sets `concurrentTransactions` (1–32, default 4),
`globalCharacters` (default 2200), `projectCharacters` (default 3000), and
`transactionTimeoutMillis` (1000–30000, default 10000). Character limits must be
1–131072. Omitting the block uses these defaults. User consent is durable policy,
not an operator configuration default.

PostgreSQL preview format 9 adds the scoped Memory tables and forced RLS.
Formats 1–8 migrate forward atomically with the format marker last. The captured
format-8 fixture preserves sessions, messages, Jobs, waits, browser records and
operation admissions through an injected migration failure and successful upgrade.
This does not migrate a personal SQLite database into the preview namespace.

`scripts/check_enterprise_postgres.py` covers isolation, CAS, replay, rollback,
consent, lease fencing, evidence revalidation and maintenance settlement.
`scripts/check_enterprise_docker.py` also runs the real executable control plane,
gateway and two Workers: private reads/updates, prompt refresh, consent revocation
while awaiting command approval, checkpoint continuation and isolated execution.

Automatic private extraction and independent maintenance are described below.
Organization-shared Memory, learning-management UI and the complete P6 fault
matrix remain separate work. Skill evaluation and application retain independent
review requirements.

The shared learning model runner now accepts an asynchronous
`LearningModelJournal`. Typed request/outcome records and normalized usage can be
handled by the data owner instead of requiring a local Worker database. The local
SQLite adapter remains the personal implementation; enterprise Workers use the
authenticated remote journal with scoped scheduling, consent, leases and budget
settlement. Only definitions with a complete configured learning path mount it.

See [中文](MEMORY.zh.md), [PostgreSQL](POSTGRES.md), [application API](APPLICATION.md)
and [deployment](DEPLOYMENT.md).

## Automatic private learning

Automatic learning requires a separate explicit `set_automation` command with
`sessionId`, `expectedRevision` and `enabled`. `automaticPrivate` defaults to false
and is independent of foreground `generatePrivate`. Only a user in a trusted
approval application can enable it. Disabling generation also disables automation;
queued and running learning work is fenced. Current organization, application and
session consent are checked at claim, model admission and Memory settlement.

The operator configures a source definition's `memoryLearning` with exact
`extraction` and `maintenance` configuration references. Both targets must be
installed completion profiles in the same logical workspace. They have no tools,
execution environment, delegation or recursively scheduled learning. Workers use
their configured native provider/credential bindings. A user does not supply a
model credential through the Memory API.

Input selection uses the shared prompt/schema envelope and JSON escaping bound;
the final request remains limited by the configured serialized-byte allowance.
Small valid profiles can carry short evidence. If current Memory cannot fit a
maintenance request, that batch becomes a durable `learning_input_budget` failure
without a provider call. It retains bounded coordinates and the input fingerprint,
preserves completed extraction, and does not block other owners' learning. An
unchanged failed batch is not continually re-enqueued. The operator can choose a
larger immutable model profile for subsequent source work.

Only completed root Jobs admitted after the current automation opt-in are
scheduled. Input and authoritative successful command evidence are frozen,
bounded and redacted before extraction. Extraction stores validated experiences,
source references and raw hints. A separately leased maintenance Job reads the
current private documents and correction signals, proposes bounded changes and
commits candidates, evidence, revisions, Job settlement and watermark atomically.
High-confidence supported changes may apply; other candidates remain reviewable.
User-owned entries and explicitly retired content remain protected.

When extracting a completed root, source selection can include commands from completed
child Agent, Workflow and Council descendants in the same owned workspace. The
data owner validates completed runtime state, child completion envelopes and
current source-session consent. Traversal visits at most 256 Jobs and 16 delegation
levels; the existing 63-operation and serialized-input limits still apply, with
truncation recorded. Agent report prose and delegated prompts are not execution or
user-authored evidence. Operation receipts must bind the exact Job/session, owner
and operation ID, succeed without cancellation, and contain complete output.
Frozen source bytes and policy are checked again at claim, renewal, model
admission and settlement.

Root and descendant completion atomically mark the root's learning scan dirty.
A late quiet result creates a new bounded extraction for previously unseen
sources; it does not alter an earlier frozen request or replay its evidence.
Source admission records distinguish captured, input-budget-omitted and unavailable
origins. Those decisions, the learning Job/activity and the observed scan-version
acknowledgement commit together. A concurrent newer completion stays pending.
Duplicate wakeups and cancelled captured batches do not repeat model work.

PostgreSQL format 22 backfills captured origins from existing frozen manifests,
including cancelled Jobs, and initializes pending scans for completed roots.
The scan retains current consent/configuration and all existing traversal/input
bounds. It does not promise exhaustive harvesting outside those bounds.

The data owner also reconstructs maintenance wakes from durable private Memory
changes and evidence validity. Once an authorized source has been extracted,
manual corrections and invalidated support can schedule maintenance without a new
foreground turn or extraction request. Only current configured source/model
bindings and current owner/session consent qualify. The scan rechecks those facts
inside the owner transaction and admits at most one active maintenance batch per
workspace.

At claim, changed document revisions or evidence retire an obsolete queued
snapshot before model admission. In-flight results still require the original
revision/evidence checks at settlement; a later scan builds a new bounded batch
for the changed input. Worker replacement retains an existing Job's budget.
Unchanged successful input and a cancelled identical batch do not schedule again.
Source invalidation suppresses recall immediately; maintenance may then remove the
unsupported managed entry. Its own successful document changes do not create a
self-sustaining model loop.

Normal Agent work takes priority when a Worker polls. Learning uses the same
bounded Worker slots, continues lease renewal during model I/O, and drains under
the existing Worker shutdown deadline. The control plane handles state only.
Learning grants have a separate signing purpose and cannot execute foreground
Jobs or gateway operations. Model request/outcome journals are scoped and durable;
late authenticated outcomes can reconcile an existing reservation without restoring
execution authority or applying Memory.

Each logical learning Job retains its deadline, attempt counter and token charges
across Worker replacement. A request reserves one third of the configured Job
token allowance; complete measured usage settles that reservation. Unknown or
incomplete usage retains the reservation as a conservative charge. Positive
persisted retry delays honor the longer provider delay. A stored valid model result
is returned on takeover so completion can proceed without another model call.

PostgreSQL format 20 preserves existing data and leaves automatic learning disabled
for all previous consent rows. Source format 19 is frozen at
`c7697391f1025e4fc0b7f6c959d0329fdffb44181bb6bdb18661bcc2c328e037`.
The native process fixture covers opt-in, separate extraction/maintenance,
private recall, cross-user isolation and opt-out. Shared organization Memory
and full operational acceptance remain pending; UI
design and implementation stay deferred to Penpot.

## Inspect and cancel learning

With the Memory backend installed, both application prefixes expose:

| Route | Result |
| --- | --- |
| `GET /workspaces/{workspace}/learning/jobs` | Owner-scoped page, optional stage/state filters |
| `GET /learning/jobs/{job}` | Current typed state and budget |
| `POST /learning/jobs/{job}/cancel` | Idempotent cancellation with `requestId` |

Pages accept `limit` 1–100 and the complete cursor pair `beforeCreatedAtMs` /
`beforeJobId`, ordered newest first. Counters and timestamps are exact decimal
strings. Views distinguish extraction/maintenance and queued, running, completed,
skipped, failed, cancelled or uncertain state. They expose charges, reservations,
model request counts and unconfirmed accounting, never frozen prompts, credentials,
configuration or execution grants. Every request rechecks current authorization.

Cancellation fences queued/running work, conservatively charges outstanding
reservations and publishes its durable activity in one transaction. It stops
future model admission and Memory settlement; it cannot undo an already sent model
request. A valid late usage receipt may adjust accounting without resuming the Job.
Repeated cancellation with the same request ID returns its original receipt;
changed reuse conflicts. Cancelling a terminal Job preserves its result.

Session history includes the same logical learning Job from queueing through
settlement, with typed kind, budget progress and supported actions. Public SDK
methods are `learningJobs`, `learningJob` and `cancelLearning`. This provides
backend/client contracts; App design and UI delivery remain paused.

## Explicit organization sharing

Organization-owned spaces are separate from private Global/Project residency.
`SharedMemoryStore` requires explicit membership and independent review; private
automatic learning is not silently published. Worker protocol 12 reloads bounded
shared snapshots before each model request and honors inherited `useMemories`.
See [SHARED_MEMORY.md](SHARED_MEMORY.md) for the real API and limitations.
