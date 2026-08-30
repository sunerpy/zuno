# zuno models

`zuno models` reports the models reachable through the providers configured on this
machine. Use it to confirm the exact `provider/model` identifier that
[`zuno run`](/cli/run) and [`zuno tui`](/cli/tui) expect, and to check that a provider you
just authenticated actually exposes what you think it does.

Model catalogs are cached. Pass `--refresh` after adding a provider or when a newly
released model does not appear yet.

## Synopsis

```sh
zuno models [OPTIONS] [PROVIDER]
```

## Arguments

| Argument | Description |
| --- | --- |
| `[PROVIDER]` | |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--verbose` | | |
| `--refresh` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

List every model available across the configured providers.

```sh
zuno models
```

Narrow the listing to one provider.

```sh
zuno models openai
```

Show the detailed view when you need more than the model identifiers.

```sh
zuno models --verbose
```

Refetch catalogs instead of reading the cache, after authenticating a new provider.

```sh
zuno models --refresh
```

## See also

- [Global options](/cli/global-options)
- [zuno providers](/cli/providers)
- [zuno run](/cli/run)
- [Providers reference](/reference/providers)
- [Configuration reference](/reference/configuration)
