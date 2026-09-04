# Headless runs

`zuno run` drives the harness without a terminal interface. It takes a message on the
command line or from a file, runs it to completion, and writes the result to stdout. This
is the form for scripts, CI jobs, and git hooks.

```sh
zuno run "explain what changed in the last commit"
zuno run --format json "list the failing tests" > result.json
zuno run --continue "now add tests for the new branch"
```

The durable model is unchanged. A headless run persists the same events, prompt receipts,
plan and todo state, and job records as an interactive session, so a headless session can
be resumed in the terminal application and the reverse.

## Selecting the session

| Option | Effect |
| --- | --- |
| none | Start a new session |
| `-c`, `--continue` | Continue the most recent session in this directory |
| `-s`, `--session <SESSION>` | Talk in this exact session |

```sh
zuno run --session ses_1a2b3c --agent plan "what would a safe migration look like?"
```

With `--continue` or `--session`, the run resumes on the Agent, model, and reasoning
level the session last ran with unless one of the flags below names another; the
precedence table is in [Sessions and turns](/guide/sessions#continuing-a-session).

Forking a session is not part of this binary. A script that explores an alternative it
does not want mixed into the session a human is reading starts a fresh one instead: omit
both `--continue` and `--session`, and pass `--title` so the run is findable afterwards.
See [zuno run](/cli/run) for the options earlier releases accepted and rejected.

## Selecting the agent, model, and effort

| Option | Effect |
| --- | --- |
| `--agent <AGENT>` | Run as this agent, with its contract |
| `-m`, `--model <MODEL>` | Use this `provider/model` |
| `--variant <VARIANT>` | Override configured reasoning with an exact model-declared variant |
| `--thinking` | Ask for `high` when available, otherwise the strongest declared non-`off` level |

`--thinking` and `--variant` are mutually exclusive. Canonical variant names
`off`, `low`, `medium`, `high`, `xhigh`, and `max` are accepted only when the selected model
declares them, or when the model exposes generic reasoning without a named catalog. A
non-canonical name copies that variant's complete provider option object. Unknown names
fail before HTTP I/O and list what is available.

Prefer `--variant max` or `--variant xhigh` when exact effort matters; `--thinking` is
deliberately an automatic convenience.

On a resumed session each of these flags outranks the value saved on the session, and a
flag left unset falls back to the saved value before configuration. `--agent` naming a
different Agent than the saved one re-routes the model through configuration; add
`--model` to keep the session's model. A saved Agent or model that no longer exists is
reported as a status note (`status_detail` in `--format json`) and the run continues on
the next fallback instead of failing.

## Output format

```sh
zuno run --format default "summarize the diff"
zuno run --format json "summarize the diff"
```

`default` is human-readable text; `json` is for parsing. Use `json` in scripts rather than
scraping formatted output, because the formatted shape is presentation.

The JSON stream includes a `notice` event when the turn did not proceed exactly as
configured: `{"type":"notice","severity":"warning","code":"budget.token_budget","detail":"…"}`.
`severity` is `info`, `warning`, or `error`; `code` is stable and comes from the
`instruction.*` family (a remote rule file that could not be fetched, so its rules are not
in force for this turn while the turn still runs) or the `budget.*` family (a turn stopped
by its allowance, or a compaction the budget policy requested). An unreadable or
over-budget local rule file fails the turn with an error before any provider request and
produces no `notice` event. The same event is published on the server event stream as
`notice`.

Provider-visible reasoning is opt-in:

```sh
zuno run --show-reasoning "summarize the failure" > answer.txt 2> reasoning.txt
```

Final answer text stays on stdout. Stderr receives only explicit provider
reasoning deltas between `<<<zuno:reasoning>>>` and
`<<<zuno:end-reasoning>>>`; signed thinking and encrypted reasoning are never
shown. Zuno delays the opening marker until the first delta if the provider
omits a start event and always closes an open block on provider error or stream
end. `--show-reasoning --format json` is rejected because JSON mode already
emits structured events.

Logs never go to stdout. Mirror them to stderr when diagnosing:

```sh
zuno run --print-logs --log-level DEBUG "summarize the build failure"
```

## Attaching files

`-f`/`--file` is repeatable, and it is the only option that carries a file. A supported
image is normalized and admitted to the database-scoped durable object store
before the inbox write; the default source limit is 20 MiB and the normalized
encoded limit is 5 MiB. Any other reference must be UTF-8 text within 51,200
bytes and 2,000 lines, inserted with explicit begin and end markers. Unsupported
binary files, including PDFs, are not silently converted. See
[Images and file references](/reference/attachments).

## Confinement in a script

`--sandbox` selects confinement for the invocation:

```sh
zuno run --sandbox read-only --agent plan "audit the retry policy"
zuno run --sandbox workspace-write "fix the failing test and re-run it"
zuno run --sandbox danger-full-access "run in a deliberately unconfined container"
zuno run --sandbox workspace-write \
  --sandbox-on-unavailable run-unconfined \
  "prefer confinement, but allow eligible unavailable fallback"
zuno run --agent plan --sandbox-backend native \
  "run natively on a host without an OS sandbox, permission mode kept"
```

An agent contract may still narrow this. A read-only agent receives `read-only` even when
the invocation asked for something wider, and read-only Agents never use unavailable
fallback. `danger-full-access` always selects the native backend and makes the effective
permission mode `allow_all`. `run-unconfined` preserves the configured permission mode and
hard denials, but requested filesystem and network restrictions are not OS-enforced during
fallback. `--sandbox-backend native` (or `ZUNO_SANDBOX_BACKEND=native`, or
`sandbox.backend: native` in a trusted layer) selects the native backend for every Agent
of the invocation, read-only ones included, with the permission mode kept; headless runs
never prompt, so on macOS and Windows this flag, the variable, or a trusted layer is how a
read-only Agent gets Shell at all.

Verify deployability before depending on it in CI, and let the exit status gate the job:

```sh
zuno debug sandbox --mode workspace-write --network deny --check
```

`--check` still fails when requested confinement is unavailable, even if
`--sandbox-on-unavailable run-unconfined` would let runtime execution proceed.

## Permission modes without a human

This is the part that decides whether a headless run works at all.

| Mode | Headless behaviour |
| --- | --- |
| `standard` | Configured rules and the normal risk gates apply. A rule that says `ask` has no one to ask |
| `strict` | Fails closed. Every side-effecting call needs a fresh human decision, and there is no attached user |
| `allow_all` | No prompts. Explicit denies, catastrophic shell denials, sandbox authority, and argument validation still apply |

For unattended automation, configure the rules you actually want rather than relying on
prompts that cannot be answered:

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "read": "allow",
      "glob": "allow",
      "grep": "allow",
      "edit": "deny",
      "shell": {
        "*": "deny",
        "cargo test*": "allow",
        "git push*": "deny"
      }
    }
  }
}
```

The last matching rule wins, so the catch-all `*` comes first and the narrow patterns that
carve exceptions out of it come last. Written the other way round, the trailing `"*": "deny"`
would override `cargo test*` and the suite this configuration exists to run would never
start. The `edit` key covers the `write`, `edit`, and `apply_patch` tools; there is no
separate `write` rule key. `--auto` exists for interactive use and yields to the human
broker in strict mode; it is not a substitute for a policy. See
[Permissions and sandboxing](/guide/permissions).

## Example: a CI gate

```sh
#!/bin/sh
set -eu

zuno debug sandbox --mode workspace-write --check

result=$(
  zuno run --format json --agent plan \
    --sandbox read-only \
    "Review the staged diff for regressions. Report blocking findings only."
)

printf '%s\n' "$result" > review.json
```

Reading a plan-agent review in CI is safe by construction: no write tool is registered, and
the contract pins confinement to `read-only`.

## The server and editor surfaces

Two other non-interactive entry points exist. `zuno serve` starts the HTTP server for
external clients; `zuno acp` speaks Agent Client Protocol over stdin and stdout for
editors.

```sh
zuno serve --port 4096
zuno acp --check
```

The server supports Basic Auth through `ZUNO_SERVER_PASSWORD` and the explicit
loopback-only `--browser-auth` bootstrap. Browser auth prints one launch URI,
consumes its token once, and issues an authority-bound signed cookie. It never
makes a non-loopback listener acceptable. For ACP, stdout carries protocol
framing, so send diagnostics to stderr with `--print-logs`. See
[Editors and ACP](/reference/zed-acp).

Three details of the HTTP surface matter to a script that drives sessions.
`POST /api/session/{id}/interrupt` cancels only a live turn; on an idle session it
answers `204` and does nothing, so it never leaves the next ordinary turn — including
an explicit resume of input that was deliberately queued — starting already
interrupted. `GET /api/session/{id}/event` answers `404` with the error code
`not_found` for a session the database has never seen, rather than opening a stream
that could never produce an event. And a prompt or steering input admitted while a
compaction holds the session lease waits in the durable inbox and is driven as soon
as the compaction releases the lease, instead of sitting there until some unrelated
event happens to wake the session.

### Reading files over the API

`GET /api/fs/read` buffers the whole file, so it refuses anything larger than 32 MiB
with `413` and the error code `file_too_large` instead of growing the server by the
size of whatever path a caller names. `GET /api/fs/find` answers with a `truncated`
field: `true` means the walk stopped early — the 20,000-entry budget, the 16-level
depth limit, or a subtree this process could not open — so `truncated: true` with no
match means *not searched*, never *not present*. The published `/openapi.json`
document still declares the find response body as a schema gap, so a generated client
has to read this field from this page.

The filesystem, session-maintenance, catalogue, and prompt-admission endpoints run
their synchronous work off the request reactor inside a fixed process-wide budget:
four concurrent `/api/fs` operations, two `/api/session/prune` previews or mutations,
eight catalogue discovery walks, and two inline image decodes for
`POST /api/session/{sessionID}/prompt`. The fourth budget is charged once for every
prompt whose `prompt.files[]` is non-empty — inline images and references to
already-admitted attachments alike — and covers each decode in that
prompt, so at most two decodes run at once across the whole process; a
prompt without files never waits for a slot. A request over any budget waits for a
slot rather than being refused, and a caller that disconnects while waiting never
starts the work at all. The budgets are cost bounds rather than tuning knobs — no
request field, header, or configuration key raises one. Resolving a durable image
object for a provider request is bounded the same way in every host, not only the
server: at most two resolutions run at once across the whole process, sized against
the 900,000,000-byte working set a stored object may cost to re-encode, and a turn
whose history needs a third waits for a slot.

### Replying to a permission request or a question

A reply is one durable transition. `204` means the request row, the reply event, and —
for a request recovered after a restart — the durable inbox entry all committed. `404`
with `not_found` means nothing was written, because the request was already settled,
belongs to another session, or has been claimed by another reply that is still
committing. Exactly one reply to a request can receive `204`.

A reply that has committed is final: if the HTTP connection drops before the `204`
arrives, the tool call still receives the decision, a standing `always` is still
installed, and the paused goal still resumes, because nothing after the commit runs on
the connection that carried the reply. A client that retries such a reply gets `404`
because the request is no longer pending, not because the reply was lost. Only a `500`
with `mutation_failed` leaves the request pending and the reply worth sending again.

A `204` reply whose original asker is gone — the turn was interrupted, or the process
that made the call restarted — is still recorded, and the answer reaches the session
through the durable inbox. Because nothing consumed it, `reply: "always"` in that case
saves no standing grant: an authorization is only installed when the call it authorizes
actually received the reply.

A request that no reply settles ends in a terminal state of its own rather than a
synthesized denial. `expired` means the observer grace window closed with no client
watching (30 seconds after the last observer disconnects), `cancelled` means the
session or turn withdrew the request, and `failed` means the server could not record a
decision. Every call a standing `always` pre-approves is recorded as its own
already-settled request row (`source: "standing"`); the grant itself is never written,
and archiving or deleting a session withdraws the grants that session made.

## See also

- [zuno run](/cli/run)
- [The terminal application](/guide/tui)
- [Permissions and sandboxing](/guide/permissions)
- [Operational logging](/logging)
