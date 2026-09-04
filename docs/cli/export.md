# zuno export

`zuno export` collects the user-owned side of a Zuno installation into a single
`.zuno-bundle` file: configuration, Skills, extensions, Agents, and other user assets.
Use it to move an environment to another machine, snapshot it before a risky change, or
hand a reproducible setup to someone else.

Credentials are excluded unless you ask for them. `--include-credentials` writes provider
and MCP credential stores into a bundle that is not encrypted, so a bundle produced that
way must be handled as a secret: anyone who can read the file can use those credentials.

## Synopsis

```sh
zuno export [OPTIONS] [bundle]
```

## Arguments

| Argument | Description |
| --- | --- |
| `[bundle]` | Bundle path; defaults to `zuno-export-<UTC timestamp>.zuno-bundle` |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--include-credentials` | Include provider and MCP credential stores in the unencrypted bundle | |
| `-v`, `--version` | Show the Zuno package version | |
| `--force` | Replace an existing output file | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Write a bundle to the default timestamped filename in the current directory.

```sh
zuno export
```

Write to an explicit path so a backup job can find it.

```sh
zuno export ~/backups/zuno-environment.zuno-bundle
```

Overwrite a bundle that already exists at that path.

```sh
zuno export --force ~/backups/zuno-environment.zuno-bundle
```

Include credential stores, accepting that the resulting file is unencrypted and must be
protected like a secret.

```sh
zuno export --include-credentials ~/private/zuno-with-credentials.zuno-bundle
```

## See also

- [Global options](/cli/global-options)
- [zuno import](/cli/import)
- [Portable bundles](/reference/portable-bundles)
- [Configuration reference](/reference/configuration)
- [Providers reference](/reference/providers)
