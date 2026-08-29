# Plugins, custom agents, and workflows

Zuno has one extension package format and three execution tiers. The package
manifest is always `extension.json` with `apiVersion: "zuno.extension/v1"`;
what changes is where executable behavior runs.

| tier | use it for | authority and lifecycle |
| --- | --- | --- |
| Declarative package | custom agents, slash-command workflows, and skills | No guest code. Contributions enter Zuno's native catalogs and are removed with the owning composition. |
| WASI component | runtime-loadable tools that can work under bounded authority | Runs in the Zuno process through Wasmtime's Component Model. Filesystem, network, and environment are opt-in. Fuel, memory, wall time, cancellation, and reverse shutdown are enforced by the host. |
| Trusted process host | a plugin that needs a normal host executable or an API WASI cannot expose | Must declare `host.full`. It inherits the Zuno process environment and OS authority, speaks line-delimited JSON-RPC over stdio, and is stopped and reaped with the profile. |

Compiled first-party or deployment-owned Rust behavior remains a native
`Component`, typed service, or `AgentDriver` in a `ProfileBundle`. Zuno does not
load Rust dynamic libraries: Rust has no stable plugin ABI, and unloading a
library cannot prove that its threads, callbacks, or borrowed values are gone.

The built-in `/develop-zuno` Skill helps choose this extension tier and links
the current authoring references. It is guidance only: loading it does not grant
plugin capabilities, tools, filesystem access, or permission bypasses.

## Install and inspect packages

Project packages live below `.zuno/extensions`; global packages live below the
Zuno configuration root, normally `~/.config/zuno/extensions`.

```sh
# Install into the current project.
zuno plugin add examples/plugins/review-kit --project

# Inspect the packages active for this directory.
zuno plugin list

# Replace the installed directory transactionally.
zuno plugin update examples/plugins/review-kit --project

# Remove it. Running hosts keep their already-mounted composition until stopped.
zuno plugin remove review-kit --project
```

`add` refuses an existing package. `update` stages a complete copy beside the
installed directory, swaps it atomically, and restores the old directory if the
swap fails. Sources containing symbolic links or special filesystem entries are
rejected. The package directory name must equal its manifest `id`.

A model may also use `extension_define`, `extension_run`, `extension_stop`,
`extension_undefine`, and `extension_inspect` for process-local declarative
packages. That path never writes disk and deliberately rejects executable
runtime declarations.

## Custom agent and workflow

An extension agent is a normal Zuno agent. Its `permission` object, not the
plugin runtime capability list, controls which model-visible tools it can use:

- `read`, `glob`, `grep`, and read-only `lsp` provide file inspection;
- `edit` provides native file mutation;
- `webfetch` and `web_search` provide network research;
- `shell` runs a host process in the workspace and inherits Zuno's process
  environment, filesystem visibility, network, proxy variables, and credentials;
- `skill` loads reusable instructions;
- `task` delegates again, subject to the configured depth bound.

This reuses the same authorization path as native agents. There is no second
plugin-only permission language. Top-level `permission.mode: "strict"` still
requires a fresh human approval for every side-effecting call, even when an
agent rule says `allow`. A rule that says `ask` cannot run on a headless surface
without an attached approver.

The bundled review example explicitly permits repository reads and network
research and asks before shell/environment access:

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "review-kit",
  "description": "Adds a network-aware release reviewer and workflow.",
  "agents": {
    "release-reviewer": {
      "description": "Reviews release safety and rollback evidence.",
      "mode": "subagent",
      "prompt": "Use repository, environment, and current external evidence. Do not delegate.",
      "permission": {
        "mode": "standard",
        "rules": {
          "*": "deny",
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "lsp": "allow",
          "webfetch": "allow",
          "web_search": "allow",
          "shell": "ask"
        }
      }
    }
  },
  "workflows": {
    "release-review": {
      "description": "Run the packaged reviewer.",
      "prompt": "Call task once with agent=\"release-reviewer\", objective=\"Review release safety\", deliverable=\"A source-backed release risk report\", instructions=\"Review $ARGUMENTS\", success_evidence=\"Cite every blocking finding\", and background=false."
    }
  }
}
```

Configured and extension subagents are included in the exact `task` target
roster when their mode is `subagent` or `all`. Their configured model and variant
participate in the normal child-model precedence ladder, and the child turn
re-resolves the same extension package, prompt, permissions, skills, tools, and
working directory. A `primary`-only agent is not a delegation target.

The workflow prompt above is expected to produce the same typed task contract a
native Agent uses:

```json
{
  "agent": "release-reviewer",
  "objective": "Review release safety",
  "deliverable": "A source-backed release risk report",
  "instructions": "Review the requested release scope.",
  "success_evidence": "Cite every blocking finding.",
  "background": false
}
```

See [agent orchestration and model routing](orchestration.md) for the exact
direct-Agent and host-owned category precedence ladders, reasoning policy,
background report delivery, configured workflow DAGs, and Council.

A workflow is a slash-command prompt template. `$ARGUMENTS` and positional
placeholders use the normal command expansion. When a workflow must run a
specific custom agent, its prompt should issue `task` with that
`agent` and a complete typed delegation contract, as the example does; it does
not create a hidden second
orchestration path.

See [`examples/plugins/review-kit/extension.json`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/review-kit/extension.json).

## WASI component tools

A WASI runtime declares a component artifact relative to the package directory:

```json
{
  "runtime": {
    "kind": "wasi",
    "artifact": "plugin.wasm",
    "capabilities": ["workspace.read", "network"],
    "environment": ["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"],
    "fuel": 10000000,
    "memoryMiB": 64,
    "timeoutMs": 30000
  }
}
```

Capabilities are grants, not descriptive labels:

| declaration | host grant |
| --- | --- |
| `workspace.read` | read-only `/workspace` preopen and initial working directory |
| `workspace.write` | read/write `/workspace` preopen; it subsumes read access |
| `network` | DNS plus TCP/UDP through WASI sockets |
| `environment` names | only the named host variables are copied into the guest |

With no grants, the component has no workspace preopen, inherited environment,
stdio, or network. Network and proxy variables are separate: a component that
must use a proxy needs the `network` capability and must explicitly whitelist
the relevant proxy variables. Secrets should be passed only when the guest
actually needs them.

The canonical interface is
[`wit/zuno-plugin/plugin.wit`](https://github.com/sunerpy/zuno/blob/main/wit/zuno-plugin/plugin.wit):

```wit
initialize: func(package-id: string, workspace: string, capabilities: list<string>) -> result<string, string>;
invoke: func(tool: string, arguments-json: string, session-id: string, message-id: string, call-id: string, agent: string) -> result<tuple<string, string, string>, string>;
shutdown: func() -> result<_, string>;
```

`initialize` returns the exact protocol version `zuno.plugin/1`. `invoke`
returns a title, textual model-visible output, and a JSON-object metadata string.
The host serializes calls to one component instance, replenishes fuel for each
call, bounds linear memory and instance resources, applies wall time and user
cancellation, and marks a trapped or timed-out instance unavailable. A lost
response around possible side effects is `Uncertain` and is never replayed.

The complete Rust guest example is under
[`examples/plugins/wasi-word-count`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/wasi-word-count).
Build and exercise it with:

```sh
sh scripts/check-plugin-examples.sh
```

## Trusted process tools

A process plugin is the escape hatch for full host APIs:

```json
{
  "runtime": {
    "kind": "process",
    "command": "python3",
    "args": ["plugin.py"],
    "capabilities": ["host.full"],
    "timeoutMs": 30000
  }
}
```

The declaration must be exactly `["host.full"]`; an ordinary OS process cannot
truthfully enforce a narrower in-process grant. Installation is therefore a
trust decision. The child runs with the package as its working directory and
inherits normal environment variables, including `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY`, and `NO_PROXY`.

“Hosted” describes lifecycle ownership, not a security sandbox. A malicious or
compromised process plugin can read inherited credentials, access anything the
Zuno process can access, open the network, detach descendants, or mutate state
outside the workspace. Zuno withdraws routing and performs bounded best-effort
cleanup of the process tree it owns, but cannot undo external side effects or
prove that hostile code did not escape that tree. Run untrusted extensions as
WASI components or place the whole Zuno process inside an OS/container sandbox.

Because `host.full` cannot enforce a read-only boundary, every process-backed
tool must remain `sideEffecting` with `replay: never`. Strict authorization
therefore asks before each process-plugin tool call. Likewise, a WASI tool with
`network` or `workspace.write` cannot claim `readOnly`; only a component whose
grants themselves exclude mutation may opt into read-only/safe replay.

The protocol is JSON-RPC 2.0, one JSON object per line:

- `initialize` receives `protocolVersion`, `packageId`, `packageRoot`,
  `workspace`, and declared capabilities, and returns
  `{"protocolVersion":"zuno.plugin/1"}`;
- `tools/call` receives the tool name, JSON arguments, session/message/call
  coordinates, and active agent, and returns `title`, `output`, and object
  `metadata`;
- `shutdown` requests graceful cleanup before Zuno terminates and reaps the
  process tree.

Frames and captured stderr are bounded, diagnostics are scrubbed against known
secret environment values, timeouts and cancellation stop the process tree, and
protocol loss after a request is sent is reported as `Uncertain`.

See [`examples/plugins/process-review`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/process-review) for
the minimal executable example. The complete implementation guide is
[trusted process plugin development](process-plugin-development.md), including
protocol frames, security review, cancellation and uncertain outcomes, testing,
and the operator-local OpenCode Antigravity search bridge.

That bridge is deliberately documented as an external tool adapter. It reuses
credentials owned and refreshed by an installed OpenCode package after explicit
operator authorization; it does not copy OAuth identity or complete Zuno's
native Antigravity login, credential, or Integration lifecycle.

## Tool declarations and HITL

Every runtime tool declares four independent policies:

```json
{
  "tools": {
    "review_outline": {
      "description": "Create a review outline.",
      "parameters": {
        "type": "object",
        "properties": {
          "subject": { "type": "string" }
        },
        "required": ["subject"],
        "additionalProperties": false
      },
      "effect": "readOnly",
      "replay": "safe",
      "concurrency": "exclusive",
      "uiIntent": "generic"
    }
  }
}
```

The default effect is `sideEffecting`, replay is `never`, concurrency is
`exclusive`, and UI intent is `generic`. Version 1 accepts `safe` replay only for
`readOnly` tools whose WASI capability envelope also excludes network and
workspace writes, and keeps runtime plugin calls exclusive. Process tools are
always side-effecting/non-replayable. Runtime plugins cannot claim
`userMediated` or `delegating`, because those effects require native Zuno control
of the interaction or child call. A runtime with no tool is rejected because it
has no consumer and would execute code merely by starting Zuno. Strict
authorization consumes the validated effect before the plugin is invoked.

## Lifecycle guarantees

Executable hosts are deferred profile effects. All candidate packages initialize
before their routing table is published. Startup failure stops already-started
hosts in reverse order. Unload first withdraws routing, then calls `shutdown` in
reverse order and waits for quiescence. A failed, timed-out, or lost cleanup marks
the profile `Uncertain`; Zuno does not report it stopped or start an overlapping
replacement.

This guarantee covers framework-owned registrations, tasks, process trees,
component instances, and routing. It cannot undo an external mutation the plugin
already completed. Such a mutation remains a durable fact and needs an explicit
compensating operation.
