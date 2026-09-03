# zuno session

Sessions are durable. Every prompt, tool result, and report that could change a model
request is written to the local store, which means the store grows and eventually needs
inspection and cleanup. `zuno session` is that surface: it lists what exists, prunes by
age, and deletes an exact session.

By default listing is scoped to the current checkout and shows only root sessions.
Child sessions created by delegation are hidden until you ask for them.

## Synopsis

```sh
zuno session [OPTIONS] <COMMAND>
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
| [`list`](#zuno-session-list) | |
| [`prune`](#zuno-session-prune) | |
| [`delete`](#zuno-session-delete) | |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno session list

```sh
zuno session list [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `--all-projects` | List sessions from every project, not just this checkout | |
| `-v`, `--version` | Show the Zuno package version | |
| `--project <PATH\|ID>` | List one project, named by its id or its worktree path | |
| `--archived` | Include archived sessions alongside the live ones | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--roots` | Only root sessions. This is the default; pass `--no-roots` for children | |
| `--no-roots` | Include child sessions, which are hidden by default | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sort <SORT>` | Order by last activity or by creation time. Possible values: `updated` (`time_updated`, last activity; upstream's `listGlobal` order), `created` (`time_created`) | `updated` |
| `-n`, `--limit <LIMIT>` | Limit to N sessions, most recent first. Defaults to 100. Alias: `--max-count` | |
| `--format <FORMAT>` | Output format. Possible values: `table`, `json` | `table` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno session prune

`--older-than <DAYS>` is required.

```sh
zuno session prune [OPTIONS] --older-than <DAYS>
```

| Option | Description | Default |
| --- | --- | --- |
| `--older-than <DAYS>` | | |
| `-v`, `--version` | Show the Zuno package version | |
| `--all-projects` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--project <PATH\|ID>` | | |
| `--by <BY>` | Possible values: `updated`, `created` | `updated` |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--archive` | | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--delete` | | |
| `--include-shared` | | |
| `--include-recent` | | |
| `--force` | | |
| `--yes` | | |
| `--format <FORMAT>` | Possible values: `table`, `json` | `table` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno session delete

```sh
zuno session delete [OPTIONS] <SESSION_ID>
```

| Argument | Description |
| --- | --- |
| `<SESSION_ID>` | |

Exactly one of the next two options is required. Deleting a session decides the fate of
the Experience records derived from it, so the command refuses to guess.

| Option | Description | Default |
| --- | --- | --- |
| `--keep-derived-experiences` | Keep Experience records and detach them from the deleted session | |
| `--cleanup-derived-experiences` | Prepare reviewed Memory and Skill revocations and forget derived Experience records | |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

List the most recent root sessions for the current checkout.

```sh
zuno session list
```

Include child sessions and archived ones, as JSON, so a script can filter them.

```sh
zuno session list --no-roots --archived --format json
```

See what a 30-day cleanup would touch across every project, ordered by creation time,
without deleting anything.

```sh
zuno session prune --older-than 30 --by created --all-projects
```

Delete one session by id, keeping the Experience records it produced.

```sh
zuno session delete ses_1a2b3c --keep-derived-experiences
```

Forgetting the derived Experience records instead needs a live learning profile, so run
it from the TUI and choose `clean learning`, or use ACP with
`cleanupDerivedExperiences=true`. On the command line it succeeds only when the session
subtree produced no Experience records at all.

```sh
zuno session delete ses_1a2b3c --cleanup-derived-experiences
```

## See also

- [Global options](/cli/global-options)
- [zuno run](/cli/run)
- [zuno db](/cli/db)
- [Session retention](/session-retention)
- [Harness runtime](/harness-runtime)
