# Rootless execution environments

The preview `zuno-environment` library implements `EnvironmentProvider` and
`OperationGateway`. It is not yet registered as an Agent tool or enterprise launch
command. Durable approval waits, the authenticated gateway service and Worker
profile assembly must be connected before that registration.

## Isolation and ownership

The provider accepts only a rootless Docker Unix socket with systemd cgroups and
memory, CPU and PID enforcement. Images require an immutable digest. Each session
workspace is a named volume with owner/environment/definition labels. Each command
uses a separately named container bound to its stable operation and request digest.
Gateway restarts can inspect that original container and read its retained logs.

Command containers have a read-only root filesystem, no network, no capabilities,
no-new-privileges, bounded memory/CPU/PIDs and a non-executable temporary mount.
Only the workspace volume is writable. The environment does not inherit the
gateway's process environment, database credentials, model keys or Docker socket.
Network access and wall-clock watchdog enforcement are not enabled by this provider.
CPU is a rate limit, not a cumulative CPU-time budget.

The gateway data directory must be private (`0700`). Its SQLite ledger and snapshot
files are separate from the main Zuno database. An exclusive instance lock prevents
two gateway processes from controlling one ledger. Per-environment control locks
coordinate starts, cancellation, snapshots and release without locking other
environments.

## Operations and recovery

The ledger commits start admission before calling Docker. That transition is
single-use. A stale `created` observation cannot turn `starting` back into a new
start opportunity. If an acknowledgement is lost, the gateway inspects the original
container; it never retries Docker start. A confirmed exit records its exit code
once. An unconfirmable start or missing in-flight container stays `uncertain` and
keeps the workspace occupied for inspection.

`OperationAuthority` is mandatory. `OrganizationOperationAuthority` reuses the
existing `RuntimeStore` and `OrganizationStore` to check a stable approval binding,
resolved environment revision, command hash and current lease. There is no
production allow-all default. This adapter is for a trusted composition root;
separated gateway deployment still requires authenticated remote authority ports.
Cancelling currently requires that authority as well; operator cancellation after
lease revocation remains part of the unfinished distributed control integration.

Output pages use logical byte offsets and a prefix digest, including stdout/stderr
identity. Reads stream through Docker frames with bounded page memory. A missing or
changed prefix conflicts instead of silently splicing unrelated output. Output is
available while the command container is retained; exporting durable long-term
artifacts and disk/log quotas remain deployment integration work.

## Snapshots and release

Snapshots require an idle environment at the expected revision. The archive is
streamed to a private pending file, checked by digest/length and validated as a
workspace-only tar archive before its manifest becomes visible. Special files,
set-id modes and escaping links are rejected. Snapshot size is currently capped at
512 MiB.

Forking verifies the stored snapshot identity and restores into a new volume. The
tar stream is rebased to the writable workspace mount without extraction into the
gateway host. Parent files remain unchanged. Release checks ownership labels,
removes only the environment's retained command containers and volume, then commits
a tombstone. Repeating release is safe; acquire cannot silently recreate a released
environment or a workspace whose volume disappeared.

Snapshot artifact storage is currently the gateway's private filesystem. A
replaceable remote artifact backend, durable recovery of partially provisioned
forks and snapshot retention/garbage collection remain required for complete
enterprise deployment.

## Native validation

For a pre-existing isolated rootless daemon:

```sh
ZUNO_ROOTLESS_DOCKER_SOCKET=/run/user/1000/zuno-preview/docker.sock \
  python3 scripts/check_enterprise_docker.py
```

Without that variable the script starts a private rootless daemon, using a unique
socket/data/exec directory and the user's systemd D-Bus. It does not change Docker
contexts or stop the host daemon. `uidmap`, rootless Docker extras and a delegated
user systemd session are prerequisites.

The test verifies read-only root, network and resource limits; immutable command
identity; one execution after reopening the ledger; output paging and prefix
validation; cancellation; snapshots; fork isolation; and tombstone release.
The preview CI/release gate runs it natively on Linux amd64 and arm64. Only completed
CI results count as platform evidence.

See [中文](ENVIRONMENTS.zh.md), [authorization](AUTHORIZATION.md) and [status](STATUS.md).

## Gateway transport

The backend now has an authenticated Worker/control-plane HTTP adapter, described
in [gateway transport](GATEWAY.md). A request ticket does not bypass operation
approval. The combined Docker gate also verifies the real HTTP path against a
temporary PostgreSQL/TLS control plane. Role startup, Agent tool assembly and
administrative cancellation remain separate delivery items.
