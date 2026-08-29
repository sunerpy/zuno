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
| `--fork` | Fork the target session, leaving the original transcript untouched |

```sh
zuno run --session ses_1a2b3c --fork --agent plan "what would a safe migration look like?"
```

Forking is the right choice when a script explores an alternative it does not want mixed
into the session a human is reading.

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

## Output format

```sh
zuno run --format default "summarize the diff"
zuno run --format json "summarize the diff"
```

`default` is human-readable text; `json` is for parsing. Use `json` in scripts rather than
scraping formatted output, because the formatted shape is presentation.

Logs never go to stdout. Mirror them to stderr when diagnosing:

```sh
zuno run --print-logs --log-level DEBUG "summarize the build failure"
```

## Attaching files

`-f`/`--file` is repeatable, and `--attach` carries an attachment. A supported image
becomes a typed image block under the 20 MiB image limit; any other reference must be UTF-8
text within 51,200 bytes and 2,000 lines, inserted with explicit begin and end markers.
Unsupported binary files, including PDFs, are not silently converted. See
[Images and file references](/reference/attachments).

## Confinement in a script

`--sandbox` selects confinement for the invocation:

```sh
zuno run --sandbox read-only --agent plan "audit the retry policy"
zuno run --sandbox workspace-write "fix the failing test and re-run it"
```

An agent contract may still narrow this. A read-only agent receives `read-only` even when
the invocation asked for something wider.

Verify deployability before depending on it in CI, and let the exit status gate the job:

```sh
zuno debug sandbox --mode workspace-write --network deny --check
```

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
      "shell": {
        "git push*": "deny",
        "cargo test*": "allow",
        "*": "deny"
      },
      "write": "deny"
    }
  }
}
```

Rules are evaluated in the order written, so a narrow pattern must precede a broader one.
`--auto` exists for interactive use and yields to the human broker in strict mode; it is
not a substitute for a policy. See [Permissions and sandboxing](/guide/permissions).

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

The server adds no authentication on your behalf. Binding to `0.0.0.0` or advertising over
mDNS exposes it beyond the local host, so restrict the bind address and CORS origins to
what the deployment needs. For ACP, stdout carries protocol framing, so send diagnostics to
stderr with `--print-logs`. See [Editors and ACP](/reference/zed-acp).

## See also

- [zuno run](/cli/run)
- [The terminal application](/guide/tui)
- [Permissions and sandboxing](/guide/permissions)
- [Operational logging](/logging)
