# PostgreSQL preview persistence

This adapter currently implements `SessionPersistence`: logical workspaces,
private sessions, cursor paging, idempotent text admission and durable events.
`AgentApplication` consumes the same port for SQLite and PostgreSQL. PostgreSQL
Job/Memory state, remote engine access and Entra HTTP entry points are subsequent
work; this library alone does not register an enterprise server or worker.

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
