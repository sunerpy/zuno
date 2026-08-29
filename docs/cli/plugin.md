# zuno plugin

Extension packages contribute agents, workflows, skills, and executable tools. `zuno plugin`
installs them from a local directory, replaces an installed one transactionally, removes it,
and reports which packages are actually active for a given directory.

Scope matters here. Without `--project` a package installs globally; with it, the package
installs below the selected project's `.zuno` directory and applies only there. `list`
resolves the whole project configuration chain, so it is the way to check what a specific
checkout really loads.

## Synopsis

```sh
zuno plugin [OPTIONS] <COMMAND>
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Subcommands

| Subcommand | Description |
| --- | --- |
| [`list`](#zuno-plugin-list) | List packages active for one directory |
| [`add`](#zuno-plugin-add) | Install a new local package |
| [`update`](#zuno-plugin-update) | Transactionally replace an installed local package |
| [`remove`](#zuno-plugin-remove) | Remove an installed package |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno plugin list

List packages active for one directory.

```sh
zuno plugin list [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `--dir <DIR>` | Directory whose project configuration chain should be inspected | |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno plugin add

Install a new local package.

```sh
zuno plugin add [OPTIONS] <SOURCE>
```

| Argument | Description |
| --- | --- |
| `<SOURCE>` | Package directory or its extension.json manifest |

| Option | Description | Default |
| --- | --- | --- |
| `--project` | Install below the selected project's `.zuno` directory instead of globally | |
| `-v`, `--version` | Show the Zuno package version | |
| `--dir <DIR>` | Directory used to select the project target | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno plugin update

Transactionally replace an installed local package.

```sh
zuno plugin update [OPTIONS] <SOURCE>
```

| Argument | Description |
| --- | --- |
| `<SOURCE>` | Package directory or its extension.json manifest |

| Option | Description | Default |
| --- | --- | --- |
| `--project` | Install below the selected project's `.zuno` directory instead of globally | |
| `-v`, `--version` | Show the Zuno package version | |
| `--dir <DIR>` | Directory used to select the project target | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno plugin remove

Remove an installed package.

```sh
zuno plugin remove [OPTIONS] <ID>
```

| Argument | Description |
| --- | --- |
| `<ID>` | Stable package id |

| Option | Description | Default |
| --- | --- | --- |
| `--project` | Remove from the selected project's `.zuno` directory instead of globally | |
| `-v`, `--version` | Show the Zuno package version | |
| `--dir <DIR>` | Directory used to select the project target | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Report the packages active for the current directory.

```sh
zuno plugin list
```

Check what a different checkout resolves, without changing directories.

```sh
zuno plugin list --dir /srv/projects/api
```

Install a package globally from its manifest.

```sh
zuno plugin add ./my-extension/extension.json
```

Install the same package for one project only, then replace it in place after editing.

```sh
zuno plugin add --project --dir /srv/projects/api ./my-extension
zuno plugin update --project --dir /srv/projects/api ./my-extension
```

## See also

- [Global options](/cli/global-options)
- [zuno mcp](/cli/mcp)
- [zuno debug](/cli/debug)
- [Plugins](/plugins)
- [Configuration reference](/reference/configuration)
