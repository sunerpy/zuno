# zuno agent

Agents carry their own prompt, model, permissions, and delegation boundaries. `zuno agent`
is the surface for checking what the current configuration chain actually resolves to,
which matters when global and project definitions overlap.

`list` reports the agents visible from here, each with the permission rules this build
enforces for it. Authoring an agent is writing a Markdown file under `.zuno/agent/`; see
[custom agents](/config/custom-agents) for the front matter that file takes.

## Synopsis

```sh
zuno agent [OPTIONS] <COMMAND>
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Subcommands

| Subcommand | Description |
| --- | --- |
| [`list`](#zuno-agent-list) | List the agents the current configuration chain resolves |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno agent list

```sh
zuno agent list [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

List the agents resolved from the current configuration chain.

```sh
zuno agent list
```

Inspect one agent's fully resolved definition, after writing
`.zuno/agent/reviewer.md`.

```sh
zuno debug agent reviewer
```

## See also

- [Global options](/cli/global-options)
- [zuno run](/cli/run)
- [zuno tui](/cli/tui)
- [zuno debug](/cli/debug)
- [Configuration reference](/reference/configuration)
- [Orchestration](/orchestration)
