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
    sandbox ── no backend ──▶ session refuses to start
        │
        ▼
   command runs, confined
```

A permission decision is about intent: should this call happen. A sandbox is about
capability: what the process can reach once it does. Allowing a call does not widen
the sandbox, and a permissive sandbox does not skip the permission gate.

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
    "network": "deny"
  }
}
```

### Network authority

`sandbox.network` is `deny` or `allow`. In confined modes the default is `deny`,
which creates a private network namespace and denies network syscalls — not a
firewall rule that a determined process can route around.

`danger-full-access` inherits host networking and **rejects an explicit `deny`**,
because it cannot enforce one. That rejection is deliberate: a configuration that
silently failed to deliver the isolation it names would be worse than one that
refuses to load.

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

## The sandbox fails closed

This is the part worth understanding before you deploy Zuno anywhere.

`read-only` and `workspace-write` both **require a proved OS confinement backend**.
When one is unavailable, Zuno does not fall back to running your command
unconfined — it refuses to start the session:

```
no trusted system bubblewrap executable was found
```

There is no configuration option that turns this into a warning, and restricted
modes never downgrade to the unconfined backend. If you want unconfined execution
you have to ask for it by name, with `danger-full-access`, which is an explicit
trusted choice rather than a silent consequence of a missing package.

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

This is the same fail-closed behaviour, not a special case: no backend means no
confined session.

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

`permission.rules` is ordered and evaluated in the order you write it. A rule is
either one action for the whole tool, or per-pattern actions.

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "read": "allow",
      "write": "ask",
      "shell": {
        "git push*": "deny",
        "git *": "allow",
        "rm -rf*": "deny",
        "*": "ask"
      }
    }
  }
}
```

Order matters and the example depends on it: `git push*` has to precede `git *`, or
the broader pattern would match first and a push would be allowed.

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
| `standard` + `danger-full-access` | You are still asked, but an approved command has full host authority. |
| `allow_all` + rule `"shell": "deny"` | Shell calls are refused. The explicit deny wins. |
| Any restricted mode, no backend | The session does not start. Nothing runs. |

## Agent contracts narrow, never widen

A read-only agent is pinned to `read-only` regardless of what configuration asks
for. This direction is one-way by design: an agent contract can only reduce
authority, so selecting a read-only agent is a guarantee rather than a default that
configuration can quietly reverse.

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## See also

- [Agents](/guide/agents) — agent contracts and what each one is allowed to do
- [Configuration reference](/reference/configuration) — every `sandbox` and `permission` key
- [zuno debug](/cli/debug) — `debug sandbox` and `debug permissions`
- [FAQ](/faq) — sandbox startup failures
