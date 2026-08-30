# DSH To Zuno Design Map

Use this map to find the native Zuno owner for a DSH change. It is a routing aid, not a compatibility matrix.

| Concern | DSH owner | Zuno owner or target | Porting rule |
| --- | --- | --- | --- |
| Composition | Cordis plugins, profiles, bundles, patch layers | `zuno-runtime`, `zuno-harness` | Preserve reversible effects, typed services, scoped overrides, and atomic profile replacement. |
| Agent execution | `core/agent`, `core/agent-loop`, agent events | `zuno-engine::AgentDriver`, engine loop | Put policy in a driver or component before changing the default loop. |
| Durable input | agent inbox plus session events | `zuno-db::event_log`, `inbox`, engine wake coordinator | Admit before execution and recover from the log after restart. |
| Goal continuation | goal service and round driver | `zuno-goal`, CLI continuation controller | Persist retry state; pause human-action failures and block permanent failures. |
| Tools | tool registry and guarded execution pipeline | `zuno-tool`, `zuno-tools`, profile tool contributions | A tool must be assembled, permission-checked, executable, observable, and documented. |
| Capability providers | Service Definition, Provider, Consumer packages | typed runtime service plus provider and consumer crates | Do not ship only one role of a capability. |
| Prompt assembly | prompt-section registry and tool schemas | turn resolver, instructions, skills, memory, tool schema assembly | Add stable section identity and provenance; model-visible bytes must be reconstructable. |
| Subagents and jobs | subagent providers, jobs, delivery tools | child-turn host, durable jobs, `task`, `job` | Keep job id separate from child session id and wake the parent through durable input. |
| Web | web service plus search/fetch providers and tools | `WebSearchProvider`, batch consumer, web fetch | Providers handle one request; the consumer owns concurrency, cancellation, limits, and rendering. |
| Permission and sandbox | permission presets, fs/shell/subprocess/sandbox seams | `zuno-permission`, auth, process and tool policy | Verify the real process and filesystem world, not only the selected policy value. |
| Session projection | append-only log, projection cache, client runtime | database events, server DTOs, TUI transcript | TUI and future GUI must derive from the same stable projection contract. |
| UI | browser client plugins and conversation nodes | `zuno-tui`, future presentation adapter | Keep domain state outside widgets and expose commands/events through a frontend-neutral interface. |

## Review Priorities

1. Lifecycle, durability, side-effect safety, permission enforcement, and process cleanup.
2. Extension points that prevent future central-loop edits.
3. Prompt provenance, agent/profile composition, subagent delivery, and complete command wiring.
4. TUI and future GUI projection quality.
5. Provider-specific features after the generic capability exists.
6. DSH build, website, translation, and TypeScript-only mechanics only when Zuno has the same problem.

## Evidence Standard

- Use exact commit ranges and final source, not release prose alone.
- Treat a reverted or superseded DSH change as `watch` or `reject` unless the final invariant still exists.
- Prove `already-covered` with a Zuno test that reaches the public surface.
- Give every `adapt-later` item a prerequisite and revisit trigger.
- Do not advance the baseline while any material file group is unclassified.
