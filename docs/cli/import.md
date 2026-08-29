# zuno import

`zuno import` restores a `.zuno-bundle` produced by [`zuno export`](/cli/export) into this
machine's user environment. It is the receiving half of environment portability: the same
configuration, Skills, extensions, and Agents land on a new machine without hand-copying
directories.

Import writes into real user-owned roots. Run `--dry-run` first to see what a bundle would
change. `--replace` transactionally replaces non-empty target roots, which discards what is
currently in them, so confirm the dry run before using it.

## Synopsis

```sh
zuno import [OPTIONS] <bundle>
```

## Arguments

| Argument | Description |
| --- | --- |
| `<bundle>` | Path to a `.zuno-bundle` produced by `zuno export` |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--replace` | Transactionally replace non-empty target roots | |
| `-v`, `--version` | Show the Zuno package version | |
| `--dry-run` | Validate and report the import without changing files | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Validate a bundle and report what it would write, without changing any files.

```sh
zuno import --dry-run ~/backups/zuno-environment.zuno-bundle
```

Import the bundle into empty target roots.

```sh
zuno import ~/backups/zuno-environment.zuno-bundle
```

Replace non-empty target roots transactionally, after the dry run confirmed the change.

```sh
zuno import --replace ~/backups/zuno-environment.zuno-bundle
```

Trace the import on stderr when it fails and the reason is not obvious from the summary.

```sh
zuno import --dry-run --print-logs --log-level DEBUG ~/backups/zuno-environment.zuno-bundle
```

## See also

- [Global options](/cli/global-options)
- [zuno export](/cli/export)
- [Portable bundles](/reference/portable-bundles)
- [Configuration reference](/reference/configuration)
- [Migration](/migration)
