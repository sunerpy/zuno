# Native Component Lifecycle Kernel

Status: implementation plan, 2026-08-22.

## Decision

Zuno will adapt the lifecycle guarantees behind Cordis as a native Rust
component kernel. It will not embed Cordis, load its JavaScript ABI, or copy its
package layout.

Zuno is unreleased. The existing `Component::mount` and `MountContext::on_close`
API will be replaced directly. There will be no compatibility facade and every
internal caller will move in the same change.

The kernel owns one enforceable postcondition:

> After an unload reports `Stopped`, the component cannot receive a new call, no
> component-owned task or process remains live, and every framework-owned
> registration, listener, connection, route, and service has been removed.

A timeout, lost process response, or failed disposer never reports `Stopped`.
It reports `Uncertain`, retains diagnostics, and is never mechanically replayed.
The kernel does not claim that unloading can reverse an already-completed
external mutation such as a remote API write. Such operations remain durable
facts and require an explicit compensating action.

## Problems being removed

1. The current cleanup closure cannot fail, time out, or describe an uncertain
   result.
2. Replacement starts a candidate while the previous component still owns
   exclusive resources.
3. Runtime state and services are removed before cleanup proves quiescence.
4. A service dependency is discovered only by an immediate `require<T>()`
   lookup. Provider replacement does not reactivate consumers.
5. `TurnHost` assembles and owns most product services outside components.
6. TUI exit and host replacement drop or abort owners instead of awaiting their
   authoritative shutdown path.
7. Declarative extension state commits before the active host composition has
   successfully changed.
8. Clients cannot inspect component lifecycle state or cleanup failures through
   one frontend-neutral projection.

## Lifecycle model

### Component preparation

`Component::prepare` is a side-effect-free planning phase. It may:

- provide typed services;
- require typed services staged by an earlier component or inherited from a
  parent scope;
- register deferred effects;
- validate configuration.

It must not spawn, bind, subscribe, write, or publish directly. A component that
needs one of those operations registers a deferred effect with `EffectScope`.

Preparation failure drops unstarted effect factories and exposes no candidate
service.

### Effects

An effect has a stable component-local identifier and two phases:

1. `start` acquires the resource and returns its exact asynchronous disposer;
2. `stop` requests shutdown and waits until the resource is quiescent.

`start` must either return an owned disposer or leave no live resource. If an
operation creates a resource and later fails, that operation cleans the resource
before returning its error.

Effects start in registration order and stop in reverse order. A start failure
stops every already-started candidate effect before returning.

### Composition transition

Every mutation builds a complete candidate composition:

1. validate component and bundle identifiers;
2. prepare all candidate components against a staging service view;
3. mark the runtime `Stopping` and make old local services unavailable;
4. stop the previous composition completely;
5. if any old effect is not known stopped, mark the runtime `Uncertain` and do
   not start the candidate;
6. start candidate effects in component order;
7. publish all candidate services atomically and mark every component `Active`;
8. if candidate start fails, stop the partial candidate and restore the previous
   definition through a fresh prepare/start cycle;
9. if restoration fails, expose no local services and mark the runtime
   `Failed` or `Uncertain`.

This deliberately prioritizes exclusive-resource safety over zero-downtime
replacement. A future provider-specific handoff protocol may opt into overlap,
but overlap is not the default lifecycle contract.

Adding, replacing, removing, or changing a profile recomposes consumers that
resolved services from the changed scope. The first implementation may
recompose the complete local scope; the runtime still records exact
`requires/provides` ownership so a later optimization can calculate a minimal
dependency closure without changing behavior.

### State and diagnostics

Runtime and component projections use these states:

- `Preparing`
- `Active`
- `Stopping`
- `Stopped`
- `Failed`
- `Uncertain`
- `Closed` for the runtime scope

Lifecycle diagnostics record:

- runtime and component identifiers;
- effect identifier;
- phase (`prepare`, `start`, `stop`, `restore`);
- typed failure kind (`rejected`, `failed`, `timed_out`, `uncertain`);
- a scrubbed message.

Cleanup has a validated positive timeout. Timeout cancellation drops the wait
future but never implies the external resource stopped.

## Ownership rules

The following registrations must be acquired through an `EffectScope` adapter:

| Resource | Stop contract |
| --- | --- |
| Typed service | Remove before stopping dependent activity |
| Tool/provider/hook/route registration | Remove exact registration handle |
| Tokio task | Cancel and await `JoinHandle` |
| Process tree | Signal complete tree, wait, then force and reap if needed |
| Watcher/subscription | Unregister listener before producer shutdown |
| MCP connection | Close protocol session, then stop transport |
| LSP manager | Send shutdown, kill and reap all children |
| Background job supervisor | Cancel or settle according to job policy, then join |

`Drop` remains a last-resort safety net and is not accepted as proof of graceful
unload.

First-party component crates may not call process spawning, `tokio::spawn`, or
global registration APIs outside an ownership adapter unless the code documents
an explicit process-lifetime exemption. Third-party executable plugins will not
run as Rust dynamic libraries in the main process. They must use an isolated
process protocol or a capability-restricted WASI host.

## Product migration

### Runtime and profiles

- Replace `MountContext` with `PrepareContext` and `EffectScope`.
- Store component definitions so a failed transition can restore the previous
  composition.
- Publish runtime inventory and lifecycle diagnostics.
- Preserve child-scope inheritance, shadowing, reverse teardown, and atomic
  service publication.
- Preserve `ProfileBundle` and `HarnessProfile` as the composition format.

### Turn host

Move product capabilities behind typed component services. The migration order
is:

1. agent driver and tool manifest/contributions;
2. background job and execution ownership;
3. MCP and LSP lifecycle;
4. provider registry and authentication-bound client construction;
5. hooks, memory/reflection, commands, and client projections;
6. server and ACP route registrations.

`TurnHost` remains a composition root and durable turn facade, but it does not
privately create resources whose lifetime differs from its runtime.

### TUI

- `drive_turns` owns the `TurnHost` and always executes `shutdown` before
  returning.
- TUI exit closes producer channels or sends explicit stop signals, then awaits
  turn, MCP, LSP, editor, cancellation, and history workers under bounded
  deadlines.
- Host replacement prepares the candidate, shuts down the current host, and
  installs the candidate only after shutdown succeeds.
- A failed or uncertain shutdown is visible in the transcript and prevents a
  second composition from claiming the same resources.
- Session remount retains the physical terminal but not the previous session's
  live capabilities.

### Extensions

Declarative agent/workflow/skill packages remain non-executable. Their lifecycle
is coupled to host composition:

1. registry creates a candidate state without marking it running;
2. the host resolves and prepares the candidate composition;
3. the old host shuts down;
4. the candidate host commits;
5. only then does the registry publish `Running` and advance the scope-local
   generation.

Failure leaves the previous registry state and composition together. Generation
is scoped by workspace rather than global to the process.

Executable extension support is deferred until the lifecycle kernel and an
isolated plugin host both pass the acceptance tests below.

## TDD and acceptance matrix

Tests are added before the behavior they require.

### `zuno-runtime`

- candidate effects do not start during prepare;
- candidate services remain invisible until every effect starts;
- effects stop in reverse registration and component order;
- stop waits for actual task/process completion;
- explicit stop failure becomes `Uncertain`;
- a hanging stop is bounded and becomes `Uncertain`;
- replacement never starts a candidate before the old exclusive effect stops;
- a failed candidate start restores the previous composition;
- failed restoration leaves no service published and records diagnostics;
- profile replacement re-prepares a consumer against the new provider;
- unmount re-prepares a consumer against the revealed provider;
- parent shutdown closes children first;
- inventory exposes active, stopping, failed, uncertain, and closed states;
- repeated shutdown is idempotent.

### Production entry paths

- a real default profile still resolves driver and tool services;
- TUI exit awaits `TurnHost::shutdown`;
- model/agent/MCP host replacement shuts down the previous host exactly once;
- session remount closes the old host before the next composition starts;
- TUI LSP shutdown reaches `Manager::shutdown`;
- TUI MCP shutdown closes remote sessions rather than only dropping transports;
- background jobs cannot outlive the host that owns their write authority.

### Extensions

- a failed host preparation does not publish `Running`;
- a failed old-host shutdown does not publish the candidate state;
- run/stop/undefine change only the matching workspace generation;
- successful commit updates catalog and lifecycle state together;
- restart still drops process-local definitions;
- static and dynamic packages use the same validated catalog contribution path.

### Full gates

Focused tests run after each red/green cycle. Delivery requires complete success
from:

```sh
cargo fmt --all --check
cargo test -p zuno-runtime
cargo test -p zuno-harness
cargo test -p zuno-extension
cargo test -p zuno-cli
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

PTY tests that exercise TUI exit/remount are mandatory evidence; source-string
assertions alone are not acceptance.

## Delivery sequence

1. Lifecycle specification and failing runtime tests.
2. Native lifecycle kernel and profile migration.
3. TUI/TurnHost graceful ownership and PTY tests.
4. Extension activation transaction and scope-local generation.
5. Critical product resource adapters and lifecycle projection.
6. Architecture documentation, DSH adoption ledger, full gates.

Each step is a separate logical commit. Existing user-owned `.omo/notepads`
changes remain outside every commit.
