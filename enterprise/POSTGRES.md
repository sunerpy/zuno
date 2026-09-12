# PostgreSQL preview persistence

This adapter implements scoped sessions, runtime Jobs, organization approvals and
the shared kernel's `TurnPersistence` port. `AgentApplication` and the ordinary
bounded driver consume the same contracts for SQLite and PostgreSQL, including
durable wait consumption. Authenticated Worker transport uses this adapter.
The [browser login/session store](BROWSER.md) uses the same data-owner pool.
PostgreSQL Memory and complete runtime assembly remain subsequent work; this
library alone does not register an enterprise server or worker.

## Database boundary

The namespace is fixed to `zuno_enterprise_preview`. It does not use the stable
channel's database or the PostgreSQL `public` schema.

Provision separate migration and runtime credentials. The runtime role must have
no superuser/BYPASSRLS capability, ownership membership, schema CREATE, table
TRUNCATE/TRIGGER or marker modification privilege. For example, a database
administrator can provision the role and set its password interactively:

```sql
CREATE ROLE zuno_preview_runtime LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE;
\password zuno_preview_runtime
```

Pass the migration pool and that role name to `zuno_postgres::migrate`. Migration
takes a transaction-scoped advisory lock, creates tables/constraints/policies,
grants the bounded runtime privileges, and writes the format marker last.
Repeating a valid migration is idempotent. Unmarked, future or changed schemas
fail without a guessed repair or downgrade.

`PostgresOptions` always selects `VerifyFull` TLS. An URL's `sslmode=disable` cannot
override it. Provide the trusted root certificate when using a private CA. Keep
connection URLs in the control plane's secret configuration; the options type
does not implement Debug or Serialize.

Each application transaction sets its tenant and principal with transaction-local
settings. All data tables enable and force row security; application queries also
filter ownership explicitly. Closing or rolling back a transaction clears its
identity before the connection is reused. A scope value is attribution, not an
authentication credential: the host authenticates first. User-facing transactions
also check organization membership, actor/application and policy revision under
locks held through commit. Organization bootstrap must precede resource use.
See the [application API](APPLICATION.md) for the connected public handlers.

Creation receipts prevent changed-content reuse of an idempotency key. Input,
admission event and caller attribution commit together. Event cursors are logical
per-session positions, and session paging includes both timestamp and ID.

## Verification

Run as a non-root development user with PostgreSQL server binaries, `pg_config`,
OpenSSL, Python and Cargo available:

```sh
python3 scripts/check_enterprise_postgres.py
```

The script creates an isolated loopback PostgreSQL cluster, temporary CA/server
certificates and restricted roles, exercises the real adapter over verified TLS,
then stops and removes that cluster. It does not use an existing database.
`ZUNO_POSTGRES_BINDIR` may select the server binary directory.

The contract checks role restrictions, TLS override refusal, RLS with and without
application filters, connection reuse, cross-user/cross-tenant isolation, equal
timestamp pagination, repeated requests, audit rollback and schema drift/future
format refusal. The dedicated preview CI runs PostgreSQL 16 on Linux amd64 and
arm64; local validation also uses PostgreSQL 18.

SQLx Core and its PostgreSQL driver are pinned together at an exact version.
The general SQLx facade resolves an optional SQLite dependency with a link range
incompatible with Zuno's released `rusqlite`; this adapter does not downgrade or
replace the local SQLite backend. SQLx-specific types remain in the backend,
outside the application persistence port.

See [the Chinese guide](POSTGRES.zh.md) and [implementation status](STATUS.md).

## Fenced kernel state

`PostgresBackend::turn_state` is a data-owner factory. The host authenticates the
Worker, selects its exact execution lease, and resolves an executor-visible
directory before calling it. The directory is not accepted from a public client
and must not expose a control-plane private path. Workers receive a state API
client in the distributed deployment; they do not receive this pool.

Every operation checks the owner/session, current lease and current organization
membership/application policy in the database transaction. Ordinary state
operations recheck the lease before commit, so a slow write cannot outlive its
authority. Policy/member read locks serialize mutations with revocation.

Message/part identities, tool parameters and settled receipts are immutable at
their respective commit boundaries. Assistant usage reuses the local accounting
normalizer and reconciles by message delta. Parallel tool results commit as one
batch. Prompt receipts, attempt facts and retry bookkeeping remain durable.
History selects the committed compaction suffix before decoding discarded tool
or reasoning payloads; exact developer receipts remain available for replay.

Driver admission and checkpoint validation use the shared journal state machine.
A checkpoint event, runtime checkpoint/version and Worker-slot release commit
together. A terminal driver result and native Job settlement also share a
transaction. A lost response after commit can therefore be reconstructed from
the next claimant's Job checkpoint, without restarting steps or tool calls.
Unfinished/in-flight advances retain the conservative inspection boundary.

The materializer supports root text inputs. The bounded driver also consumes
[durable invocation waits](WAITING.md); completion facts and original tool results
advance with the checkpoint in one transaction. Remote steering, attachments,
human/child producers and gateway-to-tool assembly remain incomplete. A local
human-request result cannot masquerade as a distributed wait checkpoint.

Preview format 4 adds message/part and retry state, parent/context metadata and
usage projections. Formats 1–3 migrate atomically without rebuilding databases.
The format-3 fixture retains its captured authorization DDL and original source
digest; injected migration failure preserves its organization policy, membership,
audit, Job budget and lease state.

## Runtime Jobs and checkpoints

`PostgresBackend::runtime` creates a tenant-bound `RuntimeStore`. It reuses native
`agent_job` identities and root-turn subjects. Admission commits the input, input
CAS, native Job, scheduling state and events together. Sessions use logical IDs
and event cursors; no physical SQLite row identifier is used as a PostgreSQL cursor.

Claims acquire the session row with `FOR UPDATE SKIP LOCKED`, then commit a
session epoch, worker incarnation and execution attempt. Leases include owner
routing, but that value is not authentication: every mutation checks the stored
owner, Job/session, worker, attempt, epoch, checkpoint version and database-time
deadline inside its transaction. Renewal cannot shorten an existing lease.

The control-plane role can call the fixed, read-only `dispatch_owners` function.
It returns at most 64 owner/order records for its requested tenant. The function
runs as the schema owner and cannot return prompts, Jobs, checkpoints, or arbitrary
queries. PUBLIC execution is revoked. Private reads and writes still use exact
owner RLS. Empty owner probes advance the scheduling order so a page of inactive
owners cannot starve later work. Workers and end users receive no database role.

A committed checkpoint releases worker execution capacity while retaining the
logical session Job. The next claimant resumes that Job before another queued
turn. Completion requires consumed input; stale leases cannot overwrite results.
Lease expiry requeues only an unchanged exact driver checkpoint whose unfinished
calls are fully accounted for. A newer started advance or unexplained operation
records `uncertain` and retains the logical session hold. Other sessions remain
eligible. This adapter does not replay external side effects.

PostgreSQL preview format 2 advances a verified format-1 schema atomically. The
schema owner can be NOSUPERUSER/NOBYPASSRLS. Backfill briefly removes FORCE RLS
under exclusive DDL locks in the migration transaction, restores FORCE, validates
deferred references, updates privileges, and writes the format marker last. The
isolated fixture injects failure midway through DDL and compares the original
workspace, session, input, event and request receipt before/after rollback and
successful upgrade. Supported preview databases are not rebuilt.

The TLS contract also exercises two concurrent claimants, independent sessions,
input CAS, checkpoint handoff, old-worker rejection, uncertain execution, empty
owner pages, RLS and atomic audit failure. Tests simulate input materialization;
they do not certify a remote engine, current organization authorization, child
completion delivery, or external-operation recovery. Those integrations remain
required before the enterprise runtime can be registered as available.

Format 3 adds [organization authorization and approvals](AUTHORIZATION.md), with forward migrations from both formats 1 and 2.

Format 5 adds owner-scoped waits, timer indexes and the `waiting` Job phase.
Registration reads early completions; publication wakes only waiting Jobs.
Consumption, tool results, checkpoint/version and lease release are atomic.
The captured format-4 fixture verifies preserved messages, signed metadata,
usage, budgets and lease state across rollback and forward migration.


## Canonical context and input application

Format 6 adds owner-scoped canonical context state and input execution receipts.
Tracker revision CAS, epoch/time monotonicity and duplicate handling share the
SQLite validation rules. A provider request commits its initial assistant,
request event, database-assigned request sequence and context state atomically.
Assistant completion also commits its context snapshot with parts and usage.
No SQLite connection is passed through the shared driver or Worker protocol.

The root materializer distinguishes received/history-recorded input from input
actually applied immediately before provider dispatch. A consumed legacy input
is backfilled as recorded, not as proof that a model executed it. Remote steering
and arbitrary old-turn reassignment remain unsupported.

A pre-canonical PostgreSQL session retains its authoritative cumulative usage;
its context occupancy stays unknown until a current request supplies a confirmed
measurement. The current request's post-hook estimate still protects its context
limit. No incomplete history is treated as an exact old context window.

The captured format-5 fixture preserves pending waits, Job budgets, lease state,
messages and usage across failed/successful format-6 migration. The internal Worker
protocol is version 9; it is separate from public UI DTOs. Its compatible claims,
stable input timestamps and bounded grant renewal are described in [Workers](WORKERS.md).

## Browser authentication state

Format 7 adds tenant-scoped `browser_login`, `browser_session` and
`authentication_audit`. Before a browser is identified, only the deployment's
fixed tenant can select its encrypted transaction or credential digest. The
host's BFF owns this factory; public requests cannot choose a tenant or database
principal. These tables also force RLS and keep explicit tenant predicates.

Transaction consumption is a single database-time `DELETE ... RETURNING`,
committed before a token POST. Invalid bindings cannot consume a different
browser's state. Configured capacities use transaction-scoped locks; expired
entries are pruned in bounded batches. Session creation/revocation and audit
commit or roll back together. Credentials and expiry are checked against both
indexed columns and decoded storage records.

Formats 1–6 migrate atomically. The captured format-6 fixture retains native
sessions, message parts, waiting Jobs and input receipts across a failed and a
successful browser-schema migration. BFF and Worker network fixtures run in
separate temporary databases in the same isolated verification cluster.

## Operation admission and completion

Format 8 adds `gateway_operation` and `gateway_operation_attempt`. Gateway
execution authorization records the logical operation and admitted attempt in
the same transaction as current approval/lease verification. Completion checks
the authenticated gateway and those immutable records; a truthful receipt may
arrive after the original lease expired. Receipt, completion event and matching
wait readiness commit together. Changed or unadmitted facts are refused.

The exact format-7 fixture preserves sessions, message parts, Jobs, waits and a
valid browser session across injected DDL failure and successful migration.
See [operation result delivery](OPERATION_RESULTS.md) for gateway acknowledgement,
output bounds and the separate parent-consumption boundary.

## Private Memory

Current format 9 adds owner-scoped Memory policy, session overrides, documents,
candidates, revisions, evidence, provenance, retirement, learning Jobs, maintenance
watermarks, request receipts and audit. Every new table forces owner RLS. The
captured format-8 fixture verifies rollback and preservation of existing runtime,
browser and operation data before the marker advances.

The data owner runs the shared Memory service in bounded blocking capacity with
one transaction per request. Actor/workspace checks, candidate CAS, source
revalidation, consent, learning lease checks and result/audit commit stay together.
Workers use internal protocol 9 and never receive this provider or pool.
See [Memory](MEMORY.md) for implemented behavior and remaining producers.

## Child execution-session binding

Format 10 separates a native Job's parent session from the session its Worker
executes. `runtime_job` keeps the logical `agent_job` foreign key independently
of its input/session binding; `runtime_session` references the exact executing
Job/session pair. Existing root rows keep the same identifiers and values.

The format-9 fixture includes real private Memory and policy values along with
the previous runtime history. Migration failure restores the old constraints
and marker; success preserves all rows. A storage regression verifies that a
child execution takes the child's slot while retaining its delegating parent.
Atomic child dispatch, persistent waiting and completion are described in
[child dispatch](CHILDREN.md); workspace preparation extends the boundary below.

## Child workspace admission

Format 11 adds `child_workspace_preparation`, workspace policy/readiness and
inherited delegation-depth limits. A staged child with pending workspace cannot
be claimed or activated by its parent's checkpoint. Only the assigned gateway
may publish the matching immutable preparation receipt. Late facts are retained
without restoring the old parent lease.

The exact format-10 fixture preserves previous columns and separately verifies
the new defaults: root sessions retain their configured maximum, while legacy
children cannot infer additional delegation authority. Forward migration performs
the backfill and restores forced RLS before updating the marker.
See [workspace preparation](WORKSPACES.md).

## Format 12: durable cancellation

Format 12 adds `runtime_control_request`, `runtime_stop`, `runtime_continuation` and `gateway_cancellation_delivery`. Stop intent, subtree lease fencing, logical completion and the gateway outbox commit together. The restricted catalog helper only returns assigned operation coordinates; actual bodies use owner RLS. The frozen format-11 fixture preserves sessions, messages, jobs, Memory, children and operation admissions through success and injected migration rollback. See [control](CONTROL.md).

## Format 13: public activity

Format 13 adds owner-scoped public activity counters, items and immutable frames, plus an immutable message-to-execution Job association. Projection, source writes and each logical cursor commit together. The exact format-12 fixture preserves messages, parts, Memory and cancellation delivery state through success and failed-DDL rollback. See [activity](ACTIVITY.md) for snapshot pagination and privacy boundaries.

## Format 14: transient live progress

Format 14 adds `live_progress` and an atomic cleanup trigger on execution changes. Original message/Job binding, lease, snapshot digest and sequence are verified. Only active, fresh pending-message snapshots can be read. Exact format-13 migration preserves durable activity and Memory rows.

## Format 15: durable Workflow coordination

`runtime_workflow` and `runtime_workflow_node` retain the fixed DAG, native Job links, logical node capacity, ordered results and exact dependency inputs. Agent Workers never claim coordination Jobs. Current authorization and short transaction locks govern node admission. The format-14 fixture preserves sessions, messages, Memory, committed frames and live rows, including rollback before the marker. See [Workflow](WORKFLOW.md).
