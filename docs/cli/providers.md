# zuno providers

`zuno providers` manages which model providers this installation can reach and the
credentials it holds for them. Login methods differ per provider, so the flow is: ask which
methods a provider implements, authenticate with one of them, and remove the stored
credential when it is no longer wanted.

Credentials are stored locally. `logout` removes them, which is what to run before handing
over a machine or rotating an account.

## Synopsis

```sh
zuno providers [OPTIONS] <COMMAND>
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
| [`list`](#zuno-providers-list) | |
| [`methods`](#zuno-providers-methods) | List the login methods implemented for a provider |
| [`login`](#zuno-providers-login) | Authenticate a provider with one of its implemented login methods |
| [`logout`](#zuno-providers-logout) | |
| `help` | Print this message or the help of the given subcommand(s) |

### zuno providers list

```sh
zuno providers list [OPTIONS]
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

### zuno providers methods

List the login methods implemented for a provider.

```sh
zuno providers methods [OPTIONS] <PROVIDER>
```

| Argument | Description |
| --- | --- |
| `<PROVIDER>` | Provider id or display name |

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

### zuno providers login

Authenticate a provider with one of its implemented login methods.

```sh
zuno providers login [OPTIONS] [TARGET]
```

| Argument | Description |
| --- | --- |
| `[TARGET]` | Provider id/name, or an HTTPS URL implementing `/.well-known/zuno`. Omit in a terminal to choose interactively. A URL login runs a program that the remote host names, shown for confirmation before it starts |

| Option | Description | Default |
| --- | --- | --- |
| `-p`, `--provider <PROVIDER>` | Provider id or display name, as an alternative to the positional target | |
| `-v`, `--version` | Show the Zuno package version | |
| `-m`, `--method <METHOD>` | Method id shown by `zuno auth methods <provider>`. Omit in a terminal to choose when several methods are available | |
| `--trust-remote-command` | Run the program named by the URL's `/.well-known/zuno` document without an interactive confirmation. The remote host chooses that program and its arguments, and it runs with your privileges: pass this only for a host you already trust. Without it, a URL login refuses when stdin or stderr is not a terminal | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | Select the Shell execution backend for this invocation; `native` is not confinement. Possible values: `auto`, `native` | `auto` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

A URL target executes a command chosen by the remote host. `zuno auth login <url>` accepts
`https://` to any host, or plain `http://` to a loopback IP address such as
`http://127.0.0.1:8080` or `http://[::1]:3000`; the URL must carry no userinfo. A hostname
that merely begins with `127.0.0.1` (for example `http://127.0.0.1.attacker.example`), a
`user@host` form, and `http://localhost` are refused before anything is fetched. Zuno
downloads `<url>/.well-known/zuno`, prints the program and every argument that document
names — one argument per line, quoted — and asks `Run this command` with `No` preselected;
Enter, Esc, or Ctrl-C declines and nothing is run or stored. The credential is stored only
after the confirmed command exits successfully. When stdin or stderr is not a terminal the
login refuses instead of prompting; pass `--trust-remote-command` to run the remote-chosen
command without confirmation, for a host you already trust. The flag is refused for
provider logins.

### zuno providers logout

```sh
zuno providers logout [OPTIONS] [PROVIDER]
```

| Argument | Description |
| --- | --- |
| `[PROVIDER]` | |

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

See which providers this installation knows about.

```sh
zuno providers list
```

Ask a provider which login methods it implements before choosing one.

```sh
zuno providers methods openai
```

Authenticate with an explicit method instead of the interactive picker.

```sh
zuno providers login openai --method api-key
```

Remove the stored credential for one provider.

```sh
zuno providers logout openai
```

## See also

- [Global options](/cli/global-options)
- [zuno models](/cli/models)
- [zuno mcp](/cli/mcp)
- [Excluded commands](/cli/excluded)
- [Providers reference](/reference/providers)
- [Configuration reference](/reference/configuration)
