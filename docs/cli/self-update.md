# zuno self-update

`zuno self-update` replaces the running executable with a build from a GitHub release,
verifying the download against its published checksum before installing it. It is the
supported path when Zuno was installed as a standalone binary rather than through a package
manager.

The command overwrites the executable in place. Use `--check` to learn whether a newer
release exists without changing anything, and `--yes` only when a script must proceed without
the interactive confirmation. `--tag` pins an exact release, and `--force` reinstalls a
release that is not newer, which is how you go back to a known-good version.

## Synopsis

```sh
zuno self-update [OPTIONS]
```

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--check` | Report whether a newer release exists without changing the executable | |
| `-v`, `--version` | Show the Zuno package version | |
| `--force` | Reinstall the selected release even when it is not newer | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--tag <vX.Y.Z>` | Install one explicit release tag instead of the latest release | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `-y`, `--yes` | Replace the executable without an interactive confirmation | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Report whether a newer release exists, leaving the executable untouched.

```sh
zuno self-update --check
```

Update to the latest release, confirming interactively.

```sh
zuno self-update
```

Update without a prompt, for an unattended maintenance job.

```sh
zuno self-update --yes
```

Reinstall an exact earlier tag to roll back a bad update.

```sh
zuno self-update --tag v0.0.1 --force --yes
```

## See also

- [Global options](/cli/global-options)
- [Excluded commands](/cli/excluded)
- [Self-update reference](/reference/self-update)
- [Migration](/migration)
