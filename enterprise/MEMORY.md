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

Internal Worker protocol 10 adds `POST /internal/worker/v1/memory`, authenticated
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

Background extraction scheduling, organization-shared Memory, learning-management
UI and the complete P6 fault matrix remain separate work. The maintenance storage
contract is implemented; it does not advertise an automatic enterprise extraction
producer or bypass independent Skill evaluation/application review.

See [中文](MEMORY.zh.md), [PostgreSQL](POSTGRES.md), [application API](APPLICATION.md)
and [deployment](DEPLOYMENT.md).
