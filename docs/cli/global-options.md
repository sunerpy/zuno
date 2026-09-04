# Global options

A few options are wired into every command in the tree rather than into individual
handlers. They control version reporting, log destination and verbosity, Shell
confinement, and help output, and they behave identically whether you pass them to
`zuno` itself or to any subcommand.

Because they are accepted everywhere, they are documented here once instead of being
repeated on each command page.

Each of them applies to the invocation it is passed to. Zuno reads them from the values
it resolved at startup rather than from the environment of the running process, which is
why they do not depend on that environment being rewritten; on Windows that also means a
program Zuno launches does not inherit them. See
[One invocation, one process](/cli/#one-invocation-one-process).

## Synopsis

```sh
zuno [OPTIONS] [COMMAND]
```

## Options

These options appear in the `--help` output of `zuno` and of every subcommand.

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE` (maximum tracing detail), `DEBUG` (verbose diagnostic events), `INFO` (normal operational events), `WARN` (warnings and errors), `ERROR` (errors only) | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Root invocation options

Running `zuno` with no subcommand starts the interactive terminal application, so the
root invocation additionally accepts the session and model selection options that
[`zuno tui`](/cli/tui) accepts.

| Option | Description | Default |
| --- | --- | --- |
| `--prompt <PROMPT>` | Submit this prompt on start, as though it had been typed and sent | |
| `-m`, `--model <MODEL>` | The model to use, as `provider/model` | |
| `--agent <AGENT>` | The agent to use | |
| `-c`, `--continue` | Continue the most recent session in this directory | |
| `-s`, `--session <SESSION>` | Talk in this exact session | |
| `--auto` | Approve every permission that is not explicitly denied, without asking. Upstream's own description ends in "(dangerous!)" and it means it: this replaces the human at the permission prompt, so a tool call the default ruleset would have stopped to ask about proceeds unattended | |

## Examples

Report the package version of the executable on `PATH`.

```sh
zuno --version
```

Mirror log output to stderr while running a one-shot prompt, so failures are visible
without opening the local log store.

```sh
zuno run --print-logs --log-level DEBUG "summarize the build failure"
```

Confine Shell to reads for a single interactive session, without changing configuration.

```sh
zuno tui --sandbox read-only
```

Permit a write-capable Agent to run with host authority when a typed confinement
availability failure is encountered. Read-only Agents still refuse, and managed
policy may override this back to `deny`.

```sh
zuno run --sandbox-on-unavailable run-unconfined "run the local build"
```

Run every Agent's Shell natively on a host without an OS sandbox while keeping the
configured permission mode. This is the one route a read-only Agent such as `plan`
has on macOS and Windows; it is a trusted declaration, not confinement, and managed
policy may override it back to `auto`.

```sh
zuno run --agent plan --sandbox-backend native "audit the retry policy"
```

Verify that a confinement mode is actually deployable on this host before relying on it.

```sh
zuno debug sandbox --mode read-only --check
```

## See also

- [CLI reference](/cli/)
- [zuno tui](/cli/tui)
- [zuno debug](/cli/debug)
- [Configuration reference](/reference/configuration)
- [Logging](/logging)
