# zuno run

`zuno run` drives the harness without a terminal UI. It takes a message on the command
line or from a file, runs it to completion, and writes the result to stdout. This is the
form to use in scripts, CI jobs, and git hooks, where there is no interactive session to
attach to and the output needs to be machine-readable.

It can start a fresh session, continue the most recent one in the current directory,
target an exact session id, or fork an existing session so the original transcript stays
untouched.

## Synopsis

```sh
zuno run [OPTIONS] [message]...
```

## Arguments

| Argument | Description |
| --- | --- |
| `[message]...` | |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--command <COMMAND>` | | |
| `-v`, `--version` | Show the Zuno package version | |
| `-c`, `--continue` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `-s`, `--session <SESSION>` | | |
| `--fork` | | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--share` | | |
| `-m`, `--model <MODEL>` | | |
| `--agent <AGENT>` | | |
| `--format <FORMAT>` | Possible values: `default`, `json` | `default` |
| `--show-reasoning` | Write provider-supplied reasoning deltas to stderr between stable markers | |
| `-f`, `--file <FILE>` | | |
| `--title <TITLE>` | | |
| `--attach <ATTACH>` | | |
| `-p`, `--password <PASSWORD>` | | |
| `-u`, `--username <USERNAME>` | | |
| `--dir <DIR>` | | |
| `--port <PORT>` | | |
| `--variant <VARIANT>` | | |
| `--thinking` | | |
| `-i`, `--interactive` | | |
| `--auto` | | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Run a single message and print the default human-readable result.

```sh
zuno run "explain what changed in the last commit"
```

Continue the most recent session in this directory instead of starting a new one.

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

Fork an existing session with a specific agent, leaving the original transcript intact.

```sh
zuno run --session ses_1a2b3c --fork --agent plan "what would a safe migration look like?"
```

## See also

- [Global options](/cli/global-options)
- [zuno tui](/cli/tui)
- [zuno session](/cli/session)
- [zuno agent](/cli/agent)
- [Orchestration](/orchestration)
- [Harness runtime](/harness-runtime)
