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

There are two different ways to run without OS confinement. Choose the one whose
meaning matches the deployment:

| Intent | Setting | What happens |
| --- | --- | --- |
| Require confinement | `workspace-write` plus `onUnavailable: "deny"` | The default. An unavailable backend stops Shell assembly. |
| Prefer confinement, but permit an unavailable-only fallback | `workspace-write` plus `onUnavailable: "run-unconfined"` | Zuno probes and verifies the confined backend first, then falls back only for an eligible typed availability failure. |
| Always use the host process backend | `danger-full-access` | Zuno skips confined-backend discovery and runs natively on every supported platform. |

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

The OS confinement backend is implemented for Linux. On macOS and Windows a
restricted mode reports:

```
OS sandbox is not implemented for platform `macos`
```

The default remains fail-closed. A trusted `run-unconfined` policy may let a
write-capable `workspace-write` Agent proceed natively; a read-only Agent still
refuses. `danger-full-access` always selects native execution directly.

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
authorization under it. There is no separate `write` or `apply_patch` rule key, so a
rule written under either name never matches anything.

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

## Agent contracts narrow, never widen

A read-only agent is pinned to `read-only` regardless of what configuration asks
for. This direction is one-way by design: an agent contract can only reduce
authority, so selecting a read-only agent is a guarantee rather than a default that
configuration can quietly reverse. It also means a read-only Agent never uses
`run-unconfined`.

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## See also

- [Agents](/guide/agents) — agent contracts and what each one is allowed to do
- [Configuration reference](/reference/configuration) — every `sandbox` and `permission` key
- [zuno debug](/cli/debug) — `debug sandbox` and `debug permissions`
- [FAQ](/faq) — sandbox startup failures
