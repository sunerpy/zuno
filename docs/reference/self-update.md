# Self-update

`zuno self-update` replaces the running Zuno executable with a prebuilt artifact
from `sunerpy/zuno` GitHub Releases. It is a native Rust command; it does not
invoke an installer script, package manager, or shell.

```sh
zuno self-update --check
zuno self-update
zuno self-update --yes
zuno self-update --tag v0.0.1
zuno self-update --tag v0.0.1 --force --yes
```

- `--check` only compares the running package version with the latest release.
  It conflicts with all mutating options.
- `--tag` selects one explicit semver release. A leading `v` is optional.
- `--force` permits reinstalling an equal or older selected release.
- `--yes` skips the terminal confirmation. Without it, non-interactive input
  fails closed instead of replacing the binary silently.

## Release and integrity contract

The updater supports exactly the targets certified by
`.github/workflows/release-candidate.yml` and promoted by
`.github/workflows/release.yml`:

| Host | Release target | Archive |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS aarch64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `.zip` |

Linux always selects the static musl artifact, even when the currently running
binary was built locally for a GNU target. Asset selection is exact:
`zuno-<version>-<target>.<archive>`. Substring matches and duplicate assets are
rejected.

Before extraction, Zuno downloads the release's `SHA256SUMS`, finds exactly one
digest for the selected archive, computes the local SHA-256, and compares them.
Missing, duplicate, malformed, or mismatched checksums stop the operation before
the current executable is touched. The extracted replacement must be a
non-empty regular file and, on Unix, carry an executable mode. Replacement uses
the platform-aware atomic self-replace implementation.

## Authentication, proxies, and permissions

Public releases need no credential. For a private repository, provide
`GITHUB_TOKEN` or `GH_TOKEN`; `GITHUB_TOKEN` has priority and blank values are
ignored:

```sh
GH_TOKEN="$(gh auth token)" zuno self-update --check
GH_TOKEN="$(gh auth token)" zuno self-update --yes
```

Release API and asset downloads inherit the process proxy environment, including
`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` in the forms supported by
the HTTP client.

Zuno replaces the executable resolved by the operating system for the running
process. If that path is owned by another user, reinstall into a writable PATH
directory such as `$HOME/.local/bin`, or rerun with the privileges that own the
existing file.
