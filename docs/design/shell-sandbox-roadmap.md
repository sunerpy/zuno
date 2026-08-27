# Shell sandbox roadmap

Status: the three-mode authority model and explicit native backend are complete.
Linux confinement was host-E2E verified on 2026-08-27. Confined macOS and
Windows backends remain future work and fail closed.

## Decision

Zuno will adopt Codex's separation of approval from confinement and DSH's concise
public mode vocabulary, not copy one platform wrapper in isolation. Approval
authorizes an intent. A sandbox backend decides which filesystem, network,
process, and credential effects are technically possible. Neither substitutes
for the other.

The stable modes are `read-only`, `workspace-write`, and
`danger-full-access`. The first two require a proved confinement backend.
`danger-full-access` is a separate explicit native-execution policy and is never
an availability fallback.

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
  -> sandbox backend capability check
  -> compiled platform policy
  -> PreparedCommand
  -> process spawn, cancellation, output, and typed denial
```

Every Shell command reaches the process layer as a `PreparedCommand`. A command
that requires confinement must never reach process spawn as raw argv. If no
backend can faithfully express the effective policy, preparation returns a typed
error and execution stops. The explicit `danger-full-access` backend also
produces a `PreparedCommand`, but preserves the native program and arguments and
does not claim confinement.

## Capability model

The effective snapshot must include:

- read, write, and deny roots after workspace and symbolic-root resolution;
- network policy and any one-call additional permissions;
- approval mode and reviewer policy as they existed when the call was issued;
- working directory, environment policy, TTY mode, and command identity;
- protected subpaths inside writable roots, including `.git`, `.zuno`, `.agents`,
  and resolved external Git directories;
- the effective `read-only`, `workspace-write`, or `danger-full-access` mode;
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
`bwrap` AppArmor profile was loaded. This is deployment evidence that the
backend was unavailable, not permission to weaken it. Loading Ubuntu's dedicated
`bwrap-userns-restrict` AppArmor profile with root ownership restored both probes.
The production backend and a real E2E then verified writable-workspace isolation,
protected descendants, symlink escape denial, network denial, zero capabilities,
`NoNewPrivs`, `ptrace`, and `process_vm_readv`. See the
[sandbox FAQ](../faq.md).

### macOS

Compile a deny-by-default SBPL profile and invoke the fixed
`/usr/bin/sandbox-exec` path. Paths enter through parameters rather than string
interpolation. Writable-root symlink traversal and network endpoints require
explicit tests. Until this backend lands, confined modes fail closed; only an
explicit `danger-full-access` invocation uses the native process backend.

### Windows

Use a restricted identity/token, Job Object containment, filesystem ACLs, and a
network enforcement backend. A backend that cannot express deny-read, split roots,
or requested network restrictions must reject the profile. WSL2 uses the Linux
backend; WSL1 remains unsupported. Until the restricted backend lands, only an
explicit `danger-full-access` invocation uses the native process backend.

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
- unavailable backends produce a clear startup or tool-registration error and no
  Shell definition reaches the model.

The Linux gate now passes on the verified host. Read-only Agent contracts compile
to `SandboxMode::ReadOnly`; write-capable Agents receive at most the trusted
configured mode. If startup probes or effective-policy compilation fail for a
confined mode, Shell is not registered and assembly returns the typed backend
error. `danger-full-access` skips confinement discovery only because the user
selected that separate authority explicitly.

## Delivery phases

1. Persist the issuing-step execution-authority snapshot. The current durable
   authority records policy identity; a richer typed HITL receipt remains follow-up.
2. Add the platform-neutral sandbox facade and require `PreparedCommand` at the
   process boundary. **Complete.**
3. Complete and E2E-test Linux bubblewrap plus seccomp. **Complete.**
4. Add the typed three-mode configuration, trusted-source ceiling, CLI override,
   explicit native backend, and durable mode metadata. **Complete.**
5. Complete macOS and Windows confined backends, with explicit
   unsupported-platform errors.
6. Dynamically expose Shell to read-only Agents only when the selected backend
   satisfies the registration gate. **Complete on Linux.**

Each phase must update `docs/harness-runtime.md`, use TDD, and finish with the
workspace gates. Interface-only or prompt-only confinement is not a completed phase.
