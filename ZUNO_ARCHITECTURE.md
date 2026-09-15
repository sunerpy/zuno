# Zuno next architecture

Zuno is a source fork of OpenAI Codex. The product executable remains named
`zuno`; Codex is the implementation baseline and upstream, not a subprocess
that Zuno wraps.

## Product boundary

Zuno owns generic capabilities, never a user's business process. In particular,
there is no built-in `frontend-consensus` workflow, model choice, prompt, or
routing branch. A team may install or write such a workflow as data, select its
models through logical execution profiles, and remove it without rebuilding
Zuno.

This is a distribution and API invariant, not only an implementation detail.
`frontend-consensus` is not a reserved workflow ID, command, App Server method,
packaged resource, default profile, or automatically installed plugin. Optional
examples in this repository are documentation inputs only; an example becomes
available only after a user explicitly copies or installs it into a discovery
root. A UI may list and run discovered workflows, but it must not own a hidden
catalog or inject a product workflow. CLI, TUI, ACP, App Server clients, and
external automation all consume the same generic workflow control plane.

The core contains only:

- workflow discovery, validation, execution, cancellation, and durable run/call
  records;
- engine providers for `graph/v1`, controlled `javascript/v1`, and the optional
  `node-worker/v1` process boundary;
- logical Agent routes and execution profiles whose concrete provider, model,
  reasoning effort, permissions, and service tier come from user configuration;
- Agent backend interfaces and providers;
- ACP projection over the same Codex App Server thread and turn state;
- plugin resource discovery and lifecycle ownership.

A workflow is an independent YAML or JSON `zuno.workflow/v1` document. It is not
a Rust enum variant. Definitions are discovered only from these external roots:

- user: `$ZUNO_HOME/workflows/`;
- project: `<trusted-project>/.zuno/workflows/`;
- plugin: files or directories explicitly declared by that plugin's `workflows`
  manifest field.

This is Zuno's adaptation of the DSH composition principle: workflows are
replaceable resources contributed at runtime, while Zuno keeps its own typed
DSL, lifecycle, durable ledger, permission model, and wire contracts. DSH is a
design source, not an embedded runtime or a compatibility target. Discovery is
deterministic and reports a source identity and SHA-256 digest. A malformed
document does not become an implicit prompt or executable script.

App Server exposes this generic control plane through `workflow/list`,
`workflow/read`, `workflow/validate`, `workflow/start`, `workflow/run/read`, and
`workflow/run/cancel`. `workflow/start` accepts a client-supplied run ID and the
executable digest returned by discovery, then responds after durable admission;
clients observe completion through the run record instead of holding the start
request open. These methods operate on configured resources only and do not
create, install, or silently select a product workflow.

## Workflow engines

`graph/v1` describes a bounded dependency graph. `javascript/v1` runs through a
Zuno-owned Code Mode companion process with typed host calls (`agent`, `phase`,
`log`, and `checkpoint`). The companion is an isolation and cancellation
boundary; JavaScript never receives an ambient Rust or filesystem ABI.
`node-worker/v1` is an optional external engine for deployments that explicitly
select and authorize Node.

Workflow v1 starts only through an explicit App Server client
`workflow/start` call against a discovered, digest-bound user, project, or
plugin document. It exposes no model-visible workflow-install or auto-start
tool, so model-generated or subsequently mutated scripts cannot enter this
admission path. Any future model-visible dynamic source must add a distinct
`workflow:dynamic` authorization before it can execute; that future capability
is not part of v1. Authorization never weakens sandbox, permission, resource,
or Agent policy. An unknown result around a side effect is `uncertain`; it is
never mechanically replayed.

### Durable workflow ledger

Accepted workflow runs and engine-to-host calls are stored in the independent
`zuno_workflows_1.sqlite` database through Codex's audited SQLite connection
configuration. A client-supplied run ID is idempotent only while the canonical
start-request digest still matches. Reusing a run or call ID with different
input fails closed.

The host records a call as `running` before dispatch. A repeated call ID returns
the existing record and is never permission to invoke the side effect again.
After process loss, every `running` call and run becomes `uncertain`; queued
runs and approval requests remain recoverable. Cancellation requests are
persisted before signalling the engine and prevent any new host call from
starting. Completed, failed, cancelled, and uncertain outcomes are immutable
unless a future explicit authoritative-inspection operation defines a typed
resolution.

The workflow database carries its own format marker, written last in the same
transaction as schema creation. Missing, corrupt, older-without-a-migration, or
future format markers fail closed without rewriting durable user state.

## Agents as plugins

The backend contract separates selection from execution:

- `native-codex` creates an in-process child thread through Codex's existing
  thread manager and Agent runner. It does not launch a second CLI.
- `claude-code` is a bounded external process provider. Model, effort,
  environment, timeout, and output limits are explicit inputs. Read-only
  profiles map to Claude `plan`, an explicit `:danger-full-access` profile maps
  to `bypassPermissions`, and other managed/custom profiles remain
  fail-closed `dontAsk`; bypass is never inferred from ambiguous writability.
- `acp` is a supervised external stdio provider. Enabled plugins may expose it
  (or namespaced aliases for the native Codex and Claude Code providers) through
  a versioned `agentBackends` declaration. Native ACP server mode is a client
  surface over the shared App Server runtime, not a second loop. The adapter
  forwards model, effort, and the named permission-profile ID. Legacy sandbox
  syntax is mapped from its effective policy to a conservative built-in profile
  ID instead of forwarding stale display metadata. A dedicated ACP work-mode
  profile field is not exposed yet.

For external products, the profile's provider ID is frozen in the route binding;
ACP also receives it as `modelProvider` in the Zuno metadata extension. Claude
Code transport and authentication remain owned by the selected Claude CLI
deployment (for example its Kiro-compatible endpoint settings or AWS Bedrock
environment). Zuno does not reinterpret an OpenAI provider definition as a
Claude credential. A plugin selects and binds that deployment through its
command and explicit `envVars`; use separate namespaced backend declarations
when multiple Claude provider deployments must coexist.

The child product's permission protocol is defense in depth, not proof of host
confinement. Claude Code and ACP process launches are independently transformed
through Codex's platform sandbox with the effective execution-profile filesystem
and network policy. If a managed restricted profile cannot be enforced on the
host, dispatch fails before spawn and never widens to native execution. A
disabled full-access profile intentionally launches natively; an external
sandbox profile retains the already established outer enforcement without
nesting another sandbox. Until the external adapter can retain the exact
session-owned managed-proxy generation, a configured managed network proxy
makes the dispatch fail before spawn rather than bypassing that policy.

A backend must advertise capabilities before dispatch. The Agent backend
factory registry exposes a stable, sorted inventory of providers, resolves one
exact registration generation, validates the selected profile's requirements,
and only then constructs the run-specific backend instance. The built-in
`native-codex` and `claude-code` entries are provider factories, not built-in
workflows, route policies, or model bindings. Plugin declarations mount
`<plugin-name>/<local-id>` factories into an immutable App Server registry
snapshot. Profile replacement swaps the complete snapshot atomically; a
resolved factory retains its exact generation while later plugin changes affect
only new admission. Factory registrations use exact mount/disposer lifecycle.
Unmounting a provider prevents new resolution while an already admitted
generation remains usable. The constructed backend must match the
factory's advertised kind and capabilities or the call fails closed before
execution. Unsupported resume, steering, permission, model, reasoning, or
service-tier requirements fail before the backend starts work. Option
capability scope distinguishes values fixed by an execution profile from
explicit per-run overrides; profile support never silently authorizes an
invocation override. Cancellation owns the child process/thread lifecycle and
waits for settlement.

Workflow admission freezes the workflow and executable digests together with a
non-secret `zuno.workflow-bindings/v1` snapshot. Every route records the backend
factory revision and plugin generation plus the effective execution profile,
model/provider, reasoning, service tier, approvals, permission profile, sandbox,
working directory, and workspace roots. Plugin discovery is freshly resolved at
admission. The platform sandbox helper selection and legacy-Landlock feature
state are part of the same durable binding; the prepared backend and forwarded
environment remain the exact in-process generation for the run. Credential values are redacted and may not
appear in the workflow ledger; explicitly forwarded plugin environment values
contribute only a digest. A new admission binds changed profile or environment
values. After process restart, queued recovery re-resolves every route and
continues only if the complete binding still matches. The selected package
executable SHA-256 is also checked immediately before external process spawn,
closing the admission-to-spawn mutation window. Plugin generation includes the
plugin version, exact declaration bytes, fixed arguments and limits, and
current-platform package executable bytes. At each immutable App Server factory
snapshot, a bare Claude PATH command is resolved to a canonical absolute regular
executable; its path and SHA-256 enter the factory revision and route binding,
and the same digest is checked again immediately before spawn. A missing or
changed executable fails selection or queued recovery instead of silently using
a different PATH generation.

## Profiles and models

Workflow definitions select an Agent backend through `agentRef` and may name a
logical `executionProfile`; they do not need to hard-code model IDs. The backend
factory and named profile remain separate bindings: a profile supplies normal
Codex model/runtime configuration, while an enabled plugin explicitly mounts
any additional backend factory. Consequently the same user-owned workflow can
change model, provider, reasoning, and policy bindings without editing or
recompiling Zuno, while changing its backend remains an explicit workflow/plugin
configuration decision.

Codex's existing configuration remains the authority for model provider,
model, reasoning effort, service tier, approval, and sandbox policy. Zuno adds
composition; it must not copy credentials into workflow documents.

## ACP ownership

ACP session operations translate to Codex App Server thread and turn operations.
Thread persistence, input ordering, interrupts, approval requests, tool events,
and child Agent events therefore have one source of truth. Ordinary input,
active-turn steering, turn interruption, Goal resume, and protected recovery
gates remain different typed operations.

## Upstream synchronization

`UPSTREAM_CODEX.toml` pins the exact Codex release commit and tree used as the
base. `DESIGN_SOURCES.toml` separately records targeted, non-ABI design references. `FORK_DELTA.toml` records each Zuno-owned or shared-modified capability.
`scripts/zuno_upstream.py` performs candidate-only synchronization:

1. fetch tags from the configured upstream;
2. validate the recorded tag, commit, and tree;
3. select an exact stable `rust-vX.Y.Z` tag;
4. report upstream/Zuno overlap;
5. for `prepare`, require a clean reviewed source and create a new branch and
   worktree at the new exact release;
6. compute the exact binary-safe tree delta from the old Codex baseline to the
   reviewed Zuno source, apply only that delta to the candidate, and update its
   manifest. This deliberately excludes unrelated legacy-main parents retained
   by the one-time source-history bridge.

The command never merges, resets, or updates the active Zuno branch. Conflicts
remain in the candidate worktree for review. Tests, schema generation, native
platform checks, packaging, and runtime smoke are required before a candidate
can replace the recorded baseline.

## CI and release ownership

Zuno does not inherit OpenAI's private runner, signing, npm, or R2 authority.
The protected `main` branch is gated by the public-runner `zuno/pr-gate`, while
OpenAI-specific Codex workflows remain manually callable for compatibility
research only. On a PR head, six native target jobs build the Zuno entrypoint
and companion package once, run package-layout and native ACP smoke, emit GitHub
provenance attestations, and seal all archives and checksums to the exact PR
head, tree, workflow run, and attempt. Promotion accepts that immutable run ID,
requires the merge commit tree to equal the certified tree, creates
`zuno-vX.Y.Z`, verifies downloaded draft assets byte-for-byte, and publishes a
non-latest preview without recompilation. Codex `rust-v*` automation is not a
Zuno release path.

The first preview intentionally has no automatic package mutation authority.
CLI/TUI update actions, inherited installers, the `zuno app` Codex Desktop
compatibility launcher, persistent managed App Server mutations, its updater
loop, and Codex tag publishers all fail closed. Daemon Version remains a
read-only diagnostic; Start, Restart, and Stop return before daemon filesystem
access. Foreground App Server, ACP, and remote-control transports remain usable.
TUI startup tips are a Zuno-owned local resource and perform no upstream
announcement fetch. Automatic update may be enabled only after a Zuno-owned
staged installer proves lock, checksum, atomic switch, rollback, and
installed-byte smoke on all six native targets.

```sh
# Read-only report against the newest locally/freshly fetched stable tag.
python3 scripts/zuno_upstream.py --json check

# Prepare a separate candidate after the Zuno delta has been reviewed and
# committed. This never changes the active worktree.
python3 scripts/zuno_upstream.py prepare \
  --target rust-vX.Y.Z \
  --worktree ../zuno-upstream-X.Y.Z
```
