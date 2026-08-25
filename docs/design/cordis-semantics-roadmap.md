# Cordis Semantics Adoption Roadmap

Status: Phase 0 and the named-capability registry foundation are implemented,
2026-08-25; dependency-closure reconciliation and product adoption remain.

## Decision

Zuno will not depend on `cordis-rs`, fork it into an asynchronous runtime, or
replace `zuno-runtime` with a compatibility layer.

Zuno will instead keep its Tokio-native component lifecycle as the only owner of
live resources and incrementally adopt the Cordis semantics that solve observed
product problems:

- named capabilities at dynamic boundaries;
- explicit dependency generations and dependency-closure reconciliation;
- lifecycle-owned event and policy subscriptions;
- transactional contribution replacement;
- isolated execution for runtime-loaded code.

The first implementation remains internal to the Zuno workspace. Extraction into
an independent crate is a later decision gated by real reuse and API stability,
not a prerequisite for product work.

The first two native slices are now implemented across `zuno-runtime` and the
profile composition root. Zuno keeps executable
Rust services on the typed plane and adds a descriptor-only named plane with
validated keys, contracts, provenance, scope-local generations, parent/child
shadowing, atomic publication, withdrawal before cleanup, stale-generation
detection, and lifecycle projection. `PrepareContext` can provide and require a
named descriptor, and the observed provider generation is recorded in component
requirements. Product components now publish extension Tool descriptors while
retaining their native `Tool` objects, and publish the immutable Agent Profile,
Workflow Template, and Skill catalogue beside its typed `CapabilitySnapshot`.
Same-name Skills use source-isolated keys rather than acquiring a hidden winner.

This implementation does not yet project MCP/remote-host manifests or the final
post-permission ToolRegistry, and it does not implement minimal affected
dependency closure, parking/reactivation, or the event/policy bus. Those remain
later phases and must use the same lifecycle authority.

This refines, rather than reverses, the decision in
[Native Component Lifecycle Kernel](component-lifecycle-kernel.md): Zuno adopts
Cordis lifecycle guarantees and composition ideas without loading its JavaScript
ABI, copying its package layout, or introducing a Rust dynamic-library ABI.

## Evidence baseline

### What DSH actually relies on

A production-source survey against the DSH vendored Cordis 4.0.1 found the
following lexical lower bounds in files that import Cordis:

| operation | observed uses | implication for Zuno |
| --- | ---: | --- |
| `ctx.on` | 214 | reversible event subscriptions are a first-class composition primitive |
| `ctx.get` | 190 | runtime-named lookup is a real requirement at dynamic boundaries |
| `ctx.effect` | 138 | precise ownership and disposal are the dominant lifecycle primitive |
| `ctx.inject` | 41 | dependency availability controls whether a plugin may run |
| `waterfall` | 13 | ordered policy transformation is used for critical decisions |
| `parallel` | 2 | parallel dispatch is rare but can represent a persistence barrier |
| `ctx.set` | 0 | arbitrary mutation of the context is not a product requirement |
| `ctx.isolate` | 0 | direct production calls are absent; isolation still exists in service resolution |
| `ctx.intercept` | 0 | a generic interception API is not required for initial adoption |

The relevant lesson is not to reproduce every Cordis method. DSH receives most
of the value from reversible contribution ownership, named capability
composition, dependency parking/reactivation, and a small set of event
semantics.

DSH also supports model-authored runtime plugins, but the plugin host is a
separate layer from Cordis. It gives guest code a whitelist proxy and declared
services rather than the raw framework context. Zuno must preserve that
separation.

### What `cordis-rs` already implements

The current Rust port is not a small skeleton. It already contains:

- fibers and transition state;
- named services and isolation labels;
- provider generations and consumer reconciliation;
- effects and disposal;
- serial, bail, waterfall, and parallel event paths;
- loader, group, and include crates.

Its core mismatch with Zuno is semantic rather than feature count. Lifecycle
reconciliation is eager and synchronous, and asynchronous callbacks are driven
through a blocking executor while lifecycle synchronization is held. Zuno must
await process shutdown, MCP/LSP protocol closure, task cancellation, WASI
interruption, and bounded cleanup without blocking a Tokio worker or pretending
that a pending disposer is complete.

Adapting `cordis-rs` to that contract would replace the mechanism that coordinates
its fibers, services, and disposal. That is effectively a semantic fork and
would leave Zuno owning an adapter to a second lifecycle model.

### What Zuno already guarantees

The current native kernel already provides:

- side-effect-free asynchronous `Component::prepare`;
- typed `provide` and `require`;
- deferred effects whose start returns an exact asynchronous disposer;
- start-order acquisition and reverse-order cleanup;
- positive stop deadlines;
- `Failed`, `Uncertain`, and `Closed` outcomes;
- full candidate preparation before old-composition shutdown;
- atomic publication of candidate services;
- restoration through a fresh prepare/start cycle;
- child-before-parent shutdown;
- lifecycle snapshots and scrubbed diagnostics.

Provider replacement already re-prepares consumers. Tests cover local provider
replacement, revealed providers after unmount, and profile-level dependencies.
The remaining opportunity is to avoid re-preparing the complete local scope when
an exact dependency closure is sufficient. This is an optimization and
composition feature, not a missing correctness repair.

Zuno also already has declarative agents, workflows, skills, WASI Component
tools, trusted process tools, strict authorization, reverse host shutdown, and
`Uncertain` handling. Runtime-loaded Rust dynamic libraries are intentionally
out of scope.

## Problems still worth solving

### 1. Dynamic capabilities need product adoption

The native service plane remains indexed by Rust `TypeId`, while the runtime now
also owns stable named capability descriptors for dynamic boundaries. The
registry foundation can express identity, contract, provenance, owner,
availability, and generation without storing an unchecked executable value.

Replacing `TypeId` with strings would discard useful compile-time safety, so the
two coordinated planes remain intentionally distinct:

- typed services for native Rust composition;
- named capabilities for dynamic and cross-process composition.

The first product adoption is complete: extension Tool contributions and the
immutable Agent Profile, Workflow Template, and Skill snapshot publish through
the registry in the same profile transaction. The remaining vertical gap is
MCP and remote-host manifests, runtime package descriptors, and the final
post-permission ToolRegistry projection.

### 2. Recomposition is correct but coarse

Every local mutation prepares a complete candidate composition. This keeps
replacement safe, but a small provider or contribution change can restart
unrelated components.

The runtime records requirements, ownership, and the named provider generation
observed by each consumer, but it does not yet calculate the minimal affected
closure.

### 3. Hooks are not one lifecycle-owned event model

Tool, permission, server, extension, MCP, and product-specific hooks have
different registration and cleanup paths. Generic event dispatch must not
replace typed security policy, but reusable ordering, cancellation, timeout, and
subscription ownership should not be reimplemented by every subsystem.

### 4. Runtime package execution is installation-oriented

Process-local `extension_define` packages are deliberately declarative.
Executable WASI/process packages are installed before the host starts so their
provenance and paths are stable.

If Zuno later allows an agent to author and activate executable behavior during a
session, that capability needs an explicit ephemeral package transaction,
approval record, capability envelope, and isolated execution boundary. It is not
implemented by exposing `PrepareContext` or loading a Rust library.

### 5. Product lifecycle adoption is incomplete

The kernel exists, but some long-lived product managers still own independent
generation and shutdown mechanisms. MCP, LSP, background execution, provider
clients, server routes, and frontend projections should converge on shared
lifecycle and capability contracts where doing so removes duplicate ownership.

They should not be rewritten solely to satisfy a framework shape. Migration is
justified only when it removes a real duplicate lifecycle or enables
transactional replacement.

## Target architecture

```text
HarnessRuntime
  |
  +-- zuno-runtime
  |     Component / PrepareContext / Effect / LifecycleState
  |     sole owner of start, stop, replacement, and Uncertain
  |
  +-- capability registry
  |     Typed(TypeId) for native Rust
  |     Named(CapabilityKey) for dynamic boundaries
  |     owner + generation + contract + availability
  |
  +-- dependency graph
  |     consumer requirements + observed provider generations
  |     deterministic affected-closure calculation
  |
  +-- async event/policy bus
  |     lifecycle-owned subscriptions
  |     emit / serial / bail / waterfall / parallel barrier
  |
  +-- zuno-extension
        declarative contributions
        capability-restricted WASI components
        contained trusted process hosts
```

There is one lifecycle authority. Registries, buses, loaders, and adapters return
owned handles or deferred effects to that authority; none invents an independent
definition of “stopped”.

## Implemented capability registry foundation

The public shape should remain small:

```rust
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CapabilityKey {
    pub namespace: String,
    pub name: String,
    pub version: CapabilityVersion,
    pub scope: CapabilityScope,
}

pub enum RequirementKey {
    Typed(TypeId),
    Named(CapabilityKey),
}

pub struct CapabilityDescriptor {
    pub key: CapabilityKey,
    pub owner: ComponentId,
    pub generation: u64,
    pub contract: CapabilityContract,
    pub provenance: CapabilityProvenance,
    pub availability: CapabilityAvailability,
}
```

Rules:

1. Native callers prefer typed services.
2. Named lookup is limited to extension, MCP, workflow, remote, and diagnostic
   boundaries that genuinely receive names at runtime.
3. Every registration has one owner and monotonically increasing generation
   within its scope.
4. Duplicate names in the same namespace, version, and scope fail before effects
   start.
5. Cross-scope inheritance and shadowing follow the existing runtime rules.
6. Capability withdrawal happens before provider cleanup starts.
7. A consumer cannot observe a provider generation that was never atomically
   published.
8. A capability contract describes compatibility and schema; it is not an
   unchecked `Any` map.

The implemented first version deliberately omits distributed discovery, semantic
version solving, arbitrary service migration, and executable values in the named
registry.

## Dependency reconciliation

Preparation records both the requirement and the generation resolved:

```text
consumer -> RequirementKey -> provider owner -> observed generation
```

When a provider changes:

1. build and validate the candidate definitions;
2. calculate providers whose key, generation, or contract changes;
3. walk reverse dependencies to form the affected consumer closure;
4. expand the closure for bundle atomicity and explicit exclusive-resource
   groups;
5. preserve the current full-scope path as the reference implementation;
6. compare the optimized plan against the reference in tests;
7. withdraw affected routing and services;
8. stop the affected subgraph in reverse dependency/effect order;
9. prepare and start the candidate subgraph;
10. atomically publish the new generations.

If a required named provider disappears, initial versions should mark the
consumer inactive and report a typed missing dependency. Automatic parking and
reactivation may be enabled only after the state is represented in lifecycle
snapshots and cancellation is proven.

No transition may mechanically replay a tool, network write, process request, or
other non-idempotent operation.

## Async event and policy bus

The event layer should provide five explicit modes rather than one ambiguous
callback API:

| mode | behavior |
| --- | --- |
| `emit` | notify subscribers in deterministic order; collect diagnostics |
| `serial` | await each subscriber in deterministic order |
| `bail` | return the first decisive value |
| `waterfall` | pass a typed value through ordered transformations |
| `parallel_barrier` | start eligible handlers concurrently and await all results |

Every subscription records:

- event key and typed payload contract;
- owning component;
- stable priority/order;
- cancellation token;
- deadline policy;
- whether failure is advisory or transition-fatal.

Security-critical paths remain typed and fail closed. Permission decisions,
credential access, tool-effect classification, replay policy, and lifecycle
state changes must not become arbitrary string events. The common bus supplies
ownership and dispatch mechanics; domain APIs retain their invariants.

## Executable extension boundary

Custom agents and workflows may require network, files, environment variables,
tools, and delegation. These capabilities are valid requirements, but they do
not justify unrestricted in-process native code.

Zuno should keep three execution tiers:

1. declarative packages for agents, workflows, commands, and skills;
2. WASI Component packages for bounded runtime-loaded tools;
3. trusted process packages for capabilities that cannot be represented in
   WASI.

An optional future ephemeral package flow should be:

```text
define source/artifact
  -> validate manifest and provenance
  -> request required human approval
  -> stage isolated host
  -> initialize and inspect contributions
  -> transactionally publish routing
  -> run under native permission and replay policy
  -> withdraw routing
  -> shutdown and prove quiescence
  -> retain audit record
```

Guest code receives only declared capability facades and invocation metadata. It
does not receive `HarnessRuntime`, `PrepareContext`, raw credential stores, or
unrestricted host references.

For WASI, network and environment grants remain separate. A plugin that uses a
proxy needs both network authority and explicit proxy environment names. A
trusted process inherits host authority and must continue to be classified as
side-effecting and non-replayable.

## DSH semantic mapping

| DSH/Cordis concept | Zuno decision |
| --- | --- |
| `ctx.effect` | keep native deferred `Effect`; Zuno's asynchronous disposer and `Uncertain` contract are stronger |
| service `get/provide` | add named capability plane beside typed services |
| `inject` and service epoch | record resolved generations and affected dependency closure |
| `ctx.on` / `emit` | add lifecycle-owned async subscriptions |
| `bail` | add typed first-decision dispatch |
| `waterfall` | add typed ordered policy transformation |
| `parallel` | add explicit parallel barrier, not implicit callback concurrency |
| function `apply` plugin | keep `Component::prepare` plus declarative package contributions |
| isolate labels | represent namespace and scope in `CapabilityKey` |
| loader groups | map to validated package/profile composition |
| JavaScript VM host | do not copy as a security boundary |
| Rust dynamic loader | reject for runtime plugins |
| logger/timer/schemastery utilities | do not adopt; use tracing, Tokio, serde, and schemars |

## Delivery plan

### Phase 0: contracts and reference tests

- Freeze lifecycle invariants in tests and documentation.
- Add fixtures for dependency disappearance, replacement, restoration, and stale
  generation.
- Keep full-scope recomposition as the behavioral oracle.
- Record runtime and component snapshots before and after every fixture.

Exit criteria: no new registry or optimizer code exists without a failing
contract test.

### Phase 1: dual capability registry

- Add named capability descriptors and registration handles.
- Preserve the typed API without string conversion.
- Project owner, generation, contract, scope, and availability.
- Integrate duplicate detection and atomic publication with candidate
  composition.

Initial pilot: expose extension tool contributions through named capability
descriptors while their native `Tool` objects remain typed.

Exit criteria: registering, shadowing, withdrawing, and replacing a named
capability is transactional and leaves no stale route.

### Phase 2: dependency graph and generations

- Record `RequirementKey` plus observed provider generation during preparation.
- Build deterministic reverse-dependency indexes.
- Add affected-closure planning without changing execution behavior.
- Compare the calculated closure with full-scope recomposition in test-only
  diagnostics.

Exit criteria: closure calculation is stable, complete, and does not yet alter
production transitions.

### Phase 3: minimal reconciliation

- Enable closure-based transition behind an internal feature flag.
- Expand closures for bundles and exclusive-resource groups.
- Add cancellation, restoration, and `Uncertain` tests under concurrent
  replacement.
- Fall back to full-scope recomposition when the optimizer cannot prove a safe
  boundary.

Exit criteria: optimized and reference paths produce equivalent service,
snapshot, and cleanup outcomes.

### Phase 4: event and policy bus

- Implement lifecycle-owned subscription handles.
- Add serial, bail, waterfall, and parallel-barrier modes.
- Migrate one advisory hook path first.
- Migrate one typed policy path only after fail-closed behavior is proven.

Exit criteria: unload removes every subscription before its producer can emit a
new callback.

### Phase 5: MCP vertical validation

Use MCP as the first complete dynamic-provider test:

```text
server connects
  -> named tool/prompt capabilities publish
  -> agent/tool consumers resolve one generation
  -> catalog changes or server disconnects
  -> old routing withdraws
  -> affected consumers reconcile
  -> server reconnects
  -> new generation publishes
```

The existing MCP generation manager remains authoritative until the shared
model proves it can replace that responsibility without losing reconnect,
shutdown, or diagnostic behavior.

Exit criteria: reconnect and shutdown tests prove there is one registration,
one owner, and no call reaches a withdrawn generation.

### Phase 6: optional ephemeral executable packages

- Design durable definition, approval, provenance, and audit records.
- Start with WASI; add a process worker only when a required capability cannot be
  represented safely.
- Reuse native permission, strict HITL, replay, cancellation, usage, and UI
  projections.
- Never auto-run model-authored code merely because it parsed successfully.

Exit criteria: define/run/stop/undefine is transactional, survives process loss
without replay, and reports `Uncertain` when cleanup cannot be proven.

### Phase 7: extraction assessment

Do not extract a framework automatically. Evaluate the gates below and record a
new architecture decision.

## Independent-crate extraction gates

An independent async lifecycle/capability crate is justified only when all are
true:

1. at least two real hosts use it, not two Zuno modules behind the same
   composition root;
2. the core API contains no Zuno agent, session, tool, provider, or UI types;
3. the API has remained stable across two product milestones;
4. cancellation, timeout, process cleanup, and uncertain outcomes have
   production evidence;
5. the DSH/Cordis semantic fixtures pass for every compatibility claim;
6. the crate can be used without the Zuno extension loader;
7. extraction reduces dependencies or duplicated code instead of adding an
   adapter layer.

Until then, the implementation is a Zuno-native capability subsystem. It should
not advertise full Cordis API compatibility.

If extraction becomes justified, keep Zuno terminology as the canonical API:
`Component`, `PrepareContext`, `Effect`, `CapabilityKey`, and `Uncertain`. A
separate optional facade may map `Plugin`, `Fiber`, and `Inject` terminology for
Cordis users.

## Non-goals

- Embedding the JavaScript Cordis ABI.
- Reproducing the npm package hierarchy.
- Depending on or forking `cordis-rs` as the Zuno runtime.
- Loading third-party Rust dynamic libraries.
- Replacing typed native services with a global string/`Any` container.
- Adding YAML or `$include` before JSON/JSONC composition has a demonstrated
  limitation.
- Reimplementing logging, timers, configuration schema generation, or generic
  utilities already supplied by Zuno's Rust stack.
- Treating process containment as a security sandbox.
- Automatically rewriting code, prompts, agents, skills, or workflows as a
  learning mechanism.

## Required tests

| area | minimum evidence |
| --- | --- |
| capability registry | duplicate, shadow, withdrawal, atomic publish, stale handle |
| dependency graph | provider replace/remove/restore, transitive closure, bundle expansion |
| lifecycle | reverse cleanup, timeout, failed disposer, restoration, repeated shutdown |
| concurrency | concurrent replace requests, cancellation, late generation output |
| event bus | stable ordering, unsubscribe during dispatch, timeout, fail-closed policy |
| MCP pilot | connect, catalog update, reconnect, disconnect, shutdown, no stale route |
| extensions | WASI/process cancellation, process-tree reap, capability denial, redaction |
| process loss | uncertain reconciliation without automatic replay |
| projections | owner, generation, state, diagnostics, and affected closure visible to clients |

Use Tokio paused-time tests where applicable, property tests for dependency
closure and ordering, and concurrency-model tests for registry publication and
replacement races. End-to-end acceptance must include a real MCP reconnect and a
real WASI/process shutdown; unit tests alone do not prove quiescence.

## Likely implementation surfaces

- `crates/zuno-runtime`: capability registry, requirements, generations,
  reconciliation, lifecycle projection.
- `crates/zuno-extension`: contribution descriptors and ephemeral package
  transaction when authorized.
- `crates/zuno-mcp`: first dynamic-provider adapter and reconnect fixtures.
- `crates/zuno-tool` / `crates/zuno-engine`: named contribution resolution
  without weakening typed permission and replay policy.
- `crates/zuno-server` / `crates/zuno-tui`: frontend-neutral capability and
  lifecycle diagnostics.
- `docs/design/component-lifecycle-kernel.md`: update only when an implemented
  phase changes its guarantees.
- `docs/plugins.md` and `docs/harness-runtime.md`: update with implemented,
  user-visible behavior rather than speculative APIs.

## Recommended next task

Phase 0 and the Phase 1 product pilot now meet their exit criteria. Continue
with one bounded Phase 2 consumer-epoch slice:

> Record named provider generations on a real consumer, replace that provider,
> and reactivate only the affected dependency closure while preserving the
> existing full-scope path as the correctness fallback.

MCP, remote-host, and final ToolRegistry descriptor projection can then reuse
the proven key, contract, provenance, and generation model instead of creating
another registry.
