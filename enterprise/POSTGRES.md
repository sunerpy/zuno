# PostgreSQL preview persistence

This adapter implements scoped sessions, runtime Jobs, organization approvals and
the shared kernel's `TurnPersistence` port. `AgentApplication` and the ordinary
bounded driver consume the same contracts for SQLite and PostgreSQL. PostgreSQL
Memory, authenticated Worker transport and OAuth2/OIDC HTTP entry points remain
subsequent work; this library alone does not register an enterprise server or worker.

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
authentication credential: the host must authenticate and authorize first.

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

The current materializer supports the root text inputs accepted by the runtime
application port. Remote steering, attachments, distributed human/child waits,
gateway receipts and their consumers are not enabled by this adapter. A local
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
Lease expiry records `uncertain` and retains the logical session hold. Other
sessions remain eligible. Automatic takeover of in-flight operations still needs
the environment gateway and receipt reconciliation; this adapter does not replay
side effects.

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
