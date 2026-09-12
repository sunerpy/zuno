# Enterprise executable deployment

The development binary `zuno-enterprise` has real control-plane, Worker, gateway,
migration and identity handlers. It supports Linux amd64 and arm64. Preview
publication remains disabled until the remaining enterprise acceptance is complete.
Personal `zuno` installation, configuration and platform support are unchanged.

Build with `cargo build -p zuno-enterprise`. Start each role with:

```sh
target/debug/zuno-enterprise --config /etc/zuno-enterprise-preview/worker.json
```

Configuration is explicit JSON with `stateDirectory` and a tagged `service`.
Zuno personal/project configuration and its credential store are not loaded.
Native cloud providers may use their standard workload credential chain when no
explicit binding is supplied. Paths must be absolute.
New state directories are created private; existing shared directories are refused.
Operational logs remain under the role's state directory. Run each instance with
its own directory; the gateway ledger is a single-owner database.

## Definitions and roles

[definition.json](examples/definition.json), [worker.json](examples/worker.json)
and [gateway.json](examples/gateway.json) are typed templates. Replace example
endpoints, model identity, certificates and secret references before use. The
Alpine fixture image demonstrates argv execution; a real task needs an approved
image with the required tools. The initial workspace is empty.

Keep identical immutable definition bytes on the control plane and compatible
Workers. ID, version and a canonical content digest bind a Job to its Agent,
model, budget and environment. Changing a definition requires a new version.
Physical credential files are separate from that digest. A restarted Worker may
read rotated model credentials without resetting a Job's stored budget.

Optional `delegation` lists exact child `{id,version,sha256}` references with
`maximumDepth` and `maximumChildren`. Compute each reference with
`zuno-enterprise --definition-ref /absolute/child.json`, install both definitions
on the control plane and Workers, and increment parent versions when references
change. Valid target catalogs mount native `task`; child workspaces are prepared
before execution. See [workspace preparation](WORKSPACES.md).

The Worker mounts a `HarnessProfile` containing the shared `AgentDriver`, resolves
native provider factories, and calls bounded advances. Models have an explicit
context limit and output cap. Token/time/tool allowances use resumed counters;
unknown provider usage stops spending. Native cloud transports use platform trust;
the compatible transport also accepts a configured model CA file.

The first gateway tool is `environment_command`, with an argv array and the
assigned `/workspace` directory. Shell syntax needs an explicit shell executable.
This is a separate tool; it does not replace the personal shell's richer contract.
Preparation may wait for human approval. Actual submission follows durable
handoff, returns an operation wait and releases Worker capacity. A lost response
queries the original operation, without repeating submission. Unconfirmed
outcomes retain the inspection obligation.

The gateway owns the rootless Docker socket and receipt ledger. Its supervisor
delivers captured results with bounded backoff and reconstructs work after restart.
Execution containers receive no runtime database credential, global model key or
Docker socket. See [environment requirements](ENVIRONMENTS.md).

## Control-plane configuration

`service.kind: "control_plane"` requires:

| Field | Meaning |
| --- | --- |
| `tenantId` | Fixed organization namespace |
| `tls` | Listen address, PEM certificate/private-key paths and connection limit |
| `database` | `urlFile`, optional `rootCertificate`, and `maxConnections` |
| `userIdentity` / `serviceIdentity` | Separate verified user and workload policies |
| `workers` | Exact verified `tenantId`, `principalId`, `clientId` subjects |
| `gateways` | A `subject` plus its assigned `gatewayId` |
| `jobKeys` / `gatewayKeys` | Separate `active` key IDs and `keys: [{id,path}]` |
| `definitions` | All retained immutable definition files |
| `activeDefinitions` | Explicit `{id,version}` selections for new sessions |
| `leaseMillis` | Database lease lifetime, 1000–300000; default 30000 |
| `browser` | Optional OIDC BFF configuration |
| `webAssetsDirectory` | Optional absolute Web bundle directory; requires `browser` |
| `memory` | Optional transaction concurrency/deadline and character budgets; defaults documented in [Memory](MEMORY.md) |

Key files contain raw key bytes. The HMAC authorities validate key sizes and
rotation sets; do not place key values in the definition or request DTOs.
Retain older definitions for waiting Jobs during an upgrade.

Identity configuration is tagged with `kind`: `jwt`, `entra` or `introspection`.
JWT takes `config` from the [generic access-token policy](AUTHENTICATION.md) and an
optional `rootCertificate` file. Entra takes its validated `config`.
Introspection takes `config`, `clientId` and `clientSecretFile`, using the
existing authenticated introspection adapter and platform trust. The issuer's
actor claims and client/scopes must be configured for that issuer; they are not
universal OAuth2 claims.

An optional browser block supplies `authority`, `clientId`, `clientSecretFile`,
`redirectUri`, `scopes`, optional `rootCertificate` and `encryptionKeys`. It uses
OIDC code/PKCE and the same verified user policy. See [BFF contracts](BROWSER.md).
The experimental [Web workbench](WEB.md) can be served at `/app/` when a separate
bundle is supplied. App design and UI delivery are deferred until the Penpot
design phase; current preview archives contain the backend only. The client
contract workflow still checks Rust schemas and the SDK. Experimental browser
checks in the client and Docker workflows require `include-experimental-web:
true`; backend Docker checks run by default on both Linux architectures. Static
resource deployment does not enable other routes or weaken authentication.

## Setup and identity

Use a separate configuration with `service.kind: "identity"`, `verifier` and
`accessTokenFile` to verify a token and print its opaque identity coordinates.
This outputs no token. Use those coordinates for the service allowlist and
organization membership; email/display names are not identity keys.

`service.kind: "migrate"` takes `database`, `runtimeRole` and optional `bootstrap`.
The migration URL file uses the schema-owner credential, separately from the
runtime URL file. Bootstrap contains `administrator` (a `PrincipalKey`) and the
validated organization `policy`. It creates an organization once and cannot undo
later revocation. Provision the roles as described in [PostgreSQL](POSTGRES.md).

Service tokens are read from private files on each request so an identity sidecar
can rotate them. Workers receive only their state API token and model bindings.
The control plane exposes the [public application API](APPLICATION.md); the
internal protocols and grants remain separate.

## Shutdown, upgrades and evidence

SIGTERM stops admission and drains bounded work. TLS connections and receipt
delivery have bounded shutdown. Gateway shutdown does not declare its external
commands complete; their ledger and containers remain independently recoverable.

Worker protocol 9 carries checkpoint schema 4. The driver reads schema 3 only
where it proves an unsubmitted wait; it never reinterprets an old record as
submitted execution. Drain incompatible Workers before changing the control
protocol and retain their definitions and durable data. Full rolling-upgrade and
backup/restore acceptance remain P6 work.

`python3 scripts/check_enterprise_docker.py` now runs the binary with a control
plane, a gateway and two independent Worker processes. A real TLS RSA issuer,
native compatible model transport, PostgreSQL and rootless Docker verify two
users, private Memory reads and updates, fresh prompts after changes, Memory-use
revocation during approval waits, parent/child workspace forks, fourteen model
requests, four command approvals, both Workers participating, one operation per
command and SIGTERM cleanup. This is fixture provider evidence,
not a live Entra tenant.

Remaining work includes richer workspace provisioning, background/shared Memory,
approved child merges, distributed Workflow/Council, remaining Web/ACP features
and the full failure matrix. No preview release is enabled by building this binary.

See [中文](DEPLOYMENT.zh.md), [platforms](PLATFORMS.md) and [status](STATUS.md).

Worker `liveMillis` defaults to 500 milliseconds, accepts 100–5000, or null to disable transient publication. Live progress is a bounded replaceable snapshot; it never holds model execution or replaces committed history. See [activity](ACTIVITY.md).

Optional `workflows` installs bounded templates over the existing child target catalog. They use a non-model coordination Job, independent node workspaces and strict command approvals. See [Workflow configuration and limits](WORKFLOW.md).

Agent `mode` defaults to `agent`, preserving existing normalized definition digests.
`mode: "completion"` uses the same bounded model driver with no tools or resident
Memory context. Omit `environment`, `delegation` and `workflows` for that profile.
The control plane does not assign a gateway to it and excludes its configuration
from Worker Memory access. Completion profiles can run as owned root sessions or
as explicitly configured model-only children. This is a backend primitive for
internal completions; it does not enable distributed Council coordination.
