# Frequently asked questions

## Is Zuno's Shell already OS-sandboxed?

It depends on the explicitly selected mode:

- `read-only` confines host writes.
- `workspace-write` confines writes to the workspace and trusted extra roots.
  It is the default.
- `danger-full-access` deliberately uses the native shell as the Zuno user, with
  host filesystem, process, credentials, and networking. It is not an OS security
  boundary.

The two confined modes are OS-sandboxed only when the active platform backend
passes its complete capability probe. Otherwise Shell registration fails closed;
Zuno never turns a failed `read-only` or `workspace-write` request into
`danger-full-access`. Use `zuno --sandbox danger-full-access` only when native
host execution is intentionally required.

On supported Linux hosts, Zuno discovers a fixed, root-owned system
`/usr/bin/bwrap` or `/bin/bwrap`, probes the required namespaces, compiles the
effective Agent policy, and passes only an opaque `PreparedCommand` to the process
layer.

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

`permission.mode: "allow_all"` skips every Zuno tool-approval prompt without
changing sandbox mode. TUI `--auto` remains narrower and cannot satisfy a
human-only request. Selecting `danger-full-access` intentionally combines native
host execution with an effective permission mode of `allow_all`, so Zuno does
not open approval cards in TUI, ACP, server, or headless surfaces. Explicit
permission denies and the Shell risk gate's catastrophic hard denials remain
terminal; they fail directly instead of asking. Structured user questions are
not approvals and may still be shown.

Confined macOS and Windows modes currently return a typed unsupported-platform
error and do not register Shell. Explicit `danger-full-access` remains available
through the native process backend. See the
[Shell sandbox roadmap](design/shell-sandbox-roadmap.md).

## Why does `bwrap` fail with `loopback: Failed RTM_NEWADDR: Operation not permitted`?

`bwrap --unshare-net` creates a network namespace and initializes its loopback
device before it executes the requested command. `RTM_NEWADDR` is the netlink
operation used to add an address. `EPERM` at this point means an outer kernel,
LSM, container, or virtualized-host policy denied the namespace-local network
administration operation. It is not evidence that Zuno should silently omit the
network namespace.

### Current Ubuntu host diagnosis

On 2026-08-27, the current Ubuntu 24.04 EC2 host reported:

```text
/usr/bin/bwrap
bubblewrap 0.9.0
kernel.unprivileged_userns_clone = 1
user.max_user_namespaces = 252820
kernel.apparmor_restrict_unprivileged_userns = 1
```

The binary is root-owned mode `0755`, with neither setuid nor file capabilities.
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

Before the AppArmor repair, the first returned `setting up uid map: Permission denied`; the second returned
`loopback: Failed RTM_NEWADDR: Operation not permitted`. User namespaces are
numerically enabled, but Ubuntu's AppArmor restriction transitions an otherwise
unconfined process into the generic `unprivileged_userns` profile, which denies
the capabilities `bwrap` needs while constructing the sandbox.

After loading the dedicated profile and correcting its ownership to `root:root`,
both probes and Zuno's real backend E2E pass on this host. A Zuno process already
running inside another restricted sandbox can still be unable to create nested
namespaces; that outer runtime must allow the complete probe.

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

The profile attaches to exactly `/usr/bin/bwrap`. A distribution that installs
the binary elsewhere needs a separately reviewed path rule; do not trust a
workspace-controlled `PATH` entry or copy an arbitrary executable into the
trusted location.

Do not use any of the following as a production repair:

- globally setting `kernel.apparmor_restrict_unprivileged_userns=0`;
- adding setuid or file capabilities to `bwrap`;
- running Zuno or its container as fully privileged; or
- removing network isolation, capability dropping, protected-path rules, or
  seccomp from the backend requirements.

Enabling the AppArmor profile only makes the namespace probe eligible to pass.
Zuno still enforces
`PR_SET_NO_NEW_PRIVS`, capability drop, seccomp policy, read-only host root,
precise writable roots, and protected-subpath overlays.

### Containers, WSL, and other Linux hosts

Inside Docker, Podman, Kubernetes, a dev container, or another managed sandbox,
the outer runtime can independently deny `clone`, `unshare`, UID/GID mapping,
route-netlink operations, or nested mount/network namespaces. Fix that runtime's
specific seccomp, AppArmor/SELinux, user-namespace, and namespace settings, or
run Zuno in a dedicated VM/bare-metal environment where the complete probe
passes. Avoid blanket `--privileged` as a substitute for a reviewed policy.

WSL1 must be rejected as unsupported. WSL2 is a Linux VM and may use the Linux
backend only when the same user, mount, PID, network, filesystem, and seccomp
probes pass.

## Why does a Zed attachment fail through Kiro with `unsupported_content_block_projection`?

Zed can send one ACP prompt as separate typed blocks, for example a
`resource_link` followed by the user's text. A standards-compatible Responses
endpoint accepts both as separate `input_text` blocks. The current Kiro gateway
projects one message onto one upstream text field and rejects that standard
shape with HTTP 400.

Declare the limitation explicitly on that compatible provider:

```json
{
  "provider": {
    "kiro-local": {
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "maxTokens": null,
        "responsesTextBlocks": "single"
      }
    }
  }
}
```

Zuno then joins only the provider-bound text projections with one blank line.
The durable message still stores the resource link and user text as separate
typed parts, and inline image blocks remain separate. The setting is generic
and opt-in; do not apply it to an endpoint that supports normal multi-block
Responses input.

### Upstream references

- [Bubblewrap project and security model](https://github.com/containers/bubblewrap)
- [Ubuntu restricted unprivileged user namespaces](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces)
- [Ubuntu AppArmor documentation](https://documentation.ubuntu.com/server/how-to/security/apparmor/)
- [AppArmor `bwrap-userns-restrict` profile](https://gitlab.com/apparmor/apparmor/-/blob/master/profiles/apparmor/profiles/extras/bwrap-userns-restrict)
