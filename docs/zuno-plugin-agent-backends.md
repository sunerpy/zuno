# Plugin-owned Agent backends

Zuno plugins can declare Agent backend factories without installing a business
workflow or choosing a model. A workflow references the declaration by its
namespaced `agentRef`; its optional `executionProfile` remains the sole owner of
provider, model, reasoning effort, service tier, permission, and sandbox
settings.

This boundary adapts DSH's composition principle without adopting its runtime or
ABI. Backend packages, workflows, and execution profiles are independent and
replaceable.

## Declare the resource

A legacy Codex-compatible plugin manifest may point at one declaration file:

```json
{
  "name": "team-agents",
  "agentBackends": "./agent-backends.json",
  "workflows": "./workflows"
}
```

A portable Agent Plugin uses the same field inside its `extensions.com.openai`
object. Zuno accepts only a `./` path below the installed plugin root.

The referenced JSON document uses the versioned `zuno.agent-backends/v1`
contract:

```json
{
  "apiVersion": "zuno.agent-backends/v1",
  "backends": {
    "native-build": {
      "kind": "native-codex"
    },
    "claude-review": {
      "kind": "claude-code",
      "command": "claude",
      "envVars": ["CLAUDE_CONFIG_DIR"]
    },
    "external-review": {
      "kind": "acp",
      "command": "./bin/external-agent-acp",
      "args": ["--stdio"],
      "envVars": ["AWS_PROFILE"]
    }
  }
}
```

The runtime ID is `<plugin manifest name>/<backend key>`, for example
`team-agents/external-review`. A user-owned workflow can select it without
embedding deployment settings:

```yaml
spec:
  routes:
    review:
      agentRef: team-agents/external-review
      executionProfile: review-agent
```

`$ZUNO_HOME/review-agent.config.toml` remains authoritative for provider, model,
reasoning effort, service tier, approvals, permission profile, and sandbox. Each
backend advertises which values it can enforce and rejects unsupported required
capabilities; changing the profile does not modify the workflow or plugin package.

## Current adapter mapping

- `native-codex` consumes the normal Codex execution profile in-process.
- `acp` forwards the profile model, reasoning effort, and selected named
  permission-profile ID. Legacy `sandbox_mode` profiles are conservatively
  projected from their effective policy to `:read-only`, `:workspace`, or
  `:danger-full-access`, rather than forwarding a stale display sidecar. A
  dedicated ACP work-mode profile option is not yet exposed, so `mode` remains
  unset.
- `claude-code` forwards model and reasoning effort. A read-only profile maps
  to Claude `plan`; an explicitly selected `:danger-full-access` profile whose
  effective permission profile is disabled maps to `bypassPermissions`.
  Managed and custom profiles otherwise use conservative non-interactive
  `dontAsk`. Zuno never infers a bypass from ambiguous writability.

For external products, the profile's provider ID is frozen in the route binding;
ACP also receives it as `modelProvider` in the Zuno metadata extension. Claude
Code transport and authentication remain owned by the selected Claude CLI
deployment (for example its Kiro-compatible endpoint settings or AWS Bedrock
environment). Zuno does not reinterpret an OpenAI provider definition as a
Claude credential. A plugin selects and binds that deployment through its
command and explicit `envVars`; use separate namespaced backend declarations
when multiple Claude provider deployments must coexist.

For both external adapters, product-level permission options are defense in
depth, not the sandbox boundary. Before spawn, Zuno independently transforms
the exact executable and fixed arguments through Codex's platform sandbox using
the execution profile's effective filesystem and network policy. A managed
restricted profile never falls back to a native process when the required
Linux, macOS, or Windows sandbox backend cannot be prepared; the call fails with
an access-policy error before the external Agent starts. An explicitly disabled
(`:danger-full-access`) profile remains an intentional native launch, while an
`external-sandbox` profile relies on the already established outer sandbox and
does not add a nested one. Consequently, an external Agent that needs provider
network access must use a profile whose network policy permits that access.
The first preview does not yet attach a session-owned managed network proxy to
an external backend process; if such a proxy is configured, dispatch fails
before spawn instead of silently bypassing its domain or credential policy.

These limits must not be worked around by adding mode or permission fields to
the plugin declaration or workflow.

## Declaration fields

- `kind`: `native-codex`, `claude-code`, or `acp`.
- `command`: optional for `claude-code`, required for `acp`, and forbidden for
  `native-codex`. Claude may use a bare executable name or a package-relative
  `./` path; ACP requires an immutable package-relative `./` command. Package
  paths use forward slashes. Absolute paths and parent traversal are rejected.
  A selected package command must resolve to a regular, non-symlink file inside
  the installed plugin root and must be executable on Unix.
- `commandWindows`: optional Windows-specific command selected instead of
  `command`; it follows the same path rules.
- `args`: fixed process arguments for `acp`. Claude Code arguments are
  host-owned so a plugin cannot bypass its bounded non-interactive contract.
- `envVars`: names of host environment variables that may be forwarded to the
  external process. Values never enter the manifest, loaded-plugin cache, or
  workflow ledger.
- `startupTimeoutMs`: ACP startup deadline, default `20000`, range
  `1..300000`.
- `runTimeoutMs`: optional external-process deadline, range `1..86400000`.
- `disposeGraceMs`: process-reaping grace period, default `3000`, range
  `1..60000`.
- `maxMessageBytes`: ACP message or Claude output limit, default `8388608`,
  range `1..67108864`.

`native-codex` forbids every process field. Claude forbids `args` and
`startupTimeoutMs`; Zuno owns its fixed non-interactive protocol arguments and
startup behavior.

Unknown fields, unsupported schema versions, malformed IDs, unsafe paths, or an
invalid kind/config combination disable that plugin contribution. Duplicate
namespaced IDs are retained through plugin loading so the backend registry can
fail closed rather than silently pick a winner.

At workflow admission, Zuno bypasses the normal plugin capability cache and
reloads these declarations. The backend generation covers the owning plugin
version, exact declaration-document bytes, current-platform package executable,
fixed arguments and limits, and a digest of explicitly forwarded environment
values. The complete non-secret route binding is persisted before execution,
while the prepared backend retains that exact generation in memory for the
whole run. The selected platform sandbox helper and legacy-Landlock mode are
also included in the durable binding. Package executable SHA-256 is checked
again immediately before an
external process is spawned. After process restart, a queued run is resumed
only when freshly resolved bindings still equal the persisted binding. Thus a
later profile, declaration, executable, or forwarded-environment change affects
new admission and makes drifted recovery fail closed, without mutating an
already running generation. Environment values themselves are never persisted.
A bare Claude PATH command remains an explicit deployment dependency, but it is
not left floating after admission. Each App Server factory snapshot resolves it
to a canonical absolute regular executable, includes that path and SHA-256 in
the factory revision and persisted route binding, and checks the digest again
immediately before spawn. Missing or changed bytes fail selection or queued
recovery; installing a new Claude Code version requires a new App Server
snapshot (normally a restart).

## What is intentionally absent

The declaration does not accept prompts, workflow nodes, model IDs, providers,
reasoning effort, service tier, permission modes, sandbox bypasses, or
credentials. Workflows remain external resources under `$ZUNO_HOME/workflows`,
`<project>/.zuno/workflows`, or an explicitly declared plugin workflow root.
There is no built-in `frontend-consensus` workflow or implicit route.

See [`examples/zuno-plugins/agent-backends`](../examples/zuno-plugins/agent-backends/)
for an opt-in package skeleton. Repository examples are documentation only and
are not installed or discovered automatically.
