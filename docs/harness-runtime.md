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

## Durable inputs

Every model-visible external input is admitted to the session event log and durable inbox in one SQLite transaction before execution is attempted. The inbox is the source of truth across active turns, idle sessions, process restarts, and competing drivers.

Drivers promote inputs in FIFO order. Promotion is transactional and can target one input identifier for a live soft interrupt. A malformed input records a session error and does not strand later queue entries.

User prompts and subagent reports share this protocol:

- An active parent receives a soft interrupt and promotes the report at the next tool-safe point.
- If the report misses the final safe point, the wake coordinator waits for the active lease to end and starts another turn while the input is still pending.
- An idle parent is claimed and driven immediately.
- A restarted process recovers pending reports from the durable inbox.

## Background subagents

`task` creates a distinct `job_*` identifier for each background run while retaining a separate child session identifier for conversational continuation.

`reportDelivery` supports:

- `nextStep` (default): settle the job and admit the report to the parent inbox atomically, then wake the parent.
- `quiet`: settle the job without admitting a parent input.

The `job` tool reads durable status for jobs owned by the current parent session. It reports `running`, `completed`, `failed`, or `cancelled` together with the child session, delivery policy, result, and error.

## Concurrent web search

`web_search` accepts only `queries: string[]`. The consumer deduplicates queries by first occurrence, runs the remaining requests concurrently through a single-query `WebSearchProvider`, and combines cancellation with the turn interrupt.

The first failed query cancels its siblings and waits for every request to settle before returning. Successful output is deterministic regardless of completion order: query content follows input order, sources are merged by rank round-robin, duplicate URLs are removed, and profile-owned query, result, and timeout limits are applied.

Provider adapters normalize transport output into `SearchResult` and `SearchSource`; they do not own batch scheduling or model-facing presentation.

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
