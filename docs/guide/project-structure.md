# Project structure and execution flow

Use this page to find the crate that owns a behavior before changing it. Zuno is a
single Rust workspace with 48 crates. `crates/zuno-cli` builds the `zuno` binary;
the other crates keep protocol, runtime, storage, tool, and client concerns out of
that entry point.

The workspace rule is simple: a capability needs an interface, an implementation,
and a consumer. If those pieces have different lifecycles, they belong in separate
components or typed services rather than a larger central loop.

## Repository layout

| Path | What belongs there |
| --- | --- |
| `crates/` | All first-party Rust crates, including the CLI binary and test fixtures |
| `docs/` | Source Markdown for this site; FirLab publishes it after changes reach `main` |
| `schemas/` | Generated and checked public schemas, including `schemas/zuno.json` |
| `examples/` | Runnable configuration and integration examples |
| `packaging/` | Release packaging and platform metadata |
| `scripts/` | Installation, generation, release, and repository maintenance scripts |
| `benchmarks/` | Performance workloads and recorded measurement inputs |
| `wit/` | WIT contracts used by component extensions |
| `.github/` | CI, release, and documentation publication workflows |
| `Cargo.toml` | Workspace membership, pinned shared dependencies, and workspace lints |
| `crates.expected` | The reviewed first-party crate inventory |

Every crate opts into the workspace lint policy. First-party code forbids
`unsafe`; operating-system FFI stays behind audited dependencies.

## Crate map

### Product surfaces

| Crate | Responsibility |
| --- | --- |
| `zuno-cli` (`zuno`) | Parses commands, resolves configuration, assembles the default harness, and owns the shipped executable |
| `zuno-tui` | Terminal views, input handling, keybindings, themes, and rendering; it does not own an agent loop |
| `zuno-server` | HTTP routes, authentication, SSE delivery, PTY access, and server projections |
| `zuno-acp` | Agent Client Protocol adapter used by editor clients such as Zed |

### Runtime and orchestration

| Crate | Responsibility |
| --- | --- |
| `zuno-runtime` | Scoped component runtime, typed service publication, transactional profile replacement, and disposer tracking |
| `zuno-harness` | `HarnessProfile` and `ProfileBundle` composition for the shipped runtime |
| `zuno-engine` | Turn loop, prompt assembly, provider streaming, tool dispatch, compaction, retry, and cancellation |
| `zuno-agent` | Agent definitions, built-in presets, and the native child-task boundary |
| `zuno-orchestration` | First-party Agent, Skill, and orchestration descriptors compiled into Zuno |
| `zuno-goal` | Durable Goal state and continuation policy |
| `zuno-review` | Host-probed review evidence, validated Council reports, readiness gates, and durable review-event projections |
| `zuno-continuity` | Session History and Notes services and their tools |
| `zuno-product-agent` | Process adapters for Codex and Claude Code product agents |
| `zuno-extension` | Validation and lifecycle for static and process-local extension packages |

### Models, authentication, and external protocols

| Crate | Responsibility |
| --- | --- |
| `zuno-llm` | Provider-neutral request, streaming event, capability, and provider-registry interfaces |
| `zuno-provider-openai` | OpenAI Responses and Chat Completions protocols |
| `zuno-provider-anthropic` | Anthropic Messages, tool use, reasoning, and cache-control protocol |
| `zuno-provider-google` | Gemini, Vertex AI, and Vertex-hosted Anthropic transport |
| `zuno-provider-bedrock` | Amazon Bedrock Responses and Converse transports |
| `zuno-provider-compatible` | Configurable OpenAI-compatible endpoints |
| `zuno-auth` | API-key and OAuth credential storage and refresh |
| `zuno-aws-auth` | AWS credential-chain resolution and SigV4 signing |
| `zuno-network` | Shared outbound HTTP clients, proxy routing, and transport policy |
| `zuno-mcp` | MCP stdio and remote clients, including tools, resources, and prompts |
| `zuno-lsp` | Language-server process pool, requests, and diagnostics |

### Tools and operating-system effects

| Crate | Responsibility |
| --- | --- |
| `zuno-tool` | The native `Tool` trait, argument schemas, result types, and exposure metadata |
| `zuno-tools` | Built-in file, Shell, search, web, work-state, learning, and delegation tools |
| `zuno-permission` | Ordered tool-call rules and ask/allow/deny decisions |
| `zuno-sandbox` | Platform-neutral command preparation and available execution backends |
| `zuno-process` | Child-process tree containment and reaping |
| `zuno-pty` | Cross-platform pseudo-terminal sessions and their process ownership |
| `zuno-search` | Ignore-aware path and content search |
| `zuno-watch` | Coalesced filesystem change events |
| `zuno-attachment` | Image validation, storage references, and provider-ready attachments |
| `zuno-snapshot` | Workspace snapshots, diffs, and `/undo`/`/redo` provenance |

### Durable state and learning

| Crate | Responsibility |
| --- | --- |
| `zuno-db` | SQLite schemas, migrations, sessions, events, inboxes, jobs, and durable projections |
| `zuno-memory` | Capped resident Memory files, candidate validation, reviewed apply, and recovery |
| `zuno-learning` | Experience extraction and retrieval, pattern mining, feedback, and Skill candidates |
| `zuno-eval` | Offline cassette evaluation for reviewed Skill candidates |
| `zuno-atomic-file` | Visibility-atomic file replacement used by Memory and other projections |
| `zuno-types` | Shared wire and domain types for sessions, messages, parts, and tool payloads |

### Configuration and support

| Crate | Responsibility |
| --- | --- |
| `zuno-config` | Configuration discovery, merge order, schema types, and variable substitution |
| `zuno-catalog` | Agents, Skills, commands, references, and filesystem/config discovery |
| `zuno-paths` | Data, cache, project, and per-worktree path resolution |
| `zuno-error` | Typed error and recovery taxonomy shared across crate boundaries |
| `zuno-observability` | Bounded structured logs, debug sinks, and span conventions |
| `zuno-testkit` | Shared fixtures, provider cassettes, and integration-test helpers |
| `zuno-reaping-fixture` | Process-tree fixture used by native containment tests |

## How one turn moves through the system

The TUI, `zuno run`, ACP, and HTTP server have different transports but converge
before model execution. None of those clients owns a private turn loop.

1. **A surface admits input.** The CLI selects a project, configuration, Agent,
   model, and sandbox policy. TUI, ACP, and HTTP inputs enter the same host
   services. A model-visible input is committed to the durable session inbox
   before the host tries to execute it.
2. **The harness mounts a profile.** `zuno-harness` composes bundles containing
   typed components. `zuno-runtime` prepares the complete candidate without
   effects, starts effects only after validation, and publishes the resulting
   services atomically.
3. **A `TurnHost` selects an `AgentDriver`.** The default driver enters
   `zuno-engine`; benchmark, evaluation, workflow, or remote profiles may provide
   another driver without changing the default loop.
4. **Prompt assembly records its inputs.** Agent instructions, runtime policy,
   selected Skills, history, resident Memory, retrieved Experience, attachments,
   and tool schemas become stable prompt sections. The exact post-hook prompt and
   its digests are persisted before provider I/O.
5. **The provider streams typed events.** `zuno-llm` supplies the neutral
   interface. One native provider crate or `zuno-provider-compatible` owns the
   wire protocol, while `zuno-network` owns outbound routing and deadlines.
6. **Tool calls cross two separate gates.** The registry exposes only tools
   allowed by the profile and Agent. `zuno-permission` then decides whether a
   concrete call is allowed, denied, or requires a human answer. Shell execution
   separately passes command-risk checks and the selected sandbox backend.
7. **Results become durable input.** Tool results, provider chunks, retry notices,
   questions, permission replies, child reports, and terminal outcomes are stored
   as session events. Side-effecting tools default to at-most-once execution;
   uncertain outcomes require inspection instead of replay.
8. **Clients consume projections.** TUI, ACP, and HTTP read the same event,
   work-state, learning, permission, and question stores. Live channels wake a
   client, but SQLite remains the authority after reconnect or restart.

```text
CLI / TUI / ACP / HTTP
          |
          v
  profile + typed components
          |
          v
 TurnHost -> AgentDriver -> prompt receipt -> provider
          |                                      |
          +-> tool registry -> permission -> effect
          |                                      |
          +---------- durable events/inbox <-----+
                             |
                             v
                    client projections
```

## Component lifecycle

A component's `prepare` method stages services, requirements, and deferred effects
without making them visible. Once every component has prepared, effects start and
return disposers. Replacement withdraws the old profile, stops it in reverse order,
and publishes the new service set only after startup succeeds.

A failed or timed-out disposer is not treated as a clean stop. The runtime records
a typed `Failed` or `Uncertain` lifecycle result and refuses to mount another
composition that could overlap the unresolved resource. See
[Harness Runtime](/harness-runtime) for the complete lifecycle contract.

## Find the owner of a change

| Change | Start here | Documentation owner |
| --- | --- | --- |
| CLI parsing or help | `crates/zuno-cli/src/cmd/` | [`docs/cli/`](/cli/) |
| TUI behavior | `crates/zuno-tui/` plus its host command in `zuno-cli` | [Terminal application](/guide/tui) |
| HTTP route or wire behavior | `crates/zuno-server/src/api/` and `events/` | [HTTP API and OpenAPI](/reference/http-api) |
| ACP behavior | `crates/zuno-acp/` | [Zed and ACP](/reference/zed-acp) |
| Default turn behavior | `crates/zuno-engine/` and the profile provider | [Harness Runtime](/harness-runtime) |
| Agent roster or delegation | `crates/zuno-agent/`, `zuno-orchestration` | [Agents](/guide/agents), [Orchestration](/orchestration) |
| Tool schema or execution | `crates/zuno-tool/`, `zuno-tools` | [Tools](/guide/tools) |
| Permission or Shell authority | `zuno-permission`, `zuno-sandbox`, `zuno-process` | [Permissions and sandboxing](/guide/permissions) |
| Provider protocol | `zuno-llm` and one `zuno-provider-*` crate | [Providers and credentials](/reference/providers) |
| Config field or merge behavior | `zuno-config`, `schemas/zuno.json` | [Configuration reference](/reference/configuration) |
| Durable schema or migration | `zuno-db` | [Database lifecycle](/migration) |
| Memory or learning | `zuno-memory`, `zuno-learning`, `zuno-eval` | [Memory and learning](/guide/memory-learning) |
| Extension lifecycle | `zuno-extension`, `zuno-runtime` | [Developing agents and extensions](/guide/extension-development) |

Changing a default Agent loop also requires an update to
[Harness Runtime](/harness-runtime). Adding or renaming a public page requires an
entry link here in the Zuno repository and a matching FirLab sidebar entry.

## Verification boundaries

Run the narrow test for the owning crate first. Before publishing a repository-wide
change, the shared gates are:

```sh
cargo fmt --all --check
cargo test -p <changed-crate>
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Platform-sensitive process, PTY, sandbox, packaging, and filesystem behavior also
needs evidence from the affected native OS and architecture. A cross-compile does
not prove runtime behavior.

## See also

- [What is Zuno?](/guide/what-is-zuno)
- [Harness Runtime](/harness-runtime)
- [Agent and extension development](/guide/extension-development)
- [Documentation architecture and coverage](/design/documentation-coverage)
