# zuno completion

`zuno completion` generates completion from Zuno's current command tree. By default the
script is written to stdout. With `--install`, Zuno atomically writes it to a
deterministic user-level path and prints the activation instruction. It never edits a shell profile.

## Synopsis

```sh
zuno completion [OPTIONS] <SHELL>
```

## Arguments

| Argument | Description |
| --- | --- |
| `<SHELL>` | Completion syntax to generate. Possible values: `bash`, `elvish`, `fish`, `powershell`, `zsh` |

## Options

| Option | Description | Default |
| --- | --- | --- |
| `--install` | Install for the current user instead of writing the script to stdout | |
| `-v`, `--version` | Show the Zuno package version | |
| `--print-logs` | Print logs to stderr in addition to the structured local log store | |
| `--log-level <LOG_LEVEL>` | Set the minimum log level. Possible values: `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` | |
| `--sandbox <SANDBOX>` | Select Shell confinement for this invocation. Possible values: `read-only`, `workspace-write`, `danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | Select what happens when confined Shell cannot be deployed. Possible values: `deny`, `run-unconfined` | `deny` |
| `-h`, `--help` | Print help (see a summary with `-h`) | |

## Installation paths

| Shell | User-level path |
| --- | --- |
| Bash | `$XDG_DATA_HOME/bash-completion/completions/zuno`, normally `~/.local/share/bash-completion/completions/zuno` |
| Zsh | `~/.zsh/completions/_zuno` |
| Fish | `$XDG_CONFIG_HOME/fish/completions/zuno.fish`, normally `~/.config/fish/completions/zuno.fish` |
| PowerShell | `%LOCALAPPDATA%\zuno\completions\_zuno.ps1`; without `LOCALAPPDATA`, the XDG data path is used |
| Elvish | `$XDG_CONFIG_HOME/elvish/lib/zuno.elv`, normally `~/.config/elvish/lib/zuno.elv` |

Paths are resolved as native paths and may contain non-UTF-8 components on Unix.
Reinstalling replaces only the completion file; no `.bashrc`, `.zshrc`, PowerShell
profile, or other startup file is changed.

## Examples

Inspect or source generated completion without installing it:

```sh
zuno completion bash
source <(zuno completion bash)
```

Install zsh completion, then add its directory to `fpath` before `compinit`:

```sh
zuno completion zsh --install
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

Fish discovers its installed user completion automatically:

```sh
zuno completion fish --install
```

PowerShell installation prints the exact script path. Dot-source that file in the
current session or from a profile you manage:

```powershell
zuno completion powershell --install
. "$env:LOCALAPPDATA\zuno\completions\_zuno.ps1"
```

For Elvish:

```sh
zuno completion elvish --install
use zuno
```

## See also

- [Installation](/guide/installation)
- [CLI reference](/cli/)
- [Global options](/cli/global-options)
