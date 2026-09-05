# AGENTS.md

Zuno is a released, fast-moving Rust agent harness. Prefer the correct foundation over compatibility layers, and update every internal caller in the same change. Zuno does not promise compatibility with OpenCode configuration, databases, plugins, hooks, HTTP APIs, tool arguments, or extensions; published Zuno database formats follow the migration rules below.

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
- A durable Plan is reconciled as work changes, not only when a turn ends. Focused temporary work uses a persisted child Plan and restores its parent exactly once; a new objective replaces the visible root instead of appending duplicate generic steps.
- A database format shipped in a release is durable user state. Every supported older format advances through a guarded, atomic forward migration; never require users to rebuild a supported database.
- Run schema creation, backfills, index creation, and the format-marker update in one transaction, with the marker updated last. A marker-only edit is corruption, not a migration.
- Migration tests use exact old-format fixtures and verify preserved rows, not only table presence. At minimum compare representative session, message, and durable-memory values before and after, then verify the new tables, indexes, and marker.
- Future, unmarked, or structurally corrupt formats fail closed without mutation. Keep recovery evidence and the original database available rather than guessing, downgrading, or deleting user data.

## Goals And Recovery

- An active goal continues until it completes, is explicitly paused or blocked, reaches its budget, or encounters a typed permanent failure.
- Recoverable provider, network, stream, SQLite contention, Agent step-limit, and eligible tool failures persist an exponential-backoff retry before waiting. A process restart reconstructs the deadline from SQLite. A turn that spends its token, tool-call, or wall-clock allowance pauses with `turn_budget` and is never retried mechanically; a Goal whose own token budget ran out ends as `budget_limited`.
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

## Cross-Platform Development

- Keep Linux x86_64/aarch64, macOS x86_64/aarch64, and Windows x86_64/aarch64 MSVC behavior explicit. Do not encode POSIX paths, shell quoting, executable suffixes, architecture aliases, or case sensitivity into platform-neutral components.
- `rg` 14 or newer is a backend dependency of the `glob` and `grep` tools only. Missing ripgrep may make those tools unavailable or fail their calls; it must not prevent Zuno startup, configuration, provider access, database access, or other core runtime behavior.
- `bwrap` 0.8.0 or newer is only the Linux confinement backend for `read-only` and `workspace-write`. `danger-full-access` uses the native backend on every platform. Trusted `run-unconfined` fallback is limited to eligible unavailable-backend failures for write-capable `workspace-write`; `read-only` never falls back. A trusted layer may instead select the native backend explicitly (`sandbox.backend: native`): every Agent's Shell, read-only contracts included, then runs natively with the configured permission mode kept, the requested contract recorded as unenforced, and no fallback involved; a project layer cannot select it, and a read-only contract under it is a tool-allowlist, permission-rule and risk-gate boundary, not an OS boundary.
- macOS and Windows currently have no confined backend. Keep native execution usable through the explicit full-access path, the explicit trusted native backend, or the eligible trusted fallback, and keep documentation, diagnostics and durable records clear that none of these is confinement.
- A platform-sensitive change is not complete after one host build. Run the available cross-target compile/link checks, then obtain native evidence for behavior tied to MSVC, ConPTY, Windows Job Objects, macOS process behavior, Linux namespaces, packaging, or architecture-specific artifacts.
- Release support requires an executable smoke result for that exact OS/architecture artifact. Cross-compilation is useful evidence but does not replace native execution where runtime semantics differ.

## Research And Change Process

- Use CodeGraph first for repository navigation, call paths, and impact analysis. Start with `codegraph status . --json`.
- When the index is usable, continue with CodeGraph search, node, and impact queries before opening source. Use `rg` or `sed` only for exact literal verification, non-indexed configuration or documentation, or a file known to be newer than the index; do not make shell text search the primary navigation path.
- Before adopting DeepSeek Harness behavior, run the project skill `$zuno-dsh-sync`; compare the pinned reference with current upstream tags and record adopt, adapt, or reject.
- Treat DeepSeek Harness, Codex, oh-my-openagent, pi-agent, OpenCode, and Claw Code as design sources, not compatibility targets.
- Non-trivial runtime changes update the relevant architecture or design document in the same change.
- Every code change includes a documentation-impact review. A user-visible behavior, configuration, CLI, protocol, persistence, permission, platform, deployment, or operational change updates the relevant English and Chinese guides or references in the same change; generated CLI/schema material and documentation contract tests must be refreshed when applicable. A genuinely internal-only change records why no user documentation changed instead of silently skipping the review.
- Adding, renaming, or removing a documentation page also updates repository entry links and the FirLab site navigation or sync contract so the published page is discoverable. A documentation-bearing delivery is not complete until the `Publish docs` workflow succeeds and the corresponding live route is verified after the source change reaches `main`.
- Tests describe Zuno behavior. Remove tests whose only purpose is cross-product parity.
- Rapid-development releases are patch-only until this rule is removed. `feat` and `fix`
  may change changelog grouping but must not change the major or minor component; do not
  use `!`, `BREAKING CHANGE`, `Release-As`, or another override that would produce anything
  except the next `x.y.(z+1)` version. The release controller must fail closed on any other
  candidate.

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
