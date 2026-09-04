# zuno tui

`zuno tui` starts the interactive terminal application, where you can steer a session
while it runs, answer permission prompts, and read the durable transcript as it is
written. It is also what `zuno` does when you give it no subcommand, so the two forms are
interchangeable.

Use its options to preselect a model or agent, resume prior work, or submit an opening
prompt so the session starts working immediately after launch.

## Synopsis

```sh
zuno tui [OPTIONS]
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--prompt <PROMPT>` | Submit this prompt on start, as though it had been typed and sent | |
| `-v`, `--version` | Show the Zuno package version | |
| `-m`, `--model <MODEL>` | The model to use, as `provider/model` | |
| `--agent <AGENT>` | The agent to use | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `-c`, `--continue` | Continue the most recent session in this directory | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `-s`, `--session <SESSION>` | Talk in this exact session | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `--auto` | Approve every permission that is not explicitly denied, without asking. Upstream's own description ends in "(dangerous!)" and it means it: this replaces the human at the permission prompt, so a tool call the default ruleset would have stopped to ask about proceeds unattended | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Start the terminal application in the current directory.

```sh
zuno tui
```

Resume the most recent session in this checkout rather than opening a new one.

```sh
zuno tui --continue
```

Launch with a specific model and an opening prompt already submitted.

```sh
zuno tui --model openai/gpt-5 --prompt "review the diff on this branch"
```

Reopen an exact session by id, with Shell confined to reads for this invocation.

```sh
zuno tui --session ses_1a2b3c --sandbox read-only
```

## See also

- [Global options](/cli/global-options)
- [zuno run](/cli/run)
- [zuno session](/cli/session)
- [zuno agent](/cli/agent)
- [Configuration reference](/reference/configuration)
- [Harness runtime](/harness-runtime)
