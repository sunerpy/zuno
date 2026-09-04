# Permissions and sandboxing

Zuno has two independent gates in front of anything that touches your machine. The
**sandbox** decides what a command is physically able to do. **Permissions** decide
whether the call is admitted in the first place. They are not alternatives, and
neither one substitutes for the other.

## The two gates

```
model asks to run a command
        │
        ▼
  permissions ── deny ──▶ refused, nothing runs
        │
      allow / after asking you
        │
        ▼
  sandbox resolver
        ├── confined backend ready ──▶ command runs, confined
        ├── eligible unavailable error + trusted fallback
        │                              └──▶ warning, then native execution
        └── otherwise ───────────────▶ refused, nothing runs
```

A permission decision is about intent: should this call happen. A sandbox is about
capability: what the process can reach once it does. Allowing a call does not widen
the sandbox, and a permissive sandbox does not erase explicit denies or hard safety
checks. The explicit `danger-full-access` mode also selects effective
`allow_all`, so it suppresses ordinary approval prompts by design.

## Sandbox modes

Set with `sandbox.mode` in configuration, or `--sandbox` on the command line.

| Mode | Filesystem | Network default |
| --- | --- | --- |
| `read-only` | Reads the host filesystem, no host writes | `deny` |
| `workspace-write` | Writes the active workspace and explicitly trusted extra roots | `deny` |
| `danger-full-access` | Runs as the Zuno user with the host filesystem, processes and network | host networking |

`workspace-write` is the default when `sandbox.mode` is absent. A read-only agent
contract still narrows it — see [Agents](/guide/agents).

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "deny"
  }
}
```

### Choosing native execution

There are three different ways to run without OS confinement. Choose the one whose
meaning matches the deployment:

| Intent | Setting | What happens |
| --- | --- | --- |
| Require confinement | `workspace-write` plus `onUnavailable: "deny"` | The default. An unavailable backend stops Shell assembly. |
| Prefer confinement, but permit an unavailable-only fallback | `workspace-write` plus `onUnavailable: "run-unconfined"` | Zuno probes and verifies the confined backend first, then falls back only for an eligible typed availability failure of a write-capable request. |
| Run every Agent natively and keep the permission mode | `backend: "native"` | Zuno skips confined-backend discovery and runs every Agent's Shell natively, read-only contracts included; the configured permission mode, rules, approvals and the risk gate stay, and the requested contract is recorded as unenforced. |
| Always use the host process backend without approval prompts | `danger-full-access` | Zuno skips confined-backend discovery, runs natively on every supported platform, and makes the effective permission mode `allow_all`. |

`backend: "native"` is the choice for a host that has no OS sandbox (macOS and
Windows today) when the permission layer should stay in force. It is a trusted
host declaration, not a fallback: nothing is probed and nothing fails first. Under
it a read-only Agent such as `plan` keeps its tool allowlist, its permission rules,
and the Shell risk gate, and that is what "read-only" then means — a role boundary,
not an OS boundary. Set it in a trusted layer, or for one invocation with
`zuno --sandbox-backend native` or `ZUNO_SANDBOX_BACKEND=native`:

```json
{
  "sandbox": {
    "backend": "native"
  }
}
```

For an explicitly unconfined invocation:

```sh
zuno run --sandbox danger-full-access "run the local build"
```

Or set it in a trusted configuration layer:

```json
{
  "sandbox": {
    "mode": "danger-full-access"
  }
}
```

For a container or host where confinement should be used when possible but native
execution is acceptable when the backend is unavailable:

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "run-unconfined"
  }
}
```

The one-invocation and environment equivalents are:

```sh
zuno run \
  --sandbox workspace-write \
  --sandbox-on-unavailable run-unconfined \
  "run the local build"

ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined zuno run "run the local build"
```

`run-unconfined` may be enabled only by a trusted global, explicit, environment,
CLI, or managed layer. Project `zuno.json[c]` and `.zuno` configuration may set
only `deny`; a checked-in repository cannot opt itself into host execution.
Managed policy has final authority and may force the value back to `deny`.
For a persistent user choice, put the JSON in the global `zuno.json` under the
config root printed by `zuno debug paths`—normally
`$XDG_CONFIG_HOME/zuno/zuno.json` or `~/.config/zuno/zuno.json`.

The same principle covers programs a checkout names. A project layer that sets `shell`, a
local `mcp.*.command`, an `lsp.*.command`, a `formatter.*.command`, or a
`productAgent.*.command` is refused outright, and switching that entry off with
`enabled: false` or `disabled: true` changes nothing, because the switch sits in the layer
the checkout controls. Only `trust.project_host_commands` in a trusted layer admits that
checkout. See [Files and precedence](/config/files).

### Network authority

`sandbox.network` is `deny` or `allow`. In confined modes the default is `deny`,
which creates a private network namespace and denies network syscalls — not a
firewall rule that a determined process can route around.

`danger-full-access` inherits host networking and **rejects an explicit `deny`**,
because it cannot enforce one. That rejection is deliberate: a configuration that
silently failed to deliver the isolation it names would be worse than one that
refuses to load.

An unavailable fallback also inherits host networking. The requested `deny` remains
recorded, but it is not the effective network boundary while the command is running
natively.

### Protected paths

`sandbox.protectedPaths` are reapplied read-only after writable roots are granted,
so a path can be carved out of a directory that is otherwise writable.

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "writableRoots": ["/srv/build-cache"],
    "protectedPaths": [".git", "secrets"]
  }
}
```

A configured entry must exist and must not be a symbolic link at the moment the
sandbox policy is built, or building the policy fails closed before any command
runs. The refusal is deliberate: a path Zuno cannot pin to a real directory would
otherwise be dropped silently, and a link beneath a writable root cannot be
protected safely. The built-in protections for `.zuno`, `.agents`, and the Git
metadata markers work differently. They are applied to whichever of those paths
exist at the moment the bubblewrap arguments are generated, so a repository without
a `.agents` directory simply gets no such mount. That same step skips, without an
error, a configured path that disappeared after the policy was built, while a
symbolic link is still refused there.

During an unavailable fallback, writable roots and protected paths remain part of the
requested policy and diagnostics, but the host process backend cannot enforce them.

## The sandbox fails closed by default

`read-only` and `workspace-write` both **require a proved OS confinement backend**.
With the default `onUnavailable: "deny"`, an unavailable backend stops Shell
assembly:

```
no trusted system bubblewrap executable was found
```

Trusted `run-unconfined` policy changes only eligible availability failures for a
write-capable Agent's `workspace-write` request. Eligible causes include an
unsupported platform, no trusted launcher being present, a missing required
launcher capability, or a namespace/container policy that makes deployment
unavailable.

The following never trigger fallback:

- a launcher that is present but untrusted;
- invalid sandbox configuration or paths;
- seccomp, helper, or internal errors;
- command preparation or execution errors; and
- every `read-only` Agent or `read-only` request.

When fallback activates, Zuno emits a host warning and records the requested mode,
network policy, effective host authority, resolution kind, and typed reason. It
preserves the configured permission mode, explicit permission denies,
catastrophic-command hard refusals, background lifecycle, timeout, cancellation,
and at-most-once execution. It cannot preserve the requested OS filesystem or
network restrictions.

Explicit `danger-full-access` is separate: it skips sandbox probing entirely, uses
the native backend from the start, and sets the effective permission mode to
`allow_all`. Explicit denies and catastrophic-command refusals still remain
terminal.

### What the Linux backend needs

The backend is bubblewrap, and it needs more than the binary being present:

- **bubblewrap 0.8.0 or newer.** Zuno requires `--disable-userns` and
  `--assert-userns-disabled`, which were introduced in 0.8.0. Ubuntu 22.04 ships
  0.6.x, which installs fine and then fails the option check.
- **Permission to create user namespaces.** A container that forbids unprivileged
  user namespaces cannot host the sandbox at all, and the probe fails with
  `No permissions to create new namespace`. Installing a newer bubblewrap does not
  help; the kernel is refusing.

Verify both with:

```sh
zuno debug sandbox
```

### Other platforms

The OS confinement backend is implemented for Linux. macOS and Windows have no
confined backend at all, so a restricted mode there fails closed rather than
degrading quietly. The refusal is written to be acted on: it names the platform,
says whether the trusted `run-unconfined` fallback applies to **this** request,
lists every remedy together with the layer that may set it, and states that none
of those remedies is confinement. It opens on the same typed cause earlier releases
printed alone:

```
OS sandbox is not implemented for platform `macos`: macos has no confined sandbox
backend, so the Shell tool cannot be registered under the requested
`workspace-write` authority. …
```

- A write-capable `workspace-write` request is eligible for the fallback, so the
  refusal offers `--sandbox-on-unavailable run-unconfined`,
  `ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`, and
  `"sandbox": {"onUnavailable": "run-unconfined"}` in a trusted global, managed,
  environment, or CLI layer — a project layer cannot enable it.
- A read-only request never falls back, and the refusal says so instead of listing
  a remedy that would not apply. Its remedy is the explicit native backend:
  `--sandbox-backend native`, `ZUNO_SANDBOX_BACKEND=native`, or
  `"sandbox": {"backend": "native"}` in a trusted layer (a project layer cannot
  select it) runs the Agent's Shell natively while keeping your permission mode.
  The requested `read-only` authority is then recorded but not OS-enforced: the
  Agent's tool contract, your permission rules, and the Shell risk gate are what
  remain, which is a role boundary and not an OS boundary.
- The write-capable refusal names the native backend beside the fallback, and
  says that `danger-full-access` additionally makes the effective permission mode
  `allow_all`.

An interactive `zuno` start on such a host asks once, before the terminal enters
raw mode, whether to run that session natively. It asks for any request the host
cannot confine, a read-only Agent's included, only when no layer set
`sandbox.onUnavailable` or `sandbox.backend`, and only when standard input and
standard error are both terminals. Answering yes resolves this process exactly as
`--sandbox-backend native` does — `resolutionKind` `trusted_native` — for every
later composition of it, a later switch to a read-only Agent included; answering
no exits with the refusal above. `run`, `acp`, `serve`, and any start without a
terminal never ask, and still need the flag, the variable, or a trusted layer.

The answer belongs to this process. On macOS the flag is exported into the real
environment by the one startup re-exec, so a nested `zuno` that a tool launches
inherits it, while an answer typed at the prompt arrives after that re-exec and is
not inherited. Set the environment variable or a trusted layer when nested Zuno
processes need the same answer. See
[Unavailable confinement](/reference/configuration#unavailable-confinement) and
[Native backend](/reference/configuration#native-backend).

Switching to an Agent whose Shell cannot be registered keeps the current Agent and
reports the same refusal on the transcript, instead of ending the session over a
switch you can undo.

`danger-full-access` always selects native execution directly. Neither it nor the
fallback is confinement: commands run with the Zuno process user's host authority,
while the configured permission mode, explicit denies, and catastrophic-command
refusals still apply.

## Permission modes

Set with `permission.mode`.

| Mode | Behaviour |
| --- | --- |
| `standard` | Apply the configured rules and the normal risk gates. The default. |
| `strict` | Require a fresh decision for every side-effecting call. |
| `allow_all` | Skip the prompts, while preserving explicit denies and sandbox validation. |

Note what `allow_all` does **not** do. It does not disable the sandbox, and it does
not override a rule that says `deny`. An explicit deny is terminal in every mode,
including this one.

## A saved "always" belongs to one session

Answering an ask with **always** saves the decision for the session you answered
in, not for the whole process. The same call in another session — including one
started later, and one another client is driving — asks again. A one-time
confirmation with no patterns to save installs nothing and is never satisfied by
an earlier `always`.

A saved `always` lives in the running process, not in the database, and it ends
when the session ends. Over HTTP, archiving or deleting a session with
`POST /api/session/prune` withdraws every authorization that session granted, and
restarting `zuno serve` clears them all; disconnecting and reconnecting an event
stream keeps them, because a stream is not the session. A decision meant to
outlive one session belongs in `permission.rules`. See
[Session retention](/session-retention#archiving-ends-a-sessions-standing-http-authorizations).

## Per-tool rules

`permission.rules` is ordered, and **the last matching rule wins**. A rule is either
one action for the whole tool, or per-pattern actions.

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "read": "allow",
      "edit": "ask",
      "shell": {
        "*": "ask",
        "git *": "allow",
        "git push*": "deny",
        "rm -rf*": "deny"
      }
    }
  }
}
```

Order matters and this example depends on it. Because later rules override earlier
ones, the catch-all `*` goes **first** and the narrow patterns that carve exceptions
out of it go **last**: `git *` overrides the catch-all, and `git push*` then overrides
`git *` so a push is denied. Reversing the order does not merely change style; it
removes the protection, because `*` written last would override every rule above it
and turn `rm -rf /` back into a prompt.

The `edit` key covers the `write`, `edit`, and `apply_patch` tools; all three request
authorization under it. There is no separate `write` or `apply_patch` rule key, and
`permission.rules` refuses one: a rule under `write`, `apply_patch`,
`list_mcp_resources`, `list_mcp_resource_templates`, or `read_mcp_resource` fails
configuration validation with a message naming the key to use instead — `edit` for the
first two, `read` for the three MCP resource tools. Those five keys used to be accepted
and evaluated nothing. Any other key is still legitimate, because MCP, plugin, and Skill
tools are named at runtime and a key may be a wildcard pattern.

A path rule is matched against the path the call names and against its normalized
spelling, so separators are unified and `.` segments and repeated separators are
dropped: `./src/main.rs`, `src//main.rs`, and the backslash spelling `src\main.rs` all
match a rule written `src/main.rs`. A `deny` deliberately reaches further. It also
covers the `..`-resolved path, and a deny written with an absolute path covers the
relative tail of that path as well, so a deny cannot be sidestepped by respelling the
file. An `allow` never widens in either of those directions, because a widened allow
would authorize a file the rule did not name.

Plan an `allow` around that asymmetry. `read`, `edit`, `write`, and `apply_patch` are
documented to take absolute paths, so `"read": {"src/main.rs": "allow"}` does not cover
the absolute path a call actually passes, while the same pattern written as a `deny`
does. Write the allow with `~`, `$HOME`, or the absolute prefix, or use `*` — which
matches across separators — as in `{"*/src/*": "allow"}`.

Print the resolved policy — the effective mode and every rule, after configuration
and any agent contract have been applied:

```sh
zuno debug permissions
```

Its output also states what a permissive mode still enforces, which is the fastest
way to confirm the guarantees above rather than take them on trust:

```json
{
  "configuredMode": "allow_all",
  "mode": "allow_all",
  "allowAllStillEnforces": [
    "explicit deny",
    "catastrophic shell denial",
    "sandbox authority",
    "argument validation"
  ]
}
```

## What an authorized path guarantees

Approving `write`, `edit`, `apply_patch`, or `read` for a path authorizes a
filesystem object, not a string. Resolution starts at the authorization boundary —
the workspace root, or the external directory you granted — and descends one segment
at a time, holding an open handle to each directory and refusing every symlink it
meets. The operation is then performed through the handle it kept, rather than by
name from the filesystem root.

The guarantee that buys is exact: the call reaches the directory object you
approved, or it fails. Replacing an ancestor directory with a symlink after you
approve cannot redirect the bytes, because the name is never resolved a second time.
Renaming the authorized directory still only reaches the object you approved.
Deleting it produces a failure, since a deleted directory accepts no new entries.

A symlink as the final component is different, and it is followed deliberately,
exactly once, before you are asked. You therefore authorize the file the link names
rather than the link itself. A link that stays inside the workspace needs only the
ordinary `edit` prompt. A link pointing outside it requires an `external_directory`
grant naming the destination's directory, not the link's. The link itself always
survives the write.

An `external_directory` grant has one spelling everywhere. The pattern is the
directory with `/*` appended, forward-slashed, with Windows' verbatim `\\?\` prefix
dropped — `C:/build-cache/*`, never `\\?\C:\build-cache\*`. One standing grant
therefore covers the shell tool and the file and search tools together, where each
previously asked under a spelling the others could not match.

Windows protection used to be absent rather than merely weaker, so upgrading changes
what the risk gate refuses there. The gate read only `HOME`, which neither `cmd` nor
PowerShell sets, so every home, profile, and credential rule switched itself off:
`rm -rf ~/.ssh` and `rm -rf $HOME` were a confirmation prompt rather than a permanent
refusal, and ran outright under `allow_all`. The home directory now falls back to the
platform's own answer, with `HOME` still taking precedence where it is set, and
`%USERPROFILE%` and `$env:USERPROFILE` expand. The verbatim `\\?\` and device `\\.\`
root spellings and UNC share roots are matched as well, drive-letter and
case-insensitively.

The hard refusals for system locations also apply to absolutely spelled
targets under PowerShell as well as Bash. Escaping is read from the shell's own
syntax, so `C:\Users\you\.ssh` is no longer reduced to `C:Usersyou.ssh` on its way
into the risk tables, and an absolute program path such as
`C:\Windows\System32\rm.exe` reaches the destructive-command table it belongs in.

## How the two interact

A few combinations are worth being explicit about, because guessing wrong about
them has real consequences.

| Configuration | What actually happens |
| --- | --- |
| `allow_all` + `read-only` | No prompts, but writes still fail. The sandbox is unaffected by permission mode. |
| `standard` + `danger-full-access` | Effective permission becomes `allow_all`; ordinary prompts are skipped, while explicit denies and catastrophic hard refusals remain. |
| `allow_all` + rule `"shell": "deny"` | Shell calls are refused. The explicit deny wins. |
| `workspace-write` + default `deny`, no backend | Shell is not assembled. Nothing runs. |
| `workspace-write` + trusted `run-unconfined`, eligible unavailable error | The command uses host authority; the configured permission mode and hard denials remain. |
| `read-only` + `run-unconfined`, no backend | Shell is not assembled. Read-only execution never falls back. |
| `read-only` + trusted `backend: "native"` | Shell runs natively with the permission mode kept. The read-only contract is a tool, permission and risk-gate boundary, not an OS boundary; the record says `trusted_native` and `requestedMode: read-only`. |

## Agent contracts narrow, never widen

A read-only agent is pinned to `read-only` regardless of what configuration asks
for. This direction is one-way by design: an agent contract can only reduce
authority, so selecting a read-only agent is a guarantee rather than a default that
configuration can quietly reverse. It also means a read-only Agent never uses
`run-unconfined`. The one way its Shell runs natively is a trusted
`sandbox.backend: native` selection, which is an explicit host declaration rather
than a fallback and leaves the contract in force as a tool and permission
boundary while removing the OS boundary.

An agent contract is deny-by-default, so a tool the contract does not name is *hidden*
rather than merely unauthorized: the contract's leading `"*": "deny"` is the last rule
that matches an unnamed id, and the model is never offered the tool at all. Two of the
default grants follow from that. `bg` is granted wherever `shell` is, including the
read-only roles, because a background execution is started by `shell` and read back only
through `bg` — and so is a result too large to return in full. `job` is granted only to
the delegating agent, because a Job resolves only for the session whose `task` call
created it.

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## See also

- [Agents](/guide/agents) — agent contracts and what each one is allowed to do
- [Configuration reference](/reference/configuration) — every `sandbox` and `permission` key
- [zuno debug](/cli/debug) — `debug sandbox` and `debug permissions`
- [FAQ](/faq) — sandbox startup failures
