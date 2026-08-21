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

## Durable goal recovery

An active goal uses two recovery layers. The provider request layer retries a bounded sequence in place and rolls back unpublished partial output before another request. If that sequence still ends in a recoverable error, the goal controller writes a `goal_retry` row before waiting and starts a fresh agent turn when its persisted deadline arrives. There is no cross-turn retry-count ceiling for recoverable failures: the delay grows exponentially, reaches the configured cap, and the goal remains active until it completes, is paused, reaches its token budget, or encounters a permanent failure.

The retry row is tied to the exact `goal_id` and stores the attempt, typed reason, selected delay, schedule time, and next eligible time. Reopening the same session reconstructs the wait from SQLite. Queued user input has priority over an automatic turn, and long waits are split by `poll_interval_ms` so an interactive surface can notice that input promptly.

Local delays use exponential backoff with symmetric jitter. A valid provider `Retry-After` value is used exactly and is never shortened by negative jitter. A provider delay beyond the local ceiling falls back to the capped local policy instead of stalling the harness for an unbounded interval.

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

Tool failures do not cause the harness to replay a call. The loop persists the failed tool result and gives it to the model in the next step, including timeouts that might have completed an external side effect before their response was lost. A later recovery turn receives a hidden, SQL-derived notice naming the retry attempt and requiring inspection of the worktree or external state before repeating an action with side effects.

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
