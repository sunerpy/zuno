# Shell sandbox roadmap

Status: the three-mode authority model, explicit native backend, and trusted
unavailable-backend resolver are complete. Linux confinement was host-E2E
verified on 2026-08-27. Confined macOS and Windows backends remain future work
and fail closed by default.

## Decision

Zuno will adopt Codex's separation of approval from confinement and DSH's concise
public mode vocabulary, not copy one platform wrapper in isolation. Approval
authorizes an intent. A sandbox backend decides which filesystem, network,
process, and credential effects are technically possible. Neither substitutes
for the other.

The stable modes are `read-only`, `workspace-write`, and
`danger-full-access`. The first two require a proved confinement backend.
`danger-full-access` is a separate explicit native-execution policy. A trusted
layer may independently set `sandbox.onUnavailable` to `run-unconfined`, allowing
only a `workspace-write` request with a typed deployment-unavailable failure to
use the native backend. The default remains `deny`, and read-only Agent contracts
never fall back. A trusted layer may also select the backend outright with
`sandbox.backend: native`: every request, read-only included, then resolves to the
native backend before any discovery as `SandboxResolutionKind::TrustedNative`,
with the requested contract recorded as unenforced and the configured permission
mode kept. That is an explicit host declaration, not a fallback, and a project
layer cannot make it.

The comparison baseline is OpenAI Codex commit
[`a26f1806a`](https://github.com/openai/codex/commit/a26f1806a4f4b8cfec2ea1be129963815a61e58c)
and its [sandbox and approvals documentation](https://learn.chatgpt.com/docs/agent-approvals-security).
Codex uses bubblewrap plus an in-sandbox seccomp helper on Linux, Seatbelt on
macOS, and restricted-token/elevated backends on Windows. Zuno will reuse the
architecture and invariants, not Codex product protocols or historical `bash`
compatibility names.

## Required execution chain

```text
validated config
  -> trusted maximum mode intersected with Agent capability
  -> immutable execution-authority snapshot for the issuing step
  -> typed approval decision
  -> SandboxResolver discovery, capability check, and real deployment verification
  -> one immutable confined, explicit-native, or unavailable-fallback resolution
  -> compiled platform policy
  -> PreparedCommand
  -> process spawn, cancellation, output, and typed denial
```

Every Shell command reaches the process layer as a `PreparedCommand`. Resolution
happens once before a command can be prepared; `prepare` and command execution
never switch backends. A confined request therefore cannot reach process spawn
as raw argv unless a trusted `run-unconfined` policy has already produced an
explicit `UnavailableFallback` resolution for an eligible deployment failure.
The explicit `danger-full-access` backend and unavailable fallback both preserve
the native program and arguments, produce a `PreparedCommand`, and never claim
confinement.

## Capability model

The effective snapshot must include:

- read, write, and deny roots after workspace and symbolic-root resolution;
- network policy and any one-call additional permissions;
- approval mode and reviewer policy as they existed when the call was issued;
- working directory, environment policy, TTY mode, and command identity;
- protected subpaths inside writable roots, including `.git`, `.zuno`, `.agents`,
  and resolved external Git directories;
- the effective `read-only`, `workspace-write`, or `danger-full-access` mode;
- the requested mode and network authority before resolution;
- the resolution kind and typed unavailable reason, when present;
- the selected backend and its declared capabilities.

An approval cache key must include every field that changes authority. A later
turn or profile replacement must not retroactively broaden an already-issued call.

## Platform backends

### Linux

The first complete backend may depend on a system `bwrap`, but it must:

1. resolve `bwrap` outside the workspace to prevent PATH injection;
2. probe user, mount, PID, and optional network namespace support before advertising
   the backend;
3. mount the host root read-only, layer exact writable roots, and reapply protected
   descendants as read-only or denied;
4. drop capabilities, set `PR_SET_NO_NEW_PRIVS`, and install a seccomp policy before
   executing the user command;
5. block process-inspection escape paths such as `ptrace` and
   `process_vm_readv`/`process_vm_writev`;
6. preserve process-tree cancellation and at-most-once background execution;
7. reject WSL1 and unsupported architectures with typed errors.

On 2026-08-27, this Ubuntu 24.04 EC2 host initially failed two independent namespace
probes. User, mount, and PID setup stopped at
`setting up uid map: Permission denied`; network setup stopped at
`loopback: Failed RTM_NEWADDR: Operation not permitted`. Unprivileged user
namespaces were numerically enabled, while
`kernel.apparmor_restrict_unprivileged_userns=1` was active and no dedicated
`bwrap` AppArmor profile was loaded. This is typed deployment-unavailable
evidence. It activates native execution only when a trusted layer has explicitly
selected `run-unconfined`; otherwise it remains a fail-closed error. Loading
Ubuntu's dedicated `bwrap-userns-restrict` AppArmor profile with root ownership
restored both probes.
The production backend and a real E2E then verified writable-workspace isolation,
protected descendants, symlink escape denial, network denial, zero capabilities,
`NoNewPrivs`, `ptrace`, and `process_vm_readv`. See the
[sandbox FAQ](../faq.md).

Backend discovery is cached per process. `system_backend` serves bubblewrap
discovery from a process-local cache keyed by the canonical workspace and the
helper executable, so the first Shell resolution in a process pays for the
`bwrap --help` and namespace probes and later resolutions do not. A hit is handed
out only after the trusted launcher, the trusted no-op executable, and the helper
are re-inspected on disk; an entry that no longer passes is evicted and the host
is probed again, which is how a bubblewrap upgrade or a replaced helper is picked
up. A failed discovery is returned and never cached, so a transient probe failure
cannot pin the process to "unavailable" or, under a trusted `run-unconfined`
policy, to native execution. Nothing is persisted or shared across processes;
another process must prove the host again. Two things deliberately stay outside
the cache: deployment verification of the effective policy still runs per
resolution, because it proves that this policy deploys rather than that the host
is capable in general, and `probe_system_backend` bypasses the cache so the
deployment report behind `zuno debug sandbox` describes the host as it is now.

### macOS

Compile a deny-by-default SBPL profile and invoke the fixed
`/usr/bin/sandbox-exec` path. Paths enter through parameters rather than string
interpolation. Writable-root symlink traversal and network endpoints require
explicit tests. Until this backend lands, confined modes fail closed by default.
A trusted `run-unconfined` policy may allow a write-capable Agent to use the
native process backend, while read-only Agents still refuse; a trusted
`sandbox.backend: native` selection runs every Agent natively with the permission
mode kept.

### Windows

Use a restricted identity/token, Job Object containment, filesystem ACLs, and a
network enforcement backend. A backend that cannot express deny-read, split roots,
or requested network restrictions must reject the profile. WSL2 uses the Linux
backend; WSL1 remains unsupported. Until the restricted backend lands, confined
modes fail closed by default; a trusted `run-unconfined` policy may opt a
write-capable Agent into native execution and records the unsupported platform
as its fallback reason, and a trusted `sandbox.backend: native` selection opts
every Agent into native execution with no fallback reason to record.

## Registration gate

The `plan`, `explorer`, `librarian`, `oracle`, and `looker` profiles may receive
the general Shell tool only after all of the following are true for the active
platform. `librarian` retains its distinct network-research policy even when its
local process execution is read-only:

- the backend probe succeeds;
- a read-only policy can be compiled without dropping a deny or protected path;
- attempted writes inside the workspace, outside the workspace, and through
  symlinks fail in integration tests;
- process and network restrictions pass platform tests;
- cancellation, timeout promotion, restart reconciliation, logs, usage, and
  durable tool events still work through `PreparedCommand`;
- unavailable backends produce either a clear startup/tool-registration error or
  a trusted native fallback with a host warning and durable `runtime.sandbox`
  section; read-only profiles always take the error path unless a trusted layer
  selected `sandbox.backend: native`, which is not a fallback and carries the same
  warning and durable section with `trusted_native` in the record.

The Linux gate now passes on the verified host. Read-only Agent contracts compile
to `SandboxMode::ReadOnly`; write-capable Agents receive at most the trusted
configured mode. If startup discovery or deployment verification fails for a
confined mode, the resolver classifies the typed error before tool publication.
Unsupported platforms, a missing trusted launcher, missing required launcher
capabilities, and namespace/container-policy denial are eligible for trusted
fallback. An untrusted launcher, invalid policy/path, seccomp/helper/internal
failure, generic process I/O failure, and command preparation/execution failure
remain terminal. `danger-full-access` skips confinement discovery because the
user selected that separate authority explicitly, and `sandbox.backend: native`
skips it because a trusted layer selected the backend explicitly.

## Delivery phases

1. Persist the issuing-step execution-authority snapshot. The current durable
   authority records policy identity; a richer typed HITL receipt remains follow-up.
2. Add the platform-neutral sandbox facade and require `PreparedCommand` at the
   process boundary. **Complete.**
3. Complete and E2E-test Linux bubblewrap plus seccomp. **Complete.**
4. Add the typed three-mode configuration, trusted-source ceiling, CLI override,
   explicit native backend, and durable mode metadata. **Complete.**
5. Add a typed pre-command resolver, trusted `onUnavailable` policy, requested
   versus effective authority v3, host/model warnings, and legacy v2 recovery.
   **Complete.**
6. Complete macOS and Windows confined backends, with explicit
   unsupported-platform errors.
7. Dynamically expose Shell to read-only Agents only when the selected backend
   satisfies the registration gate. **Complete on Linux.**

Each phase must update `docs/harness-runtime.md`, use TDD, and finish with the
workspace gates. Interface-only or prompt-only confinement is not a completed phase.
