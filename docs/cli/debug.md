# zuno debug

`zuno debug` answers "what does this installation actually think is true?" It reports resolved
paths and configuration, the exact prompt sent for a provider request, the effective
permission ruleset, whether a sandbox mode is deployable, what the file search backend sees,
what the language servers report, and what the snapshot store holds.

These are read-and-report surfaces for diagnosis. Reach for them when behavior disagrees with
configuration, when a permission prompt appears where you did not expect one, or when a
confinement mode fails at run time.

## Synopsis

```sh
zuno debug [OPTIONS] <COMMAND>
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
| [`paths`](#zuno-debug-paths) | |
| [`config`](#zuno-debug-config) | |
| [`agent`](#zuno-debug-agent) | |
| [`prompt`](#zuno-debug-prompt) | |
| [`permissions`](#zuno-debug-permissions) | |
| [`skill`](#zuno-debug-skill) | |
| [`sandbox`](#zuno-debug-sandbox) | |
| [`rg`](#zuno-debug-rg) | |
| [`lsp`](#zuno-debug-lsp) | |
| [`snapshot`](#zuno-debug-snapshot) | |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno debug paths

```sh
zuno debug paths [OPTIONS]
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

### zuno debug config

```sh
zuno debug config [OPTIONS]
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

### zuno debug agent

```sh
zuno debug agent [OPTIONS] <NAME>
```

| Argument | Description |
| --- | --- |
| `<NAME>` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno debug prompt

```sh
zuno debug prompt [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `--session <ID>` | Session whose prompt receipt should be shown; defaults to the latest receipt | |
| `-v`, `--version` | Show the Zuno package version | |
| `--step <N>` | One-based provider request step within the selected session | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--show-sensitive` | Include model-visible instruction, AGENTS, skill, and memory content | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

`--show-sensitive` prints instruction, AGENTS, skill, and memory content verbatim. Treat that
output as sensitive before pasting it into a ticket.

### zuno debug permissions

```sh
zuno debug permissions [OPTIONS]
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

### zuno debug skill

```sh
zuno debug skill [OPTIONS]
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

### zuno debug sandbox

```sh
zuno debug sandbox [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `--mode <MODE>` | Sandbox policy to probe; restricted mode verifies bubblewrap deployment. Possible values: `read-only`, `workspace-write`, `danger-full-access` | `workspace-write` |
| `-v`, `--version` | Show the Zuno package version | |
| `--network <NETWORK>` | Network authority to verify. Defaults to deny for confined modes and allow for danger-full-access. Possible values: `deny`, `allow` | |
| `--check` | Exit unsuccessfully when the requested policy is not deployable | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Report fallback resolution under this trusted invocation policy. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Report the resolution under this trusted backend selection; `native` skips confined-backend discovery and reports `trusted_native`. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

The report distinguishes requested and effective mode/network authority, names
the resolution kind, and includes fallback eligibility and a typed reason. The
`ready` field and `--check` remain strict: fallback eligibility never makes the
requested confinement deployment pass.

### zuno debug rg

```sh
zuno debug rg [OPTIONS] <COMMAND>
```

| Nested command | Description |
| --- | --- |
| [`files`](#zuno-debug-rg-files) | |
| [`search`](#zuno-debug-rg-search) | |
| `help` | Print this message or the help of the given subcommand(s) |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug rg files

```sh
zuno debug rg files [OPTIONS]
```

| Option | Description | Default |
| --- | --- | --- |
| `--query <QUERY>` | | |
| `-v`, `--version` | Show the Zuno package version | |
| `--glob <GLOB>` | | |
| `--limit <LIMIT>` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug rg search

```sh
zuno debug rg search [OPTIONS] <PATTERN>
```

| Argument | Description |
| --- | --- |
| `<PATTERN>` | |

| Option | Description | Default |
| --- | --- | --- |
| `--glob <GLOB>` | | |
| `-v`, `--version` | Show the Zuno package version | |
| `--limit <LIMIT>` | | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno debug lsp

```sh
zuno debug lsp [OPTIONS] <COMMAND>
```

| Nested command | Description |
| --- | --- |
| [`diagnostics`](#zuno-debug-lsp-diagnostics) | |
| [`symbols`](#zuno-debug-lsp-symbols) | |
| [`document-symbols`](#zuno-debug-lsp-document-symbols) | |
| `help` | Print this message or the help of the given subcommand(s) |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug lsp diagnostics

```sh
zuno debug lsp diagnostics [OPTIONS] <FILE>
```

| Argument | Description |
| --- | --- |
| `<FILE>` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug lsp symbols

```sh
zuno debug lsp symbols [OPTIONS] <QUERY>
```

| Argument | Description |
| --- | --- |
| `<QUERY>` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug lsp document-symbols

```sh
zuno debug lsp document-symbols [OPTIONS] <URI>
```

| Argument | Description |
| --- | --- |
| `<URI>` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno debug snapshot

```sh
zuno debug snapshot [OPTIONS] <COMMAND>
```

| Nested command | Description |
| --- | --- |
| [`track`](#zuno-debug-snapshot-track) | |
| [`patch`](#zuno-debug-snapshot-patch) | |
| [`diff`](#zuno-debug-snapshot-diff) | |
| `help` | Print this message or the help of the given subcommand(s) |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug snapshot track

```sh
zuno debug snapshot track [OPTIONS]
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

#### zuno debug snapshot patch

```sh
zuno debug snapshot patch [OPTIONS] <HASH>
```

| Argument | Description |
| --- | --- |
| `<HASH>` | |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

#### zuno debug snapshot diff

```sh
zuno debug snapshot diff [OPTIONS] <HASH>
```

| Argument | Description |
| --- | --- |
| `<HASH>` | |

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

Report the resolved data, config, and log paths for this installation.

```sh
zuno debug paths
```

Print the effective permission ruleset when a tool call is being stopped unexpectedly.

```sh
zuno debug permissions
```

Verify that a confinement mode is deployable and fail the command when it is not.

```sh
zuno debug sandbox --mode read-only --check
```

Inspect the prompt actually sent for a specific provider request step.

```sh
zuno debug prompt --session ses_1a2b3c --step 2
```

## See also

- [Global options](/cli/global-options)
- [zuno agent](/cli/agent)
- [zuno session](/cli/session)
- [zuno plugin](/cli/plugin)
- [Configuration reference](/reference/configuration)
- [Harness runtime](/harness-runtime)
- [Logging](/logging)
