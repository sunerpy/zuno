#!/usr/bin/env bash
set -euo pipefail

readonly ZIG_VERSION="0.13.0"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "install-zig.sh supports Linux runners only" >&2
  exit 2
fi

case "$(uname -m)" in
  x86_64 | amd64)
    readonly archive_arch="x86_64"
    readonly archive_sha256="d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea"
    ;;
  aarch64 | arm64)
    readonly archive_arch="aarch64"
    readonly archive_sha256="041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556"
    ;;
  *)
    echo "unsupported Linux architecture: $(uname -m)" >&2
    exit 2
    ;;
esac

: "${GITHUB_PATH:?GITHUB_PATH must be set by GitHub Actions}"

readonly archive="zig-linux-${archive_arch}-${ZIG_VERSION}.tar.xz"
readonly url="https://ziglang.org/download/${ZIG_VERSION}/${archive}"
readonly temp_parent="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
install_root="$(mktemp -d "${temp_parent%/}/zuno-zig-${ZIG_VERSION}-${archive_arch}.XXXXXX")"
readonly install_root
readonly archive_path="${install_root}/${archive}"
readonly zig_dir="${install_root}/zig-linux-${archive_arch}-${ZIG_VERSION}"

curl \
  --fail \
  --location \
  --proto '=https' \
  --retry 5 \
  --retry-all-errors \
  --show-error \
  --silent \
  --tlsv1.2 \
  --output "$archive_path" \
  "$url"

printf '%s  %s\n' "$archive_sha256" "$archive_path" | sha256sum --check --strict -
tar -xJf "$archive_path" -C "$install_root"

actual_version="$("$zig_dir/zig" version)"
if [[ "$actual_version" != "$ZIG_VERSION" ]]; then
  echo "installed Zig version ${actual_version}, expected ${ZIG_VERSION}" >&2
  exit 1
fi

printf '%s\n' "$zig_dir" >> "$GITHUB_PATH"
printf 'installed Zig %s for Linux %s\n' "$actual_version" "$archive_arch"
