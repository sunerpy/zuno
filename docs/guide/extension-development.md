# Developing agents and extensions

This guide explains which Zuno extension surface to use and how the current
implementation reaches the running Agent. It covers declarative Agents, WASI
Component Model tools, trusted process tools, and compiled native Rust
components.

The central distinction is:

- an **Agent definition** selects a prompt, model route, tools, permissions,
  Skills, and delegation targets;
- an **executable plugin** implements one or more tools behind a validated
  package boundary;
- an **`AgentDriver`** owns the turn-driving algorithm itself.

A declarative or WASI package can add an Agent and tools, but it cannot replace
the provider loop, credential authority, approval service, durable inbox, or
sub-turn scheduler. Those are trusted native Rust responsibilities.

This page describes `zuno.extension/v1`, `zuno.plugin/1`, and the native
`Component`/`HarnessProfile` interfaces implemented on the current `main`
branch. Zuno is a 0.x project, so use the interface definitions and examples
from the same revision as the binary you are building.

## Choose the smallest correct surface

| Requirement | Use | Runs code? | Can replace the Agent loop? |
| --- | --- | --- | --- |
| Change a prompt, model, tool allowlist, permissions, Skills, or delegation targets | Config or Markdown Agent | No | No |
| Ship an Agent, slash workflow, and Skill as one installable unit | Declarative `extension.json` package | No | No |
| Add a portable tool with explicit filesystem, environment, and network grants | WASI component package | Yes, in Wasmtime | No |
| Add a tool that needs a normal host executable or an installed SDK | Trusted process package | Yes, as a child process | No |
| Add a provider, login flow, credential store, approval service, typed service, scheduler, or complete turn policy | Native Rust `Component` or `AgentDriver` | Yes, compiled into the binary | Yes, through `AgentDriver` |
| Invoke an installed Codex or Claude Code product as a bounded subagent | `productAgent` configuration | Yes, through its native protocol adapter | No |

Start with the first row that can express the behavior. Moving down the table
increases authority, lifecycle work, testing cost, and the damage an incorrect
implementation can cause.

## How the surfaces compose

One session resolves the extension and Agent surfaces in this order:

```text
static packages + process-local declarative packages
    -> collision-checked extension catalog
    -> Agent / workflow / Skill contributions
    -> executable plugin tool proxies
    -> HarnessProfile activation
    -> native tool registry
    -> AgentProfile capability narrowing
    -> provider-visible prompt and tool schemas
```

The executable and declarative halves share package ownership, but they have
different consumers:

1. `zuno-extension` discovers and validates `extension.json`.
2. `resolve_active` merges static and process-local packages. Duplicate package
   ids or duplicate Agent, workflow, Skill, or tool names fail; Zuno does not
   silently choose a winner.
3. Agent contributions are merged with configured and Markdown Agents, then
   frozen into an `AgentProfile`.
4. Workflow contributions enter the normal slash-command registry.
5. Skill contributions enter the normal Skill catalog with package provenance.
6. Runtime tool declarations become native `Tool` proxies.
7. The runtime host is mounted as a deferred profile effect. Its tools are not
   published until initialization succeeds.
8. The final tool registry applies source precedence, Agent narrowing,
   permissions, request hooks, and provider capability filtering.

This is why an extension Agent uses the same `task`, permission, sandbox, model,
and child-session paths as a built-in Agent. The package does not create a
second Agent runtime.

## Declarative Agents are data, not drivers

An Agent contributed by `extension.json` accepts the same fields as
`agents.<name>`:

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "release-review",
  "description": "Adds a bounded release reviewer.",
  "agents": {
    "release-reviewer": {
      "description": "Reviews release safety without editing.",
      "mode": "subagent",
      "model": "myopenai/reasoner",
      "tools": ["read", "glob", "grep", "lsp", "web_search"],
      "requiredSkills": ["release-safety"],
      "prompt": "Review immutable inputs, rollback evidence, and required gates. Do not delegate.",
      "permission": {
        "mode": "standard",
        "rules": {
          "*": "deny",
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "lsp": "allow",
          "web_search": "allow"
        }
      }
    }
  },
  "skills": [
    {
      "name": "release-safety",
      "description": "Use for release and deployment reviews.",
      "content": "Check exact commits, required jobs, immutable artifacts, rollback, and production evidence."
    }
  ]
}
```

Important boundaries:

- the map key is the Agent identity; an extension Agent cannot rename itself;
- an extension cannot disable a built-in Agent;
- `mode: "subagent"` or `"all"` is required to join the `task` roster;
- `tools` is an exact allowlist, not an addition;
- `requiredSkills` guarantees instructions only and grants no tool authority;
- permissions can narrow the effective surface but cannot restore a tool absent
  from the parent Attempt;
- a workflow that wants this Agent must call the normal `task` tool with an
  explicit typed delegation contract.

Use [Custom agents](/config/custom-agents) for every field and
[Orchestration and delegation](/orchestration) for child-turn semantics. The
complete declarative example is
[`examples/plugins/review-kit`](https://github.com/sunerpy/zuno/tree/main/examples/plugins/review-kit).

## Implement a WASI tool

Use WASI when the behavior is naturally a tool and its authority can be
expressed as explicit grants. Zuno hosts a WebAssembly **component**, not a
legacy core module and not a Rust dynamic library.

### Package layout

```text
word-stats/
├── extension.json
├── plugin.wasm
└── guest/
    ├── Cargo.toml
    └── src/lib.rs
```

The installed directory name must equal the package `id`. The component artifact
must be a relative path that stays below the package directory.

### Guest crate

A Rust guest is a `cdylib` built for `wasm32-wasip2`:

```toml
[package]
name = "word-stats"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
serde_json = "1"
wit-bindgen = "0.60"

[workspace]
```

Generate bindings from the canonical WIT world:

```rust
wit_bindgen::generate!({
    path: "../../../../wit/zuno-plugin",
    world: "plugin",
});

struct WordStats;

impl Guest for WordStats {
    fn initialize(
        _package_id: String,
        _workspace: String,
        _capabilities: Vec<String>,
    ) -> Result<String, String> {
        Ok("zuno.plugin/1".to_owned())
    }

    fn invoke(
        tool: String,
        arguments_json: String,
        _session_id: String,
        _message_id: String,
        _call_id: String,
        _agent: String,
    ) -> Result<(String, String, String), String> {
        if tool != "word_stats" {
            return Err(format!("unknown tool `{tool}`"));
        }
        let arguments: serde_json::Value =
            serde_json::from_str(&arguments_json).map_err(|error| error.to_string())?;
        let text = arguments
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "`text` must be a string".to_owned())?;
        let words = text.split_whitespace().count();
        Ok((
            "Word statistics".to_owned(),
            format!("{words} words"),
            serde_json::json!({ "words": words }).to_string(),
        ))
    }

    fn shutdown() -> Result<(), String> {
        Ok(())
    }
}

export!(WordStats);
```

The ABI deliberately keeps JSON Schema in the manifest and uses JSON strings
only at the guest boundary:

```text
initialize(package-id, workspace, capabilities) -> result<protocol-version, error>
invoke(tool, arguments-json, session-id, message-id, call-id, agent)
    -> result<(title, output, metadata-json), error>
shutdown() -> result<_, error>
```

`initialize` must return exactly `zuno.plugin/1`. `metadata-json` must decode to
an object or `null`. `output` is model-visible, so do not place credentials,
authorization headers, private upstream bodies, or unbounded binary data in it.
Do not return `cancellation` in `metadata-json`: it is reserved for Zuno's own
cancellation claim and is dropped with a warning.

`workspace` is the native workspace path byte for byte, with no separator
normalization on any platform. A workspace whose path is not valid UTF-8 has no
representation on this boundary, so Zuno refuses to start the component by name
rather than passing a substituted path. Inherited environment values follow the
same rule: a variable whose value is not valid UTF-8 is omitted from the guest
environment with a warning naming the variable, never substituted.

### Manifest and authority

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "word-stats",
  "description": "Computes statistics without host access.",
  "runtime": {
    "kind": "wasi",
    "artifact": "plugin.wasm",
    "capabilities": [],
    "environment": [],
    "fuel": 10000000,
    "memoryMiB": 64,
    "timeoutMs": 30000
  },
  "tools": {
    "word_stats": {
      "description": "Count words in supplied text.",
      "parameters": {
        "type": "object",
        "properties": {
          "text": { "type": "string", "maxLength": 100000 }
        },
        "required": ["text"],
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

The grant envelope is enforced independently from the tool description:

| Grant | Guest authority |
| --- | --- |
| none | No workspace preopen, inherited environment, stdio, or network |
| `workspace.read` | Read-only `/workspace` preopen and working directory |
| `workspace.write` | Read/write `/workspace`; includes read access |
| `network` | WASI DNS and TCP/UDP sockets |
| `environment: ["NAME"]` | Copy exactly the named host variables |

Network and proxy configuration are separate. A proxy-using guest normally
needs `network` plus explicit `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY`
environment names. Do not grant an entire credential-bearing environment when
one variable is enough.

The manifest validator enforces:

- `timeoutMs` from 1 through 300,000;
- WASI `fuel` from 10,000 through 1,000,000,000;
- `memoryMiB` from 8 through 1,024;
- unique capabilities and environment names;
- at least one tool for every runtime;
- one runtime for every executable tool;
- exclusive protocol-v1 calls;
- `replay: safe` only with `effect: readOnly`;
- read-only claims only when the grant envelope excludes network,
  `workspace.write`, and `host.full`;
- no `userMediated` or `delegating` runtime tools.

The last rule is architectural, not cosmetic: human interaction and child-turn
creation must remain native, durable Zuno operations.

### Host lifecycle

The WASI runtime is connected to the native component runtime as follows:

```text
extension manifest
    -> RuntimeSurface tool proxies
    -> PluginRuntimeComponent in a ProfileBundle
    -> Component::prepare registers a deferred effect
    -> effect start builds the Wasmtime instance
    -> initialize negotiates zuno.plugin/1
    -> profile publishes tools atomically
    -> invoke executes serialized calls
    -> profile withdrawal removes routing
    -> shutdown runs during reverse-order cleanup
```

For every exported call, Zuno replenishes fuel, sets an epoch deadline, applies
the wall-clock timeout, and honors user interruption. Calls for one component
instance are serialized. A trap, join failure, malformed metadata response, or
unsettled interruption poisons the instance and removes it from future routing.

An invocation timeout is an uncertain outcome because guest side effects may
already have started. The host withdraws the instance and never mechanically
replays the call. A tool may declare safe replay only when both its semantic
operation and enforceable grant envelope are read-only. Native tools default to
`ToolReplayPolicy::Never` for the same at-most-once reason.

### Build and test

```sh
CARGO_TARGET_DIR=target/plugin-examples/word-stats \
  cargo build \
  --manifest-path path/to/word-stats/guest/Cargo.toml \
  --target wasm32-wasip2 \
  --release

cp \
  target/plugin-examples/word-stats/wasm32-wasip2/release/word_stats.wasm \
  path/to/word-stats/plugin.wasm

zuno plugin add path/to/word-stats --project
zuno plugin list
```

In the Zuno repository, the canonical acceptance path is:

```sh
sh scripts/check-plugin-examples.sh
cargo test -p zuno-extension --test manifest
cargo test -p zuno-extension --test runtime_hosts
```

The ignored WASI fixture in `runtime_hosts` is executed by
`check-plugin-examples.sh` after the guest component has been built.

## Implement a native Rust component

Use native Rust when the behavior needs a trusted typed interface, owns durable
state or credentials, participates in provider or approval lifecycles, or must
replace the turn driver. Native behavior is compiled into Zuno; there is no
runtime-loaded Rust ABI.

### Publish a typed service and an exact disposer

`Component::prepare` must be side-effect free. It may stage services and declare
effects, but it must not bind, spawn, subscribe, or mutate the outside world
directly.

```rust
use std::sync::Arc;

use async_trait::async_trait;
use zuno_runtime::{Component, EffectError, PrepareContext, RuntimeError};

trait ReviewIndex: Send + Sync {
    fn revision(&self) -> u64;
}

struct ReviewIndexService {
    revision: u64,
}

impl ReviewIndex for ReviewIndexService {
    fn revision(&self) -> u64 {
        self.revision
    }
}

struct ReviewIndexComponent {
    service: Arc<dyn ReviewIndex>,
}

#[async_trait]
impl Component for ReviewIndexComponent {
    fn id(&self) -> &str {
        "review-index"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn ReviewIndex>(Arc::clone(&self.service))?;
        context.effect("watcher", || async {
            let watcher = start_watcher()
                .await
                .map_err(|error| EffectError::new(error.to_string()))?;
            Ok::<_, EffectError>(move || async move {
                watcher
                    .stop()
                    .await
                    .map_err(|error| EffectError::new(error.to_string()))
            })
        })
    }
}
```

The function names behind `start_watcher` are application-specific; the
important contract is the shape:

1. `prepare` stages the typed service.
2. `effect` records a start closure.
3. The start closure acquires the resource only after every candidate component
   has prepared.
4. A successful start returns the exact asynchronous disposer for that resource.
5. A failed start must leave no live resource.
6. The disposer must not report success until the resource is quiescent.

A consumer resolves and records its dependency during preparation:

```rust
let index = context.require::<dyn ReviewIndex>()?;
```

When a provider is replaced, dependent components are re-prepared against the
candidate service graph before anything becomes visible.

### Build a profile

Components that share deployment and replacement ownership belong in a
`ProfileBundle`. A `HarnessProfile` is the complete composition for one runtime
scope:

```rust
use zuno_runtime::{HarnessProfile, ProfileBundle};

let profile = HarnessProfile::new("review")
    .with_bundle(
        ProfileBundle::new("review.services")
            .with_component(ReviewIndexComponent { service }),
    )
    .with_bundle(zuno_harness::tool_contributions_bundle(
        "review.tools",
        "review.tool-contributions",
        contributions,
    ));

runtime.activate_profile(profile).await?;
```

Use stable, unique profile, bundle, component, effect, tool, and capability
identifiers. Component identity controls replacement and diagnostics; it is not
just a display label.

### Contribute native tools

A native tool implements `zuno_tool::Tool` or `TypedTool`, then enters a
`ToolContributions` snapshot:

```rust
let contributions = ToolContributions::new([
    zuno_tool::erase(ReviewStatusTool::new(service)),
])?;

let profile = zuno_harness::profile_with_tools(
    "review",
    Arc::new(DefaultAgentDriver),
    ToolManifest::standard(),
    contributions,
);
```

The profile publishes both the executable typed service and a named capability
descriptor containing the stable interface id, schema digest, owner,
provenance, generation, and availability. Dynamic consumers use the descriptor;
native Rust consumers use the typed service.

Tool source precedence is built-in, then harness contribution, then MCP. A
same-named later source wins and emits a structured suppression diagnostic.
Avoid collisions unless replacement is deliberate and tested.

### Replace the Agent driver

An `AgentDriver` owns one complete turn-driving policy:

```rust
use futures::future::BoxFuture;
use zuno_engine::driver::AgentDriver;
use zuno_engine::r#loop::{
    RunTurnRequest, TurnContext, TurnError, TurnEventSender, TurnOutcome,
};

struct EvaluationDriver;

impl AgentDriver for EvaluationDriver {
    fn name(&self) -> &str {
        "evaluation"
    }

    fn drive<'a>(
        &'a self,
        request: RunTurnRequest,
        context: TurnContext<'a>,
        events: TurnEventSender,
    ) -> BoxFuture<'a, Result<TurnOutcome, TurnError>> {
        Box::pin(async move {
            // Drive the complete typed turn contract here.
            run_evaluation_turn(request, context, events).await
        })
    }
}
```

Install it with `AgentDriverComponent` or `zuno_harness::profile`:

```rust
let profile = zuno_harness::profile(
    "evaluation",
    Arc::new(EvaluationDriver),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep])?,
);
```

Replacing the driver is appropriate for benchmark, evaluation, workflow, or
remote harnesses that own a different complete turn policy. It is not required
for a new prompt-defined Agent, specialist role, or tool.

A custom driver still receives the native `RunTurnRequest`, `TurnContext`,
`TurnEventSender`, and durable stores. It must preserve the same logging,
interruption, tool replay, human wait, and terminal outcome contracts expected
by every client. Changing the default driver or loop requires updating
[Harness Runtime](/harness-runtime) in the same change.

## Transactional activation and recovery

`HarnessRuntime::activate_profile`, `mount`, `replace`, and `unmount` all use the
same transition algorithm:

1. Reject the transition if a child scope still has a live consumer.
2. Prepare the complete candidate against a staging view. Candidate services
   remain invisible.
3. If preparation fails, drop all unstarted effects and restore the stable
   projection.
4. Withdraw current capabilities and services before stopping old effects.
5. Stop the old composition in reverse order with a bounded timeout.
6. If old cleanup is not proven, enter `Uncertain` and refuse overlap.
7. Start candidate effects in order.
8. Publish all candidate services and capabilities atomically only after every
   start succeeds.
9. If candidate startup fails cleanly, freshly prepare and start the previous
   definition.
10. If candidate cleanup or restoration cannot prove quiescence, enter
    `Failed` or `Uncertain` with typed lifecycle diagnostics.

Never hide a cleanup error or convert an uncertain side effect into success.
`RuntimeSnapshot` is the client-facing inventory for lifecycle state,
components, effects, capabilities, and scrubbed diagnostics.

## Trusted process tools

Use a process plugin only when WASI cannot expose a required host API. It must
declare exactly `["host.full"]`, inherits Zuno's process environment and OS
authority, and is always side-effecting with `replay: never`.

The process protocol, containment limits, cancellation, uncertain outcomes,
security review, and complete JavaScript example are documented separately in
[Trusted process plugin development](/process-plugin-development).

## Cross-platform rules

Zuno ships native binaries for multiple operating systems and architectures.
An extension must state where its portability boundary lies:

- a declarative package is portable when its referenced Skills and workflows
  are portable;
- a WASI component is the preferred portable executable boundary, but every
  granted WASI API must be tested on the supported hosts;
- a process package is portable only when its command, arguments, runtime,
  filesystem assumptions, and process-tree cleanup work on each target;
- a native Rust component must compile on every supported target and requires
  native execution evidence for OS-specific lifecycle behavior.

Do not treat Linux process semantics, path separators, executable suffixes,
signals, permissions, or shell syntax as universal. Cross-compilation is useful
evidence, but it does not replace native execution for Windows process trees,
macOS behavior, terminal integration, or OS-owned credential and filesystem
boundaries.

## Definition of done

For a declarative Agent:

- validate the package or configuration;
- inspect `zuno debug agent <name>`;
- verify the exact `task` roster and permission ceiling;
- test the workflow or direct selection through a real client surface.

For a WASI or process tool:

- validate closed JSON Schema and semantic input bounds;
- document every grant, environment variable, credential source, and side
  effect;
- test initialization, invocation, shutdown, cancellation, timeout, malformed
  output, and uncertain cleanup;
- verify that tool effect, replay, concurrency, and UI intent agree with the
  implementation;
- install the package and invoke the archived artifact, not only the build-tree
  binary.

For native Rust:

- keep interface, provider, and consumer ownership explicit;
- make `prepare` side-effect free;
- return one exact disposer for every effect;
- test replacement, reverse cleanup, failed startup restoration, timeout, and
  uncertain state;
- update the owning architecture document and client projection;
- run the smallest crate tests, then the shared workspace gates.

## Source map

Use these implementation points when this guide and code appear to disagree:

| Concern | Current source |
| --- | --- |
| Package schema and validation | `crates/zuno-extension/src/manifest.rs` |
| Static discovery and contribution merge | `crates/zuno-extension/src/static_loading.rs`, `resolve.rs` |
| Extension revision and lease transactions | `crates/zuno-extension/src/registry.rs` |
| Runtime tool proxies and lifecycle bundle | `crates/zuno-extension/src/host.rs` |
| WASI host | `crates/zuno-extension/src/host/wasi.rs` |
| Process host | `crates/zuno-extension/src/host/process.rs` |
| Canonical WASI world | `wit/zuno-plugin/plugin.wit` |
| Native component runtime | `crates/zuno-runtime/src/lib.rs` |
| Profile helpers and tool contributions | `crates/zuno-harness/src/lib.rs` |
| Replaceable Agent driver | `crates/zuno-engine/src/driver.rs` |
| Agent catalog and merge | `crates/zuno-catalog/src/agent.rs` |
| Effective Agent capability snapshot | `crates/zuno-agent/src/profile.rs` |
| CLI composition root | `crates/zuno-cli/src/cmd/turn.rs` |
| Executable examples | `examples/plugins/` |

See [Documentation architecture and coverage](/design/documentation-coverage)
for the canonical page that owns every other public Zuno surface.
