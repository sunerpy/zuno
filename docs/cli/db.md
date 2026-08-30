# zuno db

Sessions, prompts, tool results, and inbox state live in a local SQLite database. `zuno db`
runs a query against it directly, which is the escape hatch when the shaped output of
[`zuno session`](/cli/session) does not answer the question you have.

It takes a single optional `QUERY` argument and has no subcommands. Output is TSV by
default, or JSON when you need to parse it.

Queries run against the real durable store. Prefer read-only statements; a statement that
writes will change harness state that the runtime treats as authoritative.

## Synopsis

```sh
zuno db [OPTIONS] [QUERY]
```

## Arguments

| Argument | Description |
| --- | --- |
| `[QUERY]` | |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--format <FORMAT>` | Possible values: `json`, `tsv` | `tsv` |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

List the tables in the session database as TSV.

```sh
zuno db "select name from sqlite_master where type = 'table' order by name"
```

Count stored sessions and read the result as JSON.

```sh
zuno db --format json "select count(*) as sessions from session"
```

Read the most recent sessions ordered by last activity, straight from the store.

```sh
zuno db "select id, title from session order by time_updated desc limit 5"
```

## See also

- [Global options](/cli/global-options)
- [zuno session](/cli/session)
- [Excluded commands](/cli/excluded)
- [Session retention](/session-retention)
- [Harness runtime](/harness-runtime)
