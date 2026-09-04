# zuno run

`zuno run` drives the harness without a terminal UI. It takes a message on the command
line or from a file, runs it to completion, and writes the result to stdout. This is the
form to use in scripts, CI jobs, and git hooks, where there is no interactive session to
attach to and the output needs to be machine-readable.

It can start a fresh session, continue the most recent one in the current directory, or
target an exact session id. A continued or targeted session resumes on the Agent, model,
and reasoning level it last ran with; `--agent`, `--model`, `--variant`, and `--thinking`
outrank those saved values for the run.

## Synopsis

```sh
zuno run [OPTIONS] [message]...
```

## Arguments

| Argument | Description |
| --- | --- |
| `[message]...` | The message to run. Omit it to read the message from stdin |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--command <COMMAND>` | Run this configured command, with the message as its arguments | |
| `-v`, `--version` | Show the Zuno package version | |
| `-c`, `--continue` | Continue the most recent session in this directory | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `-s`, `--session <SESSION>` | Run in this exact session | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-m`, `--model <MODEL>` | The model to use, as `provider/model` | |
| `--agent <AGENT>` | The agent to use | |
| `--format <FORMAT>` | Output format. Possible values: `default`, `json` | `default` |
| `--show-reasoning` | Write provider-supplied reasoning deltas to stderr between stable markers | |
| `-f`, `--file <FILE>` | Attach a text or image file to the message. Repeatable | |
| `--title <TITLE>` | The title a newly created session gets | |
| `--dir <DIR>` | Directory to run in. Defaults to the working directory | |
| `--variant <VARIANT>` | Reasoning variant to request, for a model that publishes named variants. Cannot be combined with `--thinking` | |
| `--thinking` | Request the model's own default thinking budget. Cannot be combined with `--variant` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

`--fork`, `--share`, `--attach`, `--port`, `--username`, `--password`, `--interactive`, and
`--auto` were accepted by earlier releases and rejected by every invocation that named
one, because no implementation stood behind them. They are no longer accepted at all, so a
script that passes one now fails while parsing instead of after the process starts.
`--auto` has a working home on the interactive surface:
`zuno tui --auto --prompt <message>`.

## Examples

Run a single message and print the default human-readable result.

```sh
zuno run "explain what changed in the last commit"
```

Continue the most recent session in this directory instead of starting a new one. The
turn runs on the Agent and model that session last used.

```sh
zuno run --continue "now add tests for the new branch"
```

Emit JSON so a script can parse the result rather than scraping formatted text.

```sh
zuno run --format json "list the failing tests" > result.json
```

Opt in to provider-visible reasoning progress without mixing it into the final
answer stream.

```sh
zuno run --show-reasoning "inspect the failure" > answer.txt 2> progress.txt
```

Only explicit provider reasoning deltas are shown. Signed thinking and encrypted
reasoning are never rendered. Each block is delimited by
`<<<zuno:reasoning>>>` and `<<<zuno:end-reasoning>>>`, including when the stream
ends with an error. `--show-reasoning` cannot be combined with `--format json`;
JSON mode keeps the existing structured event stream.

Target an exact session and choose a different agent for this turn. Naming another Agent
re-routes the model through configuration; add `--model` to keep the session's model.

```sh
zuno run --session ses_1a2b3c --agent plan "what would a safe migration look like?"
```

## See also

- [Global options](/cli/global-options)
- [zuno tui](/cli/tui)
- [zuno session](/cli/session)
- [zuno agent](/cli/agent)
- [Orchestration](/orchestration)
- [Harness runtime](/harness-runtime)
