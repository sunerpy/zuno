# zuno agent

Agents carry their own prompt, model, permissions, and delegation boundaries. `zuno agent`
is the surface for authoring one and for checking what the current configuration chain
actually resolves to, which matters when global and project definitions overlap.

`create` writes a new agent definition; `list` reports the agents visible from here.

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
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Subcommands

| Subcommand | Description |
| --- | --- |
| [`create`](#zuno-agent-create) | |
| [`list`](#zuno-agent-list) | |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno agent create

```sh
zuno agent create [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `--path <PATH>` | | |
| `-v`, `--version` | Show the Zuno package version | |
| `--description <DESCRIPTION>` | | |
| `--mode <MODE>` | Possible values: `all`, `primary`, `subagent` | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--permissions <PERMISSIONS>` | Alias: `--tools` | |
| `-m`, `--model <MODEL>` | | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

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
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

List the agents resolved from the current configuration chain.

```sh
zuno agent list
```

Create a subagent definition at an explicit path with a description and a pinned model.

```sh
zuno agent create --path .zuno/agent/reviewer.md --mode subagent --description "Review diffs for regressions" --model openai/gpt-5
```

Inspect one agent's fully resolved definition after creating it.

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
