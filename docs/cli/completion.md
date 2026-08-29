# zuno completion

`zuno completion` prints the completion script for one shell to stdout. It writes nothing to
disk and edits no shell configuration, so you decide where the output goes: sourced directly
for the current shell, or saved into the completion directory your shell already loads.

## Synopsis

```sh
zuno completion [OPTIONS] <SHELL>
```

## Arguments

| Argument | Description |
| --- | --- |
| `<SHELL>` | Shell whose completion syntax should be emitted. Possible values: `bash`, `elvish`, `fish`, `powershell`, `zsh` |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Examples

Print the bash script to stdout to inspect it before installing anything.

```sh
zuno completion bash
```

Enable completion in the current bash shell only, without touching configuration files.

```sh
source <(zuno completion bash)
```

Install the zsh script into a completion directory your shell already loads.

```sh
zuno completion zsh > ~/.zsh/completions/_zuno
```

Install the fish script into the standard user completion directory.

```sh
zuno completion fish > ~/.config/fish/completions/zuno.fish
```

## See also

- [CLI reference](/cli/)
- [Global options](/cli/global-options)
- [FAQ](/faq)
