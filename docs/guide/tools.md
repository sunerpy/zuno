# Tools

A tool is what lets a model do something instead of describing it. In Zuno a tool carries
three independent declarations: what side effects it has, whether it may be re-executed,
and whether it may overlap with other calls. Keeping those separate is what makes safe
parallelism possible without also making retries dangerous.

## The default surface

The default model-visible surface is deliberately small:

| Tool | Purpose | Effect |
| --- | --- | --- |
| `read` | Read a file or directory | Read-only |
| `glob` | Find files by pattern | Read-only |
| `grep` | Search file contents | Read-only |
| `write` | Create a file, or intentionally replace one whole | Side-effecting |
| `apply_patch` | Localized, context-verified source edits | Side-effecting |
| `shell` | Run a command under the active sandbox | Side-effecting |
| `bg` | Inspect or cancel background commands | Read-only inspection; `cancel` is side-effecting |
| `task` | Delegate a bounded objective to another agent | Delegating |
| `job` | Inspect background job state | Read-only |
| `webfetch` | Retrieve one URL | Read-only |
| `web_search` | Batch web search | Read-only |
| `skill` | Discover and load reusable instructions | Read-only |
| `question` | Ask a structured clarification during Plan | User-mediated |

Durable work state adds `plan_get`, `plan_update`, `todo_get`, `todo_update`, and
`goal_get`/`goal_update`. `memory_propose` appears when memory is enabled, and
`council_run` when the active agent can reach it.

Optional continuity tools appear only when enabled by `continuity` and retained
by the final tool and permission filters:

| Tool | Actions | Scope |
| --- | --- | --- |
| `history` | `list_windows`, `list_items`, `read_item`, `search_contents` | Normalized evidence from the current session |
| `notes` | `list_files_by_prefix`, `read_file`, `search_contents`, `append_to_file`, `write_file` | Logical documents for the current session and Agent |

History windows are delimited only by successful compactions. Returned content
excludes reasoning, encrypted values, synthetic internal prompt text, and binary
attachment bytes, and must be treated as data rather than instructions.

Notes never expose a host path. A scope may contain at most 100 documents,
256 KiB per document, and 1 MiB in total. Both write actions require the exact
`expected_revision`; use `0` only to create a document. The trusted tool
`call_id`, request digest, and revision make a repeated delivery idempotent while
rejecting stale concurrent writes.

The host's durable Plan is independent from model tool visibility. Disabling
`plan_update` hides model mutation but does not disable host Plan creation or
restart recovery.

`edit`, `execute`, and `lsp` exist as registered slots but are not part of the default
surface. `edit` remains available to explicitly constructed profiles; the default editing
path is `apply_patch` plus `write`.

`glob` and `grep` drive the official `rg` executable, with Zuno contributing only typed
arguments, cancellation, bounded decoding, and stable ordering. Ripgrep 14 or newer must
be available; a missing one is a startup error for the tool runtime rather than a silent
fallback to a slower walker.

For a Shell command that only observes remote work, set
`backgroundPurpose: "remoteObserver"`. This value is persisted with the background
execution and projected through `bg` and the durable completion input. It does not
change permissions or make the remote system part of the local process result; it
requires the resumed Agent to refresh authoritative remote state before claiming that a
CI run, deployment, or release completed.

## Effect classification

Every invocation classifies as one of four effects, and the default is the strict one:

| Effect | Meaning | Strict-mode approval |
| --- | --- | --- |
| `ReadOnly` | Observes state without changing it | Not required |
| `UserMediated` | Requires human input by design | Not an approval surface |
| `Delegating` | Runs child work that carries its own effects | Each child call re-evaluated |
| `SideEffecting` | Default. Changes state or reaches outside | Fresh approval required |

Because `SideEffecting` is the default, an unknown harness or MCP tool fails closed. A
mixed tool may classify from validated arguments: `bg list`, `bg output`, and `bg wait`
are read-only while `bg cancel` is side-effecting.

Native reads, `glob`, `grep`, skill and session and job inspection, read-only LSP, MCP
resource reads, `webfetch`, and `web_search` do not receive the extra strict prompt.
Shell, file writes, durable state changes, delegation, product agents, extension
lifecycle mutations, and unknown MCP tools do.

Mixed tools are resolved from their validated action. Every `history` action and
the three Notes read actions are `ReadOnly`; Notes append and replacement are
`SideEffecting`. An absent or unknown Notes action fails closed as
`SideEffecting`.

## Replay policy

Tool execution is at-most-once by default. `Never` is inherited unless an implementation
explicitly declares `Safe`, and the current safe set is read-only or idempotent
inspection: file reads, glob, grep, skill lookup, current-session history, Notes reads,
job status, LSP inspection, goal status, and web search or fetch. Notes writes remain
`Never`.

| Policy | Behaviour after a failure |
| --- | --- |
| `Never` | Persist the result, hand it to the model next step, require authoritative-state inspection before any new mutation |
| `Safe` | May be attempted again after backoff |

The loop never mechanically replays a call. A timeout or lost response around a side
effect is an uncertain outcome, including the case where the external effect may have
completed before the response was lost. A later recovery turn receives a notice naming the
retry attempt, derived from SQL rather than from prose in the transcript.

This is the reason a shell command that timed out is not simply run again.

## Concurrency

Overlap is a separate declaration from replay safety.

| Policy | Behaviour |
| --- | --- |
| `Exclusive` | Default. A two-sided barrier: earlier overlapping calls settle first, and no later call starts until it settles |
| `ParallelSafe` | May overlap with other non-exclusive calls, under the configured bound |
| `IsolatedBackground` | May overlap while running outside the foreground path |

The dispatcher resolves tools, validates arguments, runs hooks, and asks permissions in
the model's order. It then executes consecutive non-exclusive calls under the bound and
persists results in the original call order regardless of physical completion order.
Shell, writes, unknown extension tools, and MCP tools without an explicit declaration
remain exclusive.

```json
{
  "concurrency": {
    "tool_calls": 8,
    "delegations": 8,
    "mcp_connections": 8,
    "lsp_requests": 4
  }
}
```

Every field accepts `1..=64`. Setting one to `1` restores serial behaviour for that layer.

## Enabling and disabling tools

The top-level `tools` map is a per-tool switch keyed by tool name:

```json
{
  "tools": {
    "webfetch": false
  }
}
```

That is availability, not authorization. Authorization is `permission.rules`, and an agent
may also declare an exact `tools` allowlist that replaces rather than extends the default
surface. See [Permissions and sandboxing](/guide/permissions) and
[Custom agents](/config/custom-agents).

## Refusal is an outcome, not a failure

Malformed arguments, an unavailable tool, and a permission denial all emit a dispatch-
blocked event with `invalid_arguments`, `unavailable`, or `denied` before the model-visible
error result is appended. Durable state keeps `outcome: "blocked"` and the block kind, so a
client can say the requested effect never ran rather than implying it failed part-way.

Process, transport, and implementation failures remain error outcomes. The distinction is
worth knowing when reading a transcript: blocked means nothing happened.

## Output limits

```json
{
  "tool_output": {
    "max_bytes": 51200,
    "max_lines": 2000
  }
}
```

Those are the defaults. Output beyond them is truncated rather than allowed to consume the
model window.

## See also

- [Permissions and sandboxing](/guide/permissions)
- [Agents](/guide/agents)
- [MCP servers](/guide/mcp)
- [Harness runtime](/harness-runtime)
