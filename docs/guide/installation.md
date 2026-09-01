# Installation

Zuno ships as one executable per platform. The prebuilt binary can start, load
configuration, open its database, connect to providers, and serve TUI, headless, ACP,
and HTTP clients without Node, Python, ripgrep, or bubblewrap.

## Dependency boundaries

| Requirement | Scope |
| --- | --- |
| Linux, macOS, or Windows | Supported release hosts are listed below |
| `rg` (ripgrep) 14 or newer | Backend for the `glob` and `grep` tools only; not a Zuno startup or core-runtime dependency |
| `bwrap` (bubblewrap) 0.8.0 or newer | Linux-only backend for confined `read-only` and `workspace-write` Shell execution |
| `curl` or `wget`, `tar`, and `sha256sum` or `shasum` | Linux/macOS installer only |
| Windows PowerShell 5.1 or PowerShell 7 | Windows installer; it uses `Invoke-WebRequest`, `Get-FileHash`, and `Expand-Archive` |

If `rg` is absent, Zuno still starts; only calls that need the real `glob` or `grep`
backend are unavailable. If `bwrap` is absent, Linux confinement is unavailable, but
that does not prevent Zuno itself from starting or an explicitly trusted native mode
from running.

### Sandbox behavior by platform

| Platform | Confined `read-only` / `workspace-write` | Native execution |
| --- | --- | --- |
| Linux | Requires trusted bubblewrap 0.8.0 or newer | Explicit `danger-full-access`, or eligible trusted `workspace-write` fallback with `run-unconfined` |
| macOS | Not implemented | Explicit `danger-full-access`, or eligible trusted `workspace-write` fallback with `run-unconfined` |
| Windows | Not implemented | Explicit `danger-full-access`, or eligible trusted `workspace-write` fallback with `run-unconfined` |

`run-unconfined` is not a general “ignore the sandbox” switch. It applies only when a
write-capable `workspace-write` request encounters a typed, eligible backend-availability
failure. `read-only` never falls back and continues to fail closed. See
[Permissions and sandboxing](/guide/permissions).

## Cargo installation

Install the released `zuno` binary crate from crates.io:

```sh
cargo install zuno --locked
```

This compiles Zuno for the current host and therefore requires Rust 1.98 plus the native
build prerequisites listed under [Build from source](#build-from-source). Use a release
installer below when you prefer a prebuilt, platform-certified archive.

## Release installers

The installers download the release archive and `SHA256SUMS`, select the line for that
exact asset, compare its SHA-256 digest, and refuse to extract on any mismatch.

### Linux and macOS

The shell installer selects x86_64 or aarch64 from `uname`, requires `curl` or `wget`,
`tar`, and either `sha256sum` or `shasum`, and installs to `$HOME/.local/bin` by
default:

```sh
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

To pin a release or destination:

```sh
ZUNO_VERSION=vX.Y.Z \
ZUNO_INSTALL_DIR="$HOME/bin" \
sh -c "$(curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh)"
```

Replace `X.Y.Z` with the exact published release you intend to install. The
unpinned command above resolves the latest published release.

### Windows

Run the Windows installer from Windows PowerShell 5.1 or PowerShell 7. It selects the
x86_64 or ARM64 MSVC archive from the native process architecture and installs to
`$env:LOCALAPPDATA\Programs\zuno` by default:

```powershell
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

To pin a release or destination:

```powershell
$env:ZUNO_VERSION = "vX.Y.Z"
$env:ZUNO_INSTALL_DIR = Join-Path $HOME "bin"
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

Both installers print the `PATH` change when the destination is not already visible.
Open a new terminal after changing the user `PATH`.

## Release targets

| Host | Target | Archive |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS aarch64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `.zip` |

Linux uses the static musl artifact.

## Manual download and checksum verification

When policy forbids piping a remote script into a shell, reproduce the installer steps
manually. On Linux x86_64:

```sh
version=X.Y.Z
target=x86_64-unknown-linux-musl
asset="zuno-${version}-${target}.tar.gz"
base="https://github.com/sunerpy/zuno/releases/download/v${version}"

curl -fsSLO "${base}/${asset}"
curl -fsSLO "${base}/SHA256SUMS"
grep " ${asset}\$" SHA256SUMS | sha256sum --check -
tar -xzf "$asset"
install -m 755 zuno "$HOME/.local/bin/zuno"
```

On Windows, select the target from the native process architecture:

```powershell
$version = "X.Y.Z"
$target = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) {
  "X64"   { "x86_64-pc-windows-msvc" }
  "Arm64" { "aarch64-pc-windows-msvc" }
  default { throw "unsupported Windows architecture: $_" }
}
$asset = "zuno-$version-$target.zip"
$base = "https://github.com/sunerpy/zuno/releases/download/v$version"

Invoke-WebRequest "$base/$asset" -OutFile $asset
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS
$line = Get-Content SHA256SUMS |
  Where-Object { $_ -match "\s\*?$([Regex]::Escape($asset))$" } |
  Select-Object -First 1
if (-not $line) { throw "$asset is absent from SHA256SUMS" }
$expected = ($line -split "\s+")[0].ToLowerInvariant()
$actual = (Get-FileHash -Algorithm SHA256 $asset).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch for $asset" }
Expand-Archive $asset -DestinationPath .
```

Always match the exact asset name. A checksum file containing several archives must not
be treated as proof for a different file.

## Configuration and data paths

Zuno uses its own XDG-style layout on every platform, including macOS and Windows:

| Platform | Default configuration | Default durable data |
| --- | --- | --- |
| Linux | `${XDG_CONFIG_HOME:-$HOME/.config}/zuno` | `${XDG_DATA_HOME:-$HOME/.local/share}/zuno` |
| macOS | `${XDG_CONFIG_HOME:-$HOME/.config}/zuno` | `${XDG_DATA_HOME:-$HOME/.local/share}/zuno` |
| Windows | `$HOME\.config\zuno` | `$HOME\.local\share\zuno` |

The Windows configuration is not implicitly stored under `%APPDATA%`. Set
`XDG_CONFIG_HOME` or `ZUNO_CONFIG_DIR` when a managed deployment needs another root.
`ZUNO_CONFIG_DIR` adds a final higher-precedence configuration directory; use
`zuno debug paths` and `zuno debug config` to verify the resolved result.

PowerShell example:

```powershell
$config = Join-Path $HOME ".config\zuno"
New-Item -ItemType Directory -Force -Path $config | Out-Null
Copy-Item .\examples\config\zuno.json (Join-Path $config "zuno.json")
notepad (Join-Path $config "zuno.json")

# Optional switchable overlay:
$env:ZUNO_CONFIG_DIR = Join-Path $config "profiles\work"
zuno debug paths
zuno debug config
```

## Build from source

Source builds require:

- Git;
- Rust 1.98.0 with Cargo; `rustfmt` and Clippy are required for repository gates;
- a working C compiler and native linker because bundled SQLite and `aws-lc-sys` build
  native code;
- Linux: GCC or Clang plus the normal native linker;
- macOS: Xcode Command Line Tools (`xcode-select --install`);
- Windows: Visual Studio 2022 Build Tools with the MSVC v143 C++ toolchain and a
  Windows SDK, run from an x64 developer environment.

Ripgrep and bubblewrap are runtime tool/backend dependencies described above, not
source-compilation prerequisites.

```sh
rustup toolchain install 1.98.0 --component rustfmt clippy
git clone https://github.com/sunerpy/zuno.git
cd zuno
cargo build --locked -p zuno --bin zuno
cargo test -p zuno --test docs
```

For an unreleased Git checkout through Cargo:

```sh
cargo install --git https://github.com/sunerpy/zuno zuno --locked
```

A source build has channel `local` and normally opens `zuno-local.db`; a published
release opens `zuno.db`. An apparently empty session list after switching builds usually
means a different channel database was selected. See [Database lifecycle](/migration).

## Verify the installation

Linux and macOS:

```sh
command -v zuno
zuno --version
zuno debug paths
```

Windows PowerShell:

```powershell
Get-Command zuno
zuno --version
zuno debug paths
```

Verify optional tool backends separately:

```sh
rg --version
# Linux confined modes only:
bwrap --version
zuno debug sandbox --mode workspace-write --check
```

On macOS and Windows, a confined `workspace-write` check is expected to report that the
OS backend is not implemented. To verify only the explicit native path without running a
model task:

```powershell
zuno debug sandbox --mode danger-full-access --check
```

## Shell completion

Generate a script on stdout for inspection or manual placement:

```sh
zuno completion bash
```

Or install it into the current user's deterministic completion directory:

```sh
zuno completion bash --install
zuno completion zsh --install
zuno completion fish --install
zuno completion powershell --install
zuno completion elvish --install
```

Installation creates or atomically replaces only the completion file. It does not edit
a shell profile; the command prints the installed path and the activation instruction.
See [Shell completion](/cli/completion).

## Upgrading

```sh
zuno self-update --check
zuno self-update
zuno self-update --tag vX.Y.Z
```

`self-update` verifies the exact archive before atomically replacing the executable. A
non-interactive replacement requires `--yes`. If the executable is not writable, install
into a user-owned directory instead of elevating the updater. See
[Self-update](/reference/self-update).

## Uninstalling

Remove the executable separately from configuration and durable data. Deleting the data
root discards session databases, logs, and credentials.

```sh
rm "$HOME/.local/bin/zuno"
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/zuno"
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/zuno"
```

Windows PowerShell:

```powershell
Remove-Item (Join-Path $env:LOCALAPPDATA "Programs\zuno\zuno.exe")
# Delete these only when their configuration and history are no longer needed:
Remove-Item -Recurse -Force (Join-Path $HOME ".config\zuno")
Remove-Item -Recurse -Force (Join-Path $HOME ".local\share\zuno")
Remove-Item -Recurse -Force (Join-Path $HOME ".cache\zuno")
```

Export anything that must survive before deleting durable data. Portable bundles
deliberately exclude session databases and credential stores; see
[Portable bundles](/reference/portable-bundles).

## See also

- [Quick start](/guide/quick-start)
- [Permissions and sandboxing](/guide/permissions)
- [Self-update](/reference/self-update)
- [Database lifecycle](/migration)
