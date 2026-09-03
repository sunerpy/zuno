# Frequently asked questions

## Is Zuno's Shell already OS-sandboxed?

It depends on the selected sandbox mode:

- `read-only` confines host writes.
- `workspace-write` (the default) confines writes to the workspace and trusted
  extra roots.
- `danger-full-access` deliberately uses the native shell as the Zuno user, with
  host filesystem, process, credentials, and networking. It is not an OS security
  boundary.

The two confined modes are OS-sandboxed only when the active platform backend
passes its full capability probe. Otherwise, Shell registration fails closed by
default. A trusted global, explicit, environment, CLI, or managed layer may set
`sandbox.onUnavailable` to `run-unconfined`; that allows only a write-capable
Agent's `workspace-write` request to use the native backend for a typed platform,
launcher-absence, or namespace/container-policy availability failure. Read-only
Agents, untrusted launchers, invalid policy/path, helper/internal errors, and
command execution errors never fall back.

Unavailable fallback is not the same as selecting `danger-full-access`. It keeps
the configured permission mode and all explicit denies and catastrophic-command
refusals, while recording that requested filesystem/network restrictions are not
OS-enforced. Explicit `danger-full-access` skips confinement discovery and also
sets effective permission mode to `allow_all`.

On supported Linux hosts, Zuno locates a fixed, root-owned system `bwrap` at
`/usr/bin/bwrap` or `/bin/bwrap`, probes the required namespaces, compiles the
effective Agent policy, and passes only an opaque `PreparedCommand` to the
process layer.

Verify a deployment with the same backend used by Shell:

```sh
zuno debug sandbox --mode workspace-write --network deny --check
```

The JSON report includes staged checks, canonical launcher path,
UID/GID/mode/device/inode, root ownership, group/world writability, special-bit
and file-capability trust, backend capabilities, and the exact probe failure.
It checks every launcher ancestor, then executes `/usr/bin/true` through the
same bubblewrap, capability-drop, `PR_SET_NO_NEW_PRIVS`, and seccomp path used by
Shell. `--check` exits unsuccessfully when the requested policy is not
deployable, even if configured fallback would allow runtime execution. The
report separately shows requested/effective authority, fallback eligibility,
resolution kind, and typed reason. Do not use `danger-full-access` for deployment
verification: that mode is reported as a native-execution bypass and
intentionally skips both launcher trust and the confinement self-test.

The Linux backend:

- mounts the host root read-only and overlays only the exact writable roots;
- reapplies existing `.git`, `.zuno`, `.agents`, external Git metadata, configured
  protected paths, and the helper executable as read-only;
- uses private `/tmp` and `/var/tmp`, fresh `/proc` and `/dev`, and separate user,
  PID, UTS, IPC, and—by default—network namespaces;
- drops every capability, sets `PR_SET_NO_NEW_PRIVS`, and installs seccomp before
  executing the requested shell; and
- blocks `ptrace`, `process_vm_readv`/`process_vm_writev`, `io_uring`, and network
  syscalls when network access is denied.

An Agent contract may narrow the configured mode. A read-only Agent therefore
has no writable host root even when the invocation selected a wider mode.
Write-capable Agents receive workspace write authority only under
`workspace-write`; Git metadata remains protected unless the specific command
passes the separate Git mutation authorization.

`permission.mode: "allow_all"` skips Zuno tool-approval prompts without changing
sandbox mode, including confirmable Shell-risk asks. TUI `--auto` remains narrower
and cannot satisfy a human-only request. Selecting `danger-full-access`
intentionally combines native host execution with an effective permission mode
of `allow_all`. Explicit permission denies, Git history freshness guards,
force-push lease requirements, and the Shell risk gate's catastrophic hard
denials remain terminal. Structured questions are not approvals and are exposed
only in Plan; ordinary Work asks directly at the turn boundary only when no safe
default exists.

Confined macOS and Windows modes currently return a typed unsupported-platform
error. They do not register Shell under the default `deny`; trusted
`run-unconfined` may allow a write-capable Agent to proceed natively, while
explicit `danger-full-access` remains available independently. See the
[Shell sandbox roadmap](design/shell-sandbox-roadmap.md).

## Why does `bwrap` fail with `loopback: Failed RTM_NEWADDR: Operation not permitted`?

`bwrap --unshare-net` creates a network namespace and initializes its loopback
device before running the requested command. `RTM_NEWADDR` is the netlink
operation that adds an address. `EPERM` at this point means an outer kernel, LSM,
container, or virtualized-host policy denied that namespace-local operation. It
does not mean Zuno should silently omit the network namespace.

### Current Ubuntu host diagnosis

On 2026-08-27, the current Ubuntu 24.04 EC2 host reported:

```text
/usr/bin/bwrap
bubblewrap 0.9.0
kernel.unprivileged_userns_clone = 1
user.max_user_namespaces = 252820
kernel.apparmor_restrict_unprivileged_userns = 1
```

The binary is root-owned with mode `0755` and has neither setuid nor file
capabilities.
The following independent probes then failed:

```sh
# User, mount, and PID namespace setup
/usr/bin/bwrap \
  --unshare-user --uid 0 --gid 0 \
  --unshare-pid --unshare-uts --unshare-ipc \
  --ro-bind / / \
  -- /usr/bin/true

# Network namespace and loopback setup
/usr/bin/bwrap \
  --unshare-user --uid 0 --gid 0 \
  --unshare-net \
  --ro-bind / / \
  -- /usr/bin/true
```

Before the AppArmor repair, the first returned
`setting up uid map: Permission denied`; the second returned
`loopback: Failed RTM_NEWADDR: Operation not permitted`. User namespaces were
numerically enabled, but Ubuntu's AppArmor restriction transitioned the
otherwise unconfined process into the generic `unprivileged_userns` profile.
That profile denied the capabilities `bwrap` needed to construct the sandbox.

After loading the dedicated profile and correcting its ownership to `root:root`,
both probes and Zuno's real backend E2E pass on this host. A Zuno process already
running inside another restricted sandbox can still be unable to create nested
namespaces; that outer runtime must allow the complete probe.

An outer user namespace may also show the host's root-owned `/usr/bin/bwrap` as
`uid=65534` (`nobody`). In that execution context Zuno correctly fails closed,
because it cannot prove the launcher is root-owned from its own authority view.
Run `zuno debug sandbox ... --check` directly on the intended host service
context to establish deployment readiness; do not reinterpret the mapped UID or
weaken the trust check.

### Recommended Ubuntu 24.04 repair

Ubuntu's `apparmor-profiles` package ships an extra
`bwrap-userns-restrict` profile specifically for this case. The profile grants
the trusted `/usr/bin/bwrap` executable the setup authority it needs, then stacks
its child into `unpriv_bwrap`, where capabilities are denied again.

Review the profile before enabling it, then install and load it:

```sh
sudo apt-get update
sudo apt-get install apparmor-profiles

sudo install -o root -g root -m 0644 \
  /usr/share/apparmor/extra-profiles/bwrap-userns-restrict \
  /etc/apparmor.d/bwrap-userns-restrict

sudo /usr/sbin/apparmor_parser -r \
  /etc/apparmor.d/bwrap-userns-restrict
```

Re-run both probes above. If either still fails, inspect the actual denial before
changing policy:

```sh
sudo journalctl -k --since '-10 minutes' \
  -g 'apparmor="DENIED"'
```

The profile attaches only to `/usr/bin/bwrap`. A distribution that installs
the binary elsewhere needs a separately reviewed path rule; do not trust a
workspace-controlled `PATH` entry or copy an arbitrary executable into the
trusted location.

Do not use any of the following as a production repair:

- globally setting `kernel.apparmor_restrict_unprivileged_userns=0`;
- adding setuid or file capabilities to `bwrap`;
- running Zuno or its container as fully privileged; or
- removing network isolation, capability dropping, protected-path rules, or
  seccomp from the backend requirements.

Enabling the AppArmor profile only removes this AppArmor blocker; all other
probe requirements still apply. Zuno continues to enforce
`PR_SET_NO_NEW_PRIVS`, capability dropping, seccomp policy, a read-only host
root, precise writable roots, and protected-subpath overlays.

### Containers, WSL, and other Linux hosts

Inside Docker, Podman, Kubernetes, a dev container, or another managed sandbox,
the outer runtime can independently deny `clone`, `unshare`, UID/GID mapping,
route-netlink operations, or nested mount/network namespaces. Fix that runtime's
specific seccomp, AppArmor/SELinux, user-namespace, and namespace settings, or
run Zuno in a dedicated VM/bare-metal environment where the complete probe
passes. Avoid blanket `--privileged` as a substitute for a reviewed policy.

For a deliberately unconfined automation container, a trusted host-level config
may instead set:

```json
{"sandbox":{"onUnavailable":"run-unconfined"}}
```

That choice is ignored for read-only Agents and never hides `debug sandbox
--check` failure. Project configuration cannot enable it, and managed policy may
still force `deny`.

WSL1 is unsupported. WSL2 is a Linux VM and may use the Linux
backend only when the same user, mount, PID, network, filesystem, and seccomp
probes pass.

### Upstream references

- [Bubblewrap project and security model](https://github.com/containers/bubblewrap)
- [Ubuntu restricted unprivileged user namespaces](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces)
- [Ubuntu AppArmor documentation](https://documentation.ubuntu.com/server/how-to/security/apparmor/)
- [AppArmor `bwrap-userns-restrict` profile](https://gitlab.com/apparmor/apparmor/-/blob/master/profiles/apparmor/profiles/extras/bwrap-userns-restrict)

## Why does a guarded program on Windows exit 125 before it runs?

Every child process Zuno supervises on Windows — a Shell command, an LSP server, a
plugin host, or a product agent — runs behind the child-process guard, and the
guard must watch its own parent so that a tree it started never outlives the Zuno
process that owns it. The workspace forbids `unsafe` code, so the guard cannot open
a process handle itself. It instead starts one Windows PowerShell helper from
`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`, and that helper holds
a real handle to the parent process and waits on it. A handle names the process
object rather than its PID, so a PID reused after the parent exits cannot
impersonate a living parent, and nothing is spawned per poll.

The helper is started before the supervised program, because a guard that cannot
watch its parent must not start work it could never clean up. If the helper cannot
start, the guard fails closed with exit code `125` and writes a diagnostic to its
stderr: `child-process guard failed: Windows PowerShell is required to watch the
guard's parent process; <path> does not exist` when the executable is missing, or
`child-process guard failed: cannot start the parent-process watch helper: <error>`
when it exists but cannot be launched. The supervised program never runs. The path
is resolved from `%SystemRoot%`, then `%windir%`, then the literal `C:\Windows`,
never from `PATH`, so a workspace-controlled `PATH` cannot substitute its own
helper; restore Windows PowerShell at that location and retry. The Shell tool reads a
`125` accompanied by the guard's diagnostic as an uncertain outcome by contract, as
described under [Tools](guide/tools.md), and the diagnostic is what tells you that
in this case nothing ran.

A helper that starts and later ends without a verdict is a different case. The
guard logs one diagnostic, leaves the program running, and supervises it only for
its own exit; an unknown parent state never kills a healthy command. Only an
unambiguous verdict — the helper reporting that the parent has exited, or that the
parent was already gone when the helper armed — terminates the supervised tree.

## Why does a Kiro prompt fail with `unsupported_content_block_projection`?

The 2026-08-28 `kiro-provider` build accepts consecutive all-text Responses
blocks. It preserves their boundaries in its canonical request and concatenates
them byte-for-byte, with no separator, only at Kiro's scalar text boundary.
A Zed `resource_link` plus ordinary text therefore no longer needs a Zuno-side
single-text projection.

Use the normal provider options:

```json
{
  "provider": {
    "kiro-local": {
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "maxTokens": null,
        "timeout": false,
        "headerTimeout": 330000,
        "chunkTimeout": 210000
      }
    }
  }
}
```

Remove a stale `responsesTextBlocks: "single"` setting when upgrading. That
generic Zuno compatibility mode joins text with one blank line, so it changes
the bytes compared with the provider's current lossless projection.

The timeout values let kiro-provider's 300-second request deadline and
180-second stream-idle deadline fire first. Zuno then receives a typed gateway
error instead of cancelling the connection at the same boundary.

The error remains intentional when several text blocks are interleaved with an
image, document, tool content, or another non-text block whose ordering cannot
be represented by Kiro's single text field. Zuno and the provider fail closed
instead of reordering or flattening the prompt. If consecutive pure-text blocks
still fail, verify the running provider binary rather than adding a hidden
prompt rewrite.
