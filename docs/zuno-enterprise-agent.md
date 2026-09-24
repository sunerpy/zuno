# Enterprise agent on the Codex harness

This document is the design baseline for building an enterprise-grade agent on
Zuno. It records what the Codex harness already provides, what Zuno adds today,
which enterprise concerns still have no primitive, and the order in which Zuno
intends to close them. It is a design source, not a feature announcement: every
capability listed as *planned* stays out of the product until it is implemented
behind an explicit contract in `FORK_DELTA.toml`.

## 1. What "the Codex harness" is

OpenAI describes the harness as "the agent loop and logic that underlies all
Codex experiences" and the App Server as the client-facing, bidirectional
JSON-RPC edge over it ("Unlocking the Codex harness: how we built the App
Server", 2026-02-04, <https://openai.com/index/unlocking-the-codex-harness/>).
The harness is `codex-rs/core` plus the crates it composes; the App Server
(`codex-rs/app-server`) turns the core event stream into a small set of stable
notifications and accepts requests for threads, turns, and items.

The building blocks an enterprise agent composes, with their locations in this
repository:

| Concern | Primitive | Where |
| --- | --- | --- |
| Conversation state | Thread, Turn, Item; create/resume/fork/archive; durable rollout history | `core/src/thread_manager.rs`, `thread-store`, `protocol` |
| Client edge | App Server JSON-RPC over stdio, Unix socket, or WebSocket; `initialize` capability negotiation | `app-server`, `app-server-protocol`, `app-server-transport` |
| Tool execution | `ToolRouter`/`ToolRegistry`, sandboxed shell and file tools, unified exec | `core/src/tools`, `core/src/unified_exec` |
| Isolation | Seatbelt (macOS), bubblewrap + seccomp + Landlock (Linux), native Windows sandbox; `execve` wrapper for escalation | `sandboxing`, `linux-sandbox`, `windows-sandbox-rs`, `shell-escalation` |
| Policy | `approval_policy`, permission profiles, Starlark `execpolicy` rules, network proxy allow/deny | `core/src/config`, `execpolicy`, `network-proxy` |
| Managed configuration | `requirements.toml` layered from system, MDM, and cloud sources; cannot be overridden by users | `config`, `cloud-config` |
| Lifecycle hooks | Twelve hook events (`SessionStart` … `Stop`), MCP-backed hooks, managed hook allow-lists | `hooks`, `core/src/hook_runtime.rs` |
| Extensions | Plugins (skills, MCP servers, hooks, apps), skills, MCP client, `ext/extension-api` contributors | `plugin`, `core-plugins`, `skills`, `rmcp-client`, `ext/` |
| Code mode | Model-authored scripts run in a V8 host that calls tools through the same policy layer | `code-mode-*`, `core/src/tools/code_mode` |
| Second-opinion review | Guardian reviewer for actions that would otherwise need a human approval, with a circuit breaker | `core/src/guardian`, `guardian-context`, `ext/guardian-v2` |
| Identity | ChatGPT and API-key login, keyring storage, agent identity JWTs, workload identity | `login`, `keyring-store`, `agent-identity`, `workload-identity` |
| Telemetry | OpenTelemetry traces, metrics and log events; analytics opt-out | `otel`, `analytics` |

Two constraints in OpenAI's own documentation shape the design:

- `codex app-server` is "experimental and isn't supported for production
  workloads", and the WebSocket transport is "experimental and unsupported"
  (<https://developers.openai.com/codex/app-server>). Zuno therefore treats the
  App Server as an internal boundary that Zuno owns and tests, not as a public
  API it consumes from upstream.
- The command-level network proxy only governs sandboxed commands; MCP servers,
  browsers, and the model traffic bypass it
  (<https://learn.chatgpt.com/docs/sandboxing.md>). Egress control for an
  enterprise deployment has to happen outside the harness.

## 2. What Zuno already adds

Each item is an implemented capability in `FORK_DELTA.toml`.

- **Single static binary.** `zuno-standalone-x86_64-unknown-linux-musl` embeds
  the code-mode host, so a server deployment is one file with no dynamic loader
  and no Node.js (`server-strict-standalone`).
- **Native ACP.** `zuno acp` exposes the Agent Client Protocol as a projection
  over the same threads and turns; there is no second agent loop
  (`native-acp`, see `docs/zuno-acp.md`).
- **Strict approval.** `approval_policy = "untrusted"` prompts for every command
  and edit unless an explicit rule allows it, including terminal input into an
  already approved shell (`server-strict-standalone`).
- **Composable agents.** Plugins may mount native Codex, Claude Code, or ACP
  agent factories through `zuno.agent-backends/v1`; user-owned YAML workflows
  orchestrate them with a graph or a dynamic script engine, and no business
  workflow ships in the binary (`agent-backends`, `workflow-runtime`).
- **Upstream tracking.** Every stable Codex release is replayed onto the
  reviewed Zuno delta, built once, and promoted from the sealed bytes; conflicts
  that exist only because Zuno renamed Codex text resolve automatically
  (`upstream-sync`, see `docs/zuno-upstream-sync.md`).
- **Fail-closed inheritance.** Inherited installers, daemon updaters, and tag
  publishers are disabled so the fork cannot silently mutate a machine
  (`zuno-ci-release`).

## 3. Enterprise concerns and their gaps

The table separates what the harness offers from what must be built. "Built on
top" means the work lives in Zuno-owned paths so that upstream synchronization
cost stays near zero; "shared-modified" changes would be recorded in
`FORK_DELTA.toml` with an explicit contract.

| Concern | Harness primitive today | Gap Zuno must close |
| --- | --- | --- |
| Identity & SSO | ChatGPT login, API keys, Bedrock IAM, `allowed_login_methods`, agent identity JWTs | Map an enterprise IdP (OIDC) identity to a Zuno principal; identify the caller of every ACP/App Server session; no dependency on a ChatGPT workspace |
| Authorization | `requirements.toml` layers, permission profiles, `execpolicy`, MCP and marketplace allow-lists | Per-principal and per-project policy distribution with versioning; policy change audit |
| Audit | OTel events (`tool_decision`, `tool_result`, `api_request`), rollout JSONL, hooks | Append-only audit sink independent of the working machine; retention and redaction policy; export equivalent to OpenAI's Compliance API for self-hosted deployments |
| Secrets | keyring storage, `deny_read` for `.env` and `~/.ssh`, `secrets` sanitizer | Vault integration and per-thread short-lived credentials; one redaction policy shared by logs, transcripts, and audit |
| Egress | `network-proxy` allow/deny for sandboxed commands | Host-level egress policy covering MCP, model, and browser traffic; corporate CA injection |
| Isolation | OS sandboxes, container mode with `danger-full-access` delegated to the container | One container or worktree per thread; policy for hosts without user namespaces |
| Model routing | Custom `model_provider` (OpenAI-compatible, Azure, Bedrock, local), capability probes | Tenant-aware routing by cost and data residency; central quota |
| Cost | `thread/goal` token budgets, token usage in OTel | Organization budgets, admission control (today the WebSocket server only rejects with `-32001` when overloaded), chargeback |
| Observability | OTel exporters, `/readyz` and `/healthz` on the WebSocket listener | Trace correlation ACP → App Server → tool; SLOs; central logs |
| Multi-tenancy | None: one process shares `ZUNO_HOME`, configuration, and the code-mode host | Process or container per tenant; thread ownership checks; storage partitioning |
| Deployment | stdio (stable), WebSocket/Unix socket (experimental), `codex exec` for CI | Production hardening of the single binary as a service: TLS, authentication, rate limits, externalized `ThreadStore` |
| Upgrades | Upstream promises App Server backward compatibility; JSON Schema is generated per release | Contract tests that pin the App Server and ACP surface Zuno depends on, run on every upstream candidate |
| Compliance | Guardian policy is replaceable; `[analytics] enabled = false`; external prompt scanning hooks | Self-hosted prompt DLP, data residency, plugin provenance beyond the source allow-list |

## 4. Target architecture

```text
              enterprise clients (IDE via ACP, CI via exec, web console)
                       │ ACP over stdio ─┐   │ App Server JSON-RPC (ws/uds)
                       ▼                 ▼   ▼
   ┌────────────────────────────────────────────────────────────────┐
   │  zuno (one static binary)                                      │
   │                                                                │
   │  gateway layer (Zuno-owned)                                    │
   │    principal resolution · policy bundle · admission · audit    │
   │                                                                │
   │  Codex harness (App Server + core)                             │
   │    threads · turns · tools · sandbox · hooks · plugins · MCP   │
   │    guardian · code-mode host (embedded)                        │
   │                                                                │
   │  Zuno extensions                                               │
   │    ACP projection · workflow runtime · agent backends          │
   └───────────────┬───────────────────────────┬────────────────────┘
                   │ OTel + audit events       │ thread store (pluggable)
                   ▼                           ▼
        collector / SIEM / object store     local SQLite+JSONL → external DB
```

Design rules that follow from sections 1–3:

1. **The harness stays the only execution authority.** The gateway layer never
   executes tools or talks to models; it decides who the caller is, which
   policy bundle applies, whether the request is admitted, and what gets
   recorded. This is the same rule the ACP adapter already follows
   (`zuno-acp/src/lib.rs`).
2. **Policy is data, delivered through the harness's own channel.** Enterprise
   policy becomes a signed `requirements.toml` bundle plus `execpolicy` rules
   and hook allow-lists; Zuno adds distribution and versioning, not a second
   policy language.
3. **Every enterprise addition is a Zuno-owned crate or a documented
   shared-modified path.** Anything else raises the cost of the upstream sync,
   which is the mechanism that keeps the fork current.
4. **Experimental upstream surfaces are wrapped, not exposed.** The WebSocket
   transport, `plugin/*`, permission profiles, and code mode are all marked
   experimental upstream. Zuno pins the exact behaviour it relies on with
   contract tests that run against each upstream candidate, so a change shows
   up as a failing PR gate rather than a production incident.
5. **Fail closed.** Missing policy, unreachable audit sink, or an unknown
   principal denies the request. This extends the existing strict approval
   posture from commands to sessions.

## 5. Roadmap

Phases are ordered by dependency, not by calendar. Each phase ends when its
contract is recorded in `FORK_DELTA.toml` and covered by the PR gate.

### Phase 0 — baseline (done)

Single binary, native ACP, strict approval, plugin agent backends, workflow
runtime, automated upstream sync with rebrand replay.

### Phase 1 — contract tests for the surfaces Zuno depends on

- Pin the App Server methods and notification shapes the ACP adapter and the
  workflow runtime consume (`thread/*`, `turn/*`, `item/*`, approvals) with
  request/response fixtures, and run them in `zuno/pr-gate` on every upstream
  candidate.
- Pin the ACP v1 surface Zuno implements (`IMPLEMENTED_METHODS`, error codes,
  prompt block mapping) against the published ACP schema.
- Outcome: an upstream release that changes a depended-upon shape fails the
  candidate PR instead of shipping.

### Phase 2 — principal and audit

- Add a Zuno-owned gateway crate that resolves a caller principal for every ACP
  and App Server session (OIDC bearer token or mTLS certificate), attaches it as
  session metadata, and rejects unknown callers.
- Emit one audit record per approval decision, tool execution, and model
  request into an append-only sink (OTel log exporter first; object storage
  second), with the redaction policy shared with transcripts.
- Outcome: every action answers "who, what, under which policy, with which
  result" without relying on a ChatGPT workspace.

### Phase 3 — policy distribution

- Package `requirements.toml`, `execpolicy` rules, hook allow-lists, and MCP
  allow-lists as a signed policy bundle fetched by the binary at start and on a
  timer; refuse to start without a valid bundle when enterprise mode is on.
- Version bundles, log the bundle digest in every audit record, and expose the
  active digest through `zuno doctor`.
- Outcome: administrators change policy centrally and can prove which policy a
  past action ran under.

### Phase 4 — service deployment

- Harden the single binary as a long-running service: TLS termination, the
  Phase 2 authentication on the WebSocket listener, per-principal admission
  limits, `/readyz` semantics tied to policy and audit availability.
- Externalize the thread store behind the existing `ThreadStore` trait so
  several instances share history; isolate tenants at process level (one
  binary per tenant) before attempting in-process multi-tenancy.
- Outcome: an operator runs Zuno like any other stateless service, with state
  in a managed database and per-tenant isolation.

### Phase 5 — egress, secrets, and cost

- Document and test the host-level egress topology (proxy or network policy)
  that covers model, MCP, and browser traffic; inject the corporate CA.
- Integrate a vault for per-thread credentials handed to MCP servers and tools.
- Add organization budgets and chargeback on top of the token accounting the
  harness already emits.

## 6. Non-goals

- Zuno does not embed a business workflow, a default model route, or a default
  plugin set; those stay in user, project, or plugin roots.
- Zuno does not re-implement the agent loop, the sandbox, or the tool router;
  enterprise features wrap the harness.
- Zuno does not depend on ChatGPT workspace features (Compliance API,
  automations, cloud-managed configuration) for its enterprise story; where
  they exist they can be used, but the self-hosted path must work without them.

## 7. Sources

- OpenAI, "Unlocking the Codex harness: how we built the App Server", 2026-02-04:
  <https://openai.com/index/unlocking-the-codex-harness/>
- Codex App Server documentation: <https://developers.openai.com/codex/app-server>
- Codex sandboxing and approvals: <https://learn.chatgpt.com/docs/sandboxing.md>,
  <https://learn.chatgpt.com/docs/agent-approvals-security.md>
- Managed configuration: <https://learn.chatgpt.com/docs/enterprise/managed-configuration.md>
- Hooks, rules, plugins: <https://learn.chatgpt.com/docs/hooks.md>,
  <https://learn.chatgpt.com/docs/agent-configuration/rules.md>,
  <https://learn.chatgpt.com/docs/build-plugins.md>
- Agent Client Protocol: <https://agentclientprotocol.com/>
- Reference ACP adapter for Codex: <https://github.com/agentclientprotocol/codex-acp>
