# Installation

Zuno ships as one executable per platform. Installation is: get the binary onto `PATH`,
verify its checksum, and make sure `rg` is available. Nothing else has to be installed
or kept version-aligned.

## Requirements

| Requirement | Why |
| --- | --- |
| Linux, macOS, or Windows | Release targets below |
| `rg` (ripgrep) 14 or newer | `glob` and `grep` drive the real ripgrep executable |
| `bwrap` (bubblewrap) 0.8.0 or newer, Linux only | Required by the `read-only` and `workspace-write` sandbox backends |
| `curl` or `wget`, and `tar` | Only for the shell installer |

Without a working confinement backend on Linux, a restricted sandbox mode fails closed
by default. Install bubblewrap before relying on either confined mode, and verify it with
`zuno debug sandbox`. A trusted deployment that intentionally runs without OS
confinement can instead select explicit `danger-full-access`, or set
`sandbox.onUnavailable` to `run-unconfined` for eligible write-capable
`workspace-write` fallback. See [Permissions and sandboxing](/guide/permissions) for the
exact boundary, complete probe list, and Ubuntu AppArmor case.

## Install script

The installers download the release archive and its `SHA256SUMS`, compare the digest for
that exact asset, and refuse to extract on a mismatch. A checksum failure is a hard
error, never a warning.

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

Both installers read two environment variables:

| Variable | Meaning | Default |
| --- | --- | --- |
| `ZUNO_VERSION` | Release to install, with or without a leading `v` | Latest published release |
| `ZUNO_INSTALL_DIR` | Destination directory | `$HOME/.local/bin`; on Windows, `%LOCALAPPDATA%\Programs\zuno` |

```sh
ZUNO_VERSION=v0.2.0 ZUNO_INSTALL_DIR="$HOME/bin" sh -c "$(curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh)"
```

If the destination is not already on `PATH`, the installer prints the line to add.

## Release targets

| Host | Target | Archive |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS aarch64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |

Linux always uses the static musl artifact. `aarch64-pc-windows-msvc` is deliberately
absent: no available runner can execute that artifact, and the pipeline does not publish
a binary it never ran.

## Manual download and verification

Doing it by hand is the same three steps the installer performs, and is the right choice
when a policy forbids piping a remote script to a shell.

```sh
version=0.2.0
target=x86_64-unknown-linux-musl
base="https://github.com/sunerpy/zuno/releases/download/v${version}"

curl -fsSLO "${base}/zuno-${version}-${target}.tar.gz"
curl -fsSLO "${base}/SHA256SUMS"

grep " zuno-${version}-${target}.tar.gz\$" SHA256SUMS | sha256sum --check -
tar -xzf "zuno-${version}-${target}.tar.gz"
install -m 755 zuno "$HOME/.local/bin/zuno"
```

Verify the digest for your exact asset. A `SHA256SUMS` file listing five archives can
otherwise be used to "verify" a different one.

## Build from source

A source build is the path for local development or for a target the release matrix does
not cover.

```sh
cargo install --git https://github.com/sunerpy/zuno zuno-cli --locked
```

A source build has no channel define, so its channel is `local` and it opens
`zuno-local.db` rather than the release `zuno.db`. An empty session list right after
switching between an installed release and a source build is that, not lost data. See
[Database lifecycle](/migration) for how to point one build at the other's database.

## Confirm the installation

```sh
zuno --version
zuno debug paths
zuno debug sandbox --mode workspace-write --check
```

`debug paths` prints the resolved roots, which is how to confirm which configuration and
data directories this executable actually uses:

```text
home       /config
data       /config/.local/share/zuno
bin        /config/.cache/zuno/bin
log        /config/.local/share/zuno/log
repos      /config/.local/share/zuno/repos
cache      /config/.cache/zuno
config     /config/.config/zuno
state      /config/.local/state/zuno
tmp        /tmp/zuno
```

`debug sandbox --check` exits unsuccessfully when the requested policy is not deployable,
which makes it usable as a deployment gate rather than something to read by eye.

## Shell completion

```sh
zuno completion zsh > "${fpath[1]}/_zuno"
zuno completion bash > /etc/bash_completion.d/zuno
```

`bash`, `elvish`, `fish`, `powershell`, and `zsh` are supported. See
[zuno completion](/cli/completion).

## Upgrading

```sh
zuno self-update --check
zuno self-update
zuno self-update --tag v0.2.0
```

`self-update` replaces the running executable from a checksum-verified GitHub release. It
downloads `SHA256SUMS`, requires exactly one digest for the selected archive, and stops
before touching the current executable on any mismatch. Without `--yes`, a
non-interactive invocation fails closed instead of replacing the binary silently.

If the executable path is owned by another user, reinstall into a writable `PATH`
directory such as `$HOME/.local/bin` rather than running the updater with elevated
privileges. See [Self-update](/reference/self-update).

## Uninstalling

There is no `zuno uninstall` that does work; the command exists only to say so. Remove
the pieces yourself:

```sh
rm "$HOME/.local/bin/zuno"
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/zuno"
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/zuno"
```

The data root holds session databases, logs, and the credential store, so removing it
discards durable session history. Export first if any of it matters:

```sh
zuno export "$HOME/zuno-backup.zuno-bundle"
```

A default bundle carries configuration, Skills, extensions, and Agents, and deliberately
excludes session databases and credential stores. See
[Portable bundles](/reference/portable-bundles).

## See also

- [Quick start](/guide/quick-start)
- [Self-update](/reference/self-update)
- [Portable bundles](/reference/portable-bundles)
- [Database lifecycle](/migration)
