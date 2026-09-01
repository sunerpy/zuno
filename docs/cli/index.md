# CLI reference

Zuno is a single executable. Every capability the harness exposes, from a one-shot
prompt to durable session inspection, is reachable through a subcommand of `zuno`.
Running `zuno` with no subcommand starts the interactive terminal application, so
`zuno` and `zuno tui` are equivalent.

This reference is generated against the shipped binary. Each page reproduces the
options that command actually accepts, with the defaults the binary reports. Options
that carry no default in `--help` are documented without one.

```sh
zuno --help
zuno <command> --help
```

A small set of options is accepted by every subcommand. They are documented once, in
[Global options](/cli/global-options), rather than being explained again on each page.

## Running Zuno

| Command | Purpose |
| --- | --- |
| [`zuno run`](/cli/run) | Run a message through the harness non-interactively, with optional JSON output. |
| [`zuno tui`](/cli/tui) | Start the interactive terminal application. Also the default with no command. |
| [`zuno serve`](/cli/serve) | Start the headless server for external clients. |
| [`zuno acp`](/cli/acp) | Serve Agent Client Protocol over stdin and stdout for editor integration. |

## Managing state

| Command | Purpose |
| --- | --- |
| [`zuno session`](/cli/session) | List, prune, and delete durable sessions. |
| [`zuno agent`](/cli/agent) | Create agent definitions and list the agents currently resolved. |
| [`zuno db`](/cli/db) | Run a query against the local session database. |
| [`zuno export`](/cli/export) | Write configuration, Skills, extensions, and Agents into a portable bundle. |
| [`zuno import`](/cli/import) | Restore a portable bundle into this machine's user environment. |

## Providers and extensions

| Command | Purpose |
| --- | --- |
| [`zuno models`](/cli/models) | List the models reachable through the configured providers. |
| [`zuno providers`](/cli/providers) | Inspect providers and manage their stored credentials. |
| [`zuno mcp`](/cli/mcp) | Register, authenticate, and debug Model Context Protocol servers. |
| [`zuno plugin`](/cli/plugin) | Install, replace, remove, and inspect extension packages. |

## Maintenance

| Command | Purpose |
| --- | --- |
| [`zuno self-update`](/cli/self-update) | Replace the running executable from a checksum-verified GitHub release. |
| [`zuno debug`](/cli/debug) | Inspect paths, resolved configuration, prompts, permissions, sandbox, and snapshots. |
| [`zuno completion`](/cli/completion) | Generate or install completion for bash, elvish, fish, powershell, or zsh. |

## Excluded

| Command | Purpose |
| --- | --- |
| [Excluded commands](/cli/excluded) | `console`, `web`, `stats`, `github`, `pr`, `uninstall`, and `generate` are registered only to explain what replaces them. |

## See also

- [Global options](/cli/global-options)
- [Configuration reference](/reference/configuration)
- [Providers reference](/reference/providers)
- [Orchestration](/orchestration)
- [Logging](/logging)
- [FAQ](/faq)
