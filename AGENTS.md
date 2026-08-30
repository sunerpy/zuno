# AGENTS.md

Zuno is an unreleased Rust agent harness. Prefer the correct foundation over compatibility layers, and update every internal caller in the same change. Zuno does not promise OpenCode configuration, database, plugin, hook, HTTP, tool-argument, or extension compatibility.

## Architecture

- Everything is a native component. Product behavior belongs in a `Component`, a typed service, or an `AgentDriver`, not in a growing central loop.
- A capability is complete only when its interface, provider, and consumer are all present. Keep those roles separate when they have different lifecycles.
- Registrations are effects. Mounting returns the disposer that removes exactly what was registered; profile replacement is transactional and rolls back in reverse order.
- Prefer composition through `ProfileBundle` and `HarnessProfile`. Deployment choices and tunables belong in validated profile or config fields.
- New behavior uses documented extension points. Changing the default agent loop requires updating `docs/harness-runtime.md`.
- Cross-component identifiers are typed where practical. Validate at config, model/tool JSON, durable storage, process, and wire boundaries; trust typed same-process values.

## Durable Agent State

- Model-visible means logged. Every prompt section, external input, tool result, retry notice, and subagent report that can change a model request must be reconstructable from durable session events.
- Prompt assembly uses stable section identifiers, an exact source, ordered content, and a content digest. Persist the actual post-hook prompt before the provider request.
- User prompts, steering, and subagent reports enter the durable FIFO inbox before execution. `reportDelivery: nextStep` must settle the child result, admit the parent input, and wake the parent without a polling race.
- Client surfaces consume durable events, inbox state, and projections. TUI, server, ACP, and future GUI clients must not acquire private agent-loop behavior.

## Goals And Recovery

- An active goal continues until it completes, is explicitly paused or blocked, reaches its budget, or encounters a typed permanent failure.
- Recoverable provider, network, stream, SQLite contention, turn-budget, and eligible tool failures persist an exponential-backoff retry before waiting. A process restart reconstructs the deadline from SQLite.
- Retry delays are positive, capped, jittered, and interruptible by user input. A valid peer `Retry-After` is clamped to the configured ceiling and is never replaced by an earlier local delay.
- Retry decisions use typed errors, never rendered messages. Authentication and user interruption pause; invalid protocol, corrupt durable state, and permanent configuration failures block.
- Tool execution is at-most-once by default. `ToolReplayPolicy::Never` is the default; only explicitly read-only or idempotent tools may declare `Safe`.
- A timeout or lost response around a side effect is an uncertain outcome. Persist it, require authoritative-state inspection, and never mechanically replay the call.

## Agents And Tools

- `build` owns end-to-end delivery; `plan` is read-only planning; `deep` owns difficult cross-cutting implementation without recursive delegation.
- Specialist agents have explicit positive responsibilities, negative delegation boundaries, permissions, and structured output expectations.
- `web_search` accepts a batch of queries and owns concurrency, cancellation, stable ordering, limits, and URL deduplication above single-query providers.
- A CLI command is not registered until a real handler exists. Help text, dispatch, assembled execution, and failure behavior must agree.
- HTTP routes and OpenAPI operations are registered only with real handlers. Do not publish placeholder endpoints that can only report an unavailable backend.
- Tool UI intent is part of the tool design. Keep tool arguments, result semantics, retry policy, and presentation independently testable.

## Research And Change Process

- Use CodeGraph first for repository navigation, call paths, and impact analysis. Start with `codegraph status . --json`.
- Before adopting DeepSeek Harness behavior, run the project skill `$zuno-dsh-sync`; compare the pinned reference with current upstream tags and record adopt, adapt, or reject.
- Treat DeepSeek Harness, Codex, oh-my-openagent, pi-agent, OpenCode, and Claw Code as design sources, not compatibility targets.
- Non-trivial runtime changes update the relevant architecture or design document in the same change.
- Tests describe Zuno behavior. Remove tests whose only purpose is cross-product parity.

## Checks

Run the smallest commands that cover the changed surface, then the shared gates before publishing:

```sh
cargo fmt --all --check
cargo test -p <changed-crate>
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Do not claim a workspace-wide gate passed unless its command reached successful completion.
