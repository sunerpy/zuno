# Harness Runtime

Zuno assembles an agent from a native harness profile. A profile is a set of bundles, and each bundle contributes typed components to one scoped runtime.

## Runtime model

- `Component` is the lifecycle unit. Mounting a component returns its typed service contributions and an asynchronous disposer.
- `ProfileBundle` groups components that are installed and replaced together.
- `HarnessProfile` is the complete composition selected for a session.
- `HarnessRuntime` owns `Profile`, `Session`, `Agent`, and `Turn` scopes. A child scope inherits services and may override them locally.
- `AgentDriver` owns the turn-driving policy. The default driver wraps the standard agent loop; benchmark, workflow, remote, and evaluation harnesses can install another driver without modifying that loop.
- `ToolManifest` is the profile's model-visible tool surface. The registry filters all built-ins, including automatically assembled file tools, through this manifest.
- `ToolContributions` carries native `Tool` implementations owned by the profile. Contributions are assembled after built-ins and before MCP tools, pass through the same visibility rules, and may intentionally replace a built-in by wire id.

Profile activation is transactional. Candidate components mount against a staging service view, duplicate component identifiers fail before mount, and no candidate service is visible outside the transaction until every component succeeds. Failure disposes candidates in reverse order. Successful replacement publishes the complete new profile atomically, then disposes the previous profile in reverse order.

## Declarative extension packages

Zuno also exposes one validated declarative package protocol for agents, slash-command workflows,
and skills. It adapts DSH's lifecycle outcome without loading the Cordis/JavaScript ABI:

- `extension_define` records an immutable package in the current process and worktree scope.
- `extension_run` validates the complete active package set and activates it transactionally.
- `extension_stop` removes its contributions while retaining the definition.
- `extension_undefine` removes the definition.
- `extension_inspect` projects static and process-local package state.

The TUI detects an active-composition generation change after the tool turn, tears down the complete
session composition, and resolves it again inside the same process before the next turn. This
refreshes the agent catalog, command registry, skill catalog, prompt provenance, permissions, and
tool definitions together. An inactive definition does not trigger a rebuild.

Process-local definitions are held only by `StartupEnvironment`'s shared `ExtensionRegistry`; a new
process starts with an empty registry. Static packages live at
`.zuno/extensions/<id>/extension.json` or
`~/.config/zuno/extensions/<id>/extension.json`, are loaded at composition startup, and require the
directory name to match the package id. Dynamic and static packages use the same
`zuno.extension/v1` schema and contribution merger. Duplicate package ids or duplicate
agent/workflow/skill names across active extension packages fail instead of silently choosing a
winner. An agent contribution cannot rename its map identity or mark itself disabled.

Declarative packages do not evaluate JavaScript, load a foreign plugin ABI, or load Rust dynamic
libraries. Executable tools, providers, drivers, approvals, and other typed services remain compiled
Rust `Component` implementations mounted through a `HarnessProfile`.

## Native agents

The built-in catalog separates primary modes, delegable specialists, and hidden engine agents:

| agent | role |
| --- | --- |
| `build` | End-to-end implementation owner and the only agent that may delegate. |
| `plan` | Read-only repository research and implementation-ready planning. |
| `deep` | Difficult cross-cutting implementation without recursive delegation. |
| `explorer` | Read-only repository structure, definition, caller, and impact discovery. |
| `librarian` | Current external documentation, release, and upstream research. |
| `advisor` | Architecture review, failure-mode analysis, and explicit trade-off advice. |
| `worker` | Bounded, well-specified implementation and verification. |
| `looker` | Visual artifact inspection when a vision-capable model is available. |

`compaction`, `title`, and `summary` are hidden engine agents. A user-defined agent may be declared under `agent.<name>` or as Markdown under `.zuno/agent/**/*.md`; it enters the same resolution, permission, prompt, and provenance pipeline as a native agent.

## Prompt provenance

Prompt assembly is ordered data, not string concatenation spread across the CLI. Every section has a stable identifier, source, exact content, and SHA-256 digest. The composition root currently orders:

1. native or configured agent base prompt;
2. generated agent policy;
3. global and project memory;
4. extension lifecycle guidance and the exact active package projection;
5. discovered instruction files;
6. the skill trigger policy;
7. the available skill catalog.

The trigger policy makes a named or clearly matching skill a pre-action requirement: the model must load the complete body through the `skill` tool, use only the minimal matching set, and may not claim a skill was used unless that call completed successfully. The catalog remains discovery metadata rather than a substitute for the body.

Before the provider request, the loop persists `session.prompt.assembled`. The event records the ordered sections and the actual post-hook system prompt, so a model request can be reconstructed even when a hook transformed the assembled text. Identical prompt content is logged once per turn.

## Durable inputs

Every model-visible external input is admitted to the session event log and durable inbox in one SQLite transaction before execution is attempted. The inbox is the source of truth across active turns, idle sessions, process restarts, and competing drivers.

Drivers promote inputs in FIFO order. Promotion is transactional and can target one input identifier for a live soft interrupt. A malformed input records a session error and does not strand later queue entries.

User prompts and subagent reports share this protocol:

- An active parent receives a soft interrupt and promotes the report at the next tool-safe point.
- If the report misses the final safe point, the wake coordinator waits for the active lease to end and starts another turn while the input is still pending.
- An idle parent is claimed and driven immediately.
- A restarted process recovers pending reports from the durable inbox.

## Durable goal recovery

An active goal uses two recovery layers. The provider request layer retries a bounded sequence in place and rolls back unpublished partial output before another request. If that sequence still ends in a recoverable error, the goal controller writes a `goal_retry` row before waiting and starts a fresh agent turn when its persisted deadline arrives. There is no cross-turn retry-count ceiling for recoverable failures: the delay grows exponentially, reaches the configured cap, and the goal remains active until it completes, is paused, reaches its token budget, or encounters a permanent failure.

The retry row is tied to the exact `goal_id` and stores the attempt, typed reason, selected delay, schedule time, and next eligible time. Reopening the same session reconstructs the wait from SQLite. Queued user input has priority over an automatic turn, and long waits are split by `poll_interval_ms` so an interactive surface can notice that input promptly.

Local delays use exponential backoff with symmetric jitter and never collapse to zero. A valid provider `Retry-After` value is never shortened by jitter; it is clamped to the configured ceiling rather than replaced by an earlier local delay.

```json
{
  "goal": {
    "retry": {
      "initial_delay_ms": 2000,
      "max_delay_ms": 300000,
      "jitter_percent": 20,
      "poll_interval_ms": 250
    }
  }
}
```

Recovery is selected from typed errors, never rendered messages:

- Transport failures, rate limits, incomplete streams, SQLite writer contention, empty assistant messages, and per-turn step limits schedule another goal turn.
- Context-limit failures compact retained history before retrying. Successful compaction is persisted as its own retry phase so a restart does not compact the same history twice.
- Authentication failures, user interruption, and a closed event consumer pause the goal for human action.
- Invalid provider protocol, unavailable agent/model configuration, corrupt durable state, and other permanent failures block the goal.

Tool execution is at-most-once by default. `ToolReplayPolicy::Never` is inherited by every tool unless the implementation explicitly declares `Safe`; current safe tools are read-only or idempotent inspection operations such as file reads, glob, grep, skill lookup, session search, job status, LSP inspection, goal status, and web search/fetch.

The loop never mechanically replays a call. It persists the failed tool result and gives it to the model in the next step, including timeouts that might have completed an external side effect before their response was lost. A later recovery turn receives a hidden, SQL-derived notice naming the retry attempt. A `Safe` failure may be attempted again after backoff; a `Never` failure requires authoritative inspection of the worktree or external state before the model decides whether another mutation is appropriate.

## Background subagents and product agents

`task` creates a distinct `job_*` identifier for each background run while retaining a separate child session identifier for conversational continuation.

Enabled `productAgent` instances register independent static tools backed by a host-installed Codex or Claude Code process. A product invocation has a one-shot `run_*` id and, in background mode, a separate `job_*` id. It does not create a Zuno child session and cannot be resumed as one.

`reportDelivery` supports:

- `nextStep` (default): settle the job and admit the report to the parent inbox atomically, then wake the parent.
- `quiet`: settle the job without admitting a parent input.

The `job` tool reads durable status for jobs owned by the current parent session. `JobSubject` distinguishes `ChildSession` from `ProductAgent`; status is `running`, `completed`, `failed`, `cancelled`, or `uncertain`, together with delivery policy, result, error, and subject identity.

`job_cancel` verifies parent ownership and requests cancellation from the live supervisor. It never pre-settles a job and has `ToolReplayPolicy::Never`; the executor records `cancelled` only after the child session or complete product process tree has stopped. Product protocol or process loss after work may have begun records `uncertain`. A restart reconciles still-running product jobs to `uncertain` and never replays them.

Codex and Claude Code retain ownership of their native login, configuration, and model choice. Zuno inherits the session directory and proxy environment but never copies product tokens into `AuthStore`. See [Codex and Claude Code product agents](design/product-agents.md).

## Concurrent web search

`web_search` accepts only `queries: string[]`. The consumer deduplicates queries by first occurrence, runs the remaining requests concurrently through a single-query `WebSearchProvider`, and combines cancellation with the turn interrupt.

The first failed query cancels its siblings and waits for every request to settle before returning. Successful output is deterministic regardless of completion order: query content follows input order, sources are merged by rank round-robin, duplicate URLs are removed, and profile-owned query, result, and timeout limits are applied.

Provider adapters normalize transport output into `SearchResult` and `SearchSource`; they do not own batch scheduling or model-facing presentation.

## Network egress

`zuno-network` owns the outbound HTTP construction policy shared by providers,
authentication, catalogs, remote instructions, remote MCP, and web tools.
Session traffic uses `ProxyPolicy::Environment`, which resolves the standard
HTTP, HTTPS, all-proxy, and no-proxy environment variables when a connection
pool is constructed. A capability that constructs reqwest directly bypasses
this product contract and is incomplete.

`ProxyPolicy::Direct` is an explicit security boundary, not a fallback. It is
used only for local control-plane probes and cloud metadata endpoints. Bedrock
therefore has two transports with separate lifecycles: runtime and SSO traffic
is proxy-aware, while IMDS and approved local ECS credential endpoints are
direct. Remote HTTPS container credential endpoints remain on the proxy-aware
transport.

Child processes inherit the process proxy environment unless their typed
configuration deliberately overrides a variable. The agent loop does not
rewrite process globals per session; deployment-specific proxy choices belong
in the environment that launches the Zuno process.

## Building a harness

Use `zuno_harness::profile_with_tools` to combine an `AgentDriver`, `ToolManifest`, and native `ToolContributions`, then add more `ProfileBundle` values for typed capability providers:

```rust
let profile = zuno_harness::profile_with_tools(
    "review",
    Arc::new(ReviewDriver::new()),
    ToolManifest::new([BuiltinSlot::Read, BuiltinSlot::Grep, BuiltinSlot::Task])?,
    ToolContributions::new([Arc::new(ReviewSummaryTool::new())])?,
);

runtime.activate_profile(profile).await?;
```

Registrations are effects: every component must return the exact cleanup needed to undo its contribution. Deployment choices belong in profile configuration rather than hardcoded branches in the agent loop.

## Client surfaces

The TUI, headless CLI, server, ACP adapter, and future GUI consume the same commands, durable events, inbox, and projections. Cursor replay closes gaps after disconnects; live delivery is only a wake/latency path. See [client interface architecture](design/client-interfaces.md).

The design sources and explicit adopt/adapt/reject decisions are recorded in [the harness comparison](design/harness-comparison.md).
