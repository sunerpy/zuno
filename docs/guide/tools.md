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

When connected MCP tools survive capability and permission filtering, Zuno keeps their
implementations executable but normally withholds their full JSON schemas. A conditional
`tool_search` tool searches compact metadata; each match expands the provider-visible
tool set on the next model step. This avoids paying the prompt cost of every connected
service on every request. See [MCP servers](/guide/mcp).

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

The host classifier decides whether a request requires a durable strategic Plan,
but it does not generate visible generic steps. The model uses
`plan_update action=create` for the first Plan or a genuinely new objective;
`patch` changes only named step ids, `append` adds host-identified steps, `push`
opens a focused child, and `pop` restores the exact parent without retransmitting
the Plan. Every existing-Plan mutation requires the current
`expected_revision`. `completed` and `superseded` are terminal.

Before successful delivery, a durable reconciliation driver checks Plan, Todo,
Job, Goal, tool-result, and verification state. Ordinary sessions holding
unreconciled durable work receive at most two reconciliation continuations, then
enter typed `PlanUnreconciled` human wait instead of claiming completion. Only
durably recorded work counts: a session that recorded no Plan, Todo, or Job
finishes on its first answer even when the host classifier expected a Plan. Disabling
`plan_update` prevents model creation or mutation; an existing Plan is still
persisted, projected, and restored.

See [History and Notes continuity](/config/continuity) for complete enable/disable,
profile-overlay, permission, revision, and restart guidance.

`edit`, `execute`, and `lsp` exist as registered slots but are not part of the default
surface. `edit` remains available to explicitly constructed profiles; the default editing
path is `apply_patch` plus `write`.

## `apply_patch` conflict recovery

`apply_patch` uses a stable SHA-256 read receipt plus a read generation instead
of trusting remembered prose. An existing source used by `update`, `delete`, or
`move` must first be read; an already-existing move destination must also be
read. The tool acquires one mutation lock, validates every affected file, and
only then writes, so any preflight conflict leaves all files unchanged.

Mutation conflicts are typed and model-correctable, but never automatically
replayed:

| Conflict | Meaning | Required recovery |
| --- | --- | --- |
| `ReadRequired` | The current file has no valid read receipt | Read the named resource before constructing a mutation |
| `StaleRead` | The file changed after the recorded read | Read the current file and rebuild the operation from that content |
| `ContextMismatch` | A hunk no longer matches the current logical lines | Read the named hunk area and generate a smaller patch with fresh, unique context |
| `IdenticalReplay` | The same operation digest was submitted against the same file content | Do not resend it; revise the operation, or wait for a real file change and read again |

The conflict includes the resource, operation digest, current content digest,
hunk number/title when applicable, and a concrete `requiredAction`. Matching is
performed on logical lines while preserving the original BOM, LF or CRLF style,
and final-newline state. Patch grammar errors remain `InvalidArgs`. If a later
I/O or formatter failure occurs after a write, the result is `Uncertain`, names
the paths observed as changed, and requires inspection rather than mechanical
replay.

`webfetch` accepts only credential-free HTTP(S) targets. Zuno resolves and
validates every address, rejects a whole mixed public/private DNS answer, pins
the validated addresses, and repeats validation for each of at most five
redirects. It honors the process `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and
`NO_PROXY` policy while sending the validated target IP through HTTP, HTTPS,
SOCKS4, or SOCKS5 proxy routes and preserving the original Host/TLS SNI. A
configured proxy failure never falls back to direct. The default timeout is 30
seconds and the per-call maximum is 120 seconds; timeout errors report route,
phase, and elapsed duration.

`web_search` keeps provider credentials and full wire URLs out of errors and
logs. Diagnostics identify only provider, scheme, host, path, status, and error
category; they do not contain an API key, authorization header, or full query.

`glob` and `grep` drive the official `rg` executable, with Zuno contributing only typed
arguments, cancellation, bounded decoding, and stable ordering. Ripgrep 14 or newer must
be available for those two tools. Discovery is lazy and scoped to them: a missing `rg`
makes `glob` and `grep` report a typed tool error, and never a silent fallback to a
slower walker, but it does not block Zuno from starting, reading configuration, reaching
a provider, or opening its database.

For a Shell command that only observes remote work, set
`backgroundPurpose: "remoteObserver"`. This value is persisted with the background
execution and projected through `bg` and the durable completion input. It does not
change permissions or make the remote system part of the local process result; it
requires the resumed Agent to refresh authoritative remote state before claiming that a
CI run, deployment, or release completed.

## What a shell exit status proves

A pipeline's exit code is the last stage's exit code. `cargo test | tail -5` exits
zero when the tests fail, because only `tail` decided it. Zuno therefore puts
`set -o pipefail` in effect for every `shell` call by default, so a failure at any
stage is the command's failure.

`exitPolicy` selects that behaviour explicitly:

| Value | Effect | Exit status authority |
| --- | --- | --- |
| `pipefail` (default) | A failing stage of a pipeline fails the command | Authoritative on `bash`, `ksh`, and `zsh` |
| `all` | Also stops at the first failing command in a sequence | Authoritative |
| `last` | Only the final stage decides, the POSIX default | Derived |

Choose `last` deliberately, for a command meant to tolerate a failing stage.

Every result carries a verification receipt saying what ran, what it decided, the
exit code, and how far that code reaches. The distinction is the point: a `derived`
status is recorded but is not evidence, because the code came from a stage that
never saw the failure. The interpreter matters too. `set -o pipefail` is not in
POSIX, and `dash` exits without running the command at all when it is asked for it,
so an interpreter outside the known-capable set reports `derived` and names itself
in the receipt. `sh` is treated as unknown on purpose, since the name does not say
what is behind it. PowerShell has no equivalent, so only `all` is authoritative
there.

The configuration is only the starting point. Zuno also reads the command's own
text, because text that runs inside an authoritative shell can take the guarantee
apart: `cargo test || true` exits zero whatever the tests did, `set +e` turns the
prologue's option back off, `bash -c '…'` runs under its own options, a check
written as an `if` or `while` condition exits zero when that check fails, a loop
reports only its last iteration, and under `all` a `&&` chain before the last
statement is exempt from `set -e`. In PowerShell, one native command piped into
another leaves `$LASTEXITCODE` holding the second one's code alone.

None of this changes what runs. The command executes exactly as written, and a
`||` fallback is meant to win. What changes is the claim: it drops to `derived`
with a limitation naming the construct and the remedy. The reading is a syntax
tree rather than a search for characters, so `echo 'run tests || true'` masks
nothing and keeps the configuration's verdict.

A stored receipt is addressed by an id that appears in the tool result as
`[verification rcp_…]`. That id, not a recollection of the transcript, is what
satisfies a Goal completion criterion that requires evidence.

## What a shell command inherits

A `shell` call runs with the host environment the Zuno process itself has, minus
Zuno's own secrets. Three variables are removed before the command is assembled:

| Variable | What it holds |
| --- | --- |
| `ZUNO_AUTH_CONTENT` | Injected provider credentials, replacing the credential store |
| `ZUNO_SERVER_PASSWORD` | The HTTP server's Basic authentication password |
| `ZUNO_SERVER_USERNAME` | The account name that password belongs to |

Names are compared case-insensitively, because Windows environment variable names
are. Removal happens before any host-supplied environment hook, so the host stays
the single place that decides what a model-composed command may read, and nothing
in the shipped configuration puts these three back.

Everything else is inherited on purpose. A wildcard filter over `*_API_KEY` and
`*_TOKEN` was considered and rejected: it silently breaks `gh`, `aws`, `az`, and
`gcloud`, along with every user who exports a token because a command needs it. A
tool that quietly removes the credential a command requires fails worse than one
that keeps it, because the removal surfaces later as an unexplained authentication
error somewhere else.

One consequence is worth stating plainly. A nested `zuno` launched from inside a
`shell` call no longer inherits `ZUNO_AUTH_CONTENT`, so it resolves credentials the
ordinary way and needs its own configuration or credential store. The interactive
terminal is unaffected, because its shell is driven by you rather than composed by
a model.

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

Native reads, `glob`, `grep`, skill and session and job inspection, `tool_search`,
read-only LSP, MCP resource reads, `webfetch`, and `web_search` do not receive the extra strict prompt.
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
job status, LSP inspection, goal status, connected-tool metadata search, and web search
or fetch. Notes writes remain `Never`.

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

Those are the defaults. Output beyond either limit is withheld rather than allowed to
consume the model window. Nothing is truncated and nothing is lost: the full output is
saved to a file, and the model receives one refusal naming the measured size, the limit it
crossed, and that file's path. A call can ask for the whole thing by repeating itself with
`accept_large_output: true`.

## See also

- [Permissions and sandboxing](/guide/permissions)
- [History and Notes continuity](/config/continuity)
- [Agents](/guide/agents)
- [MCP servers](/guide/mcp)
- [Harness runtime](/harness-runtime)
