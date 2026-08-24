#!/bin/sh
# Install a released Zuno binary on Linux or macOS.
#
# The archive is never extracted before its SHA-256 has been compared against the
# `SHA256SUMS` published with the SAME release. That check is the point of the
# script rather than a nicety: a one-line installer downloads remote content and
# puts it on the user's PATH, so a mismatch is a hard failure and never a warning.
#
# Environment:
#   ZUNO_VERSION      release to install, with or without a leading `v`.
#                     Defaults to the latest published release.
#   ZUNO_INSTALL_DIR  destination directory. Defaults to `$HOME/.local/bin`.
set -eu

repo="sunerpy/zuno"
binary="zuno"
checksum_file="SHA256SUMS"

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

say() {
  printf '%s\n' "$1" >&2
}

# curl or wget, whichever exists. The previous version of this script required the
# GitHub CLI, which was only ever needed to read a private repository; a public
# release is plain HTTPS, and requiring `gh` turned a one-line install into a
# prerequisite hunt.
if command -v curl > /dev/null 2>&1; then
  download() { curl -fsSL "$1" -o "$2"; }
  fetch() { curl -fsSL "$1"; }
elif command -v wget > /dev/null 2>&1; then
  download() { wget -qO "$2" "$1"; }
  fetch() { wget -qO - "$1"; }
else
  fail "curl or wget is required"
fi

command -v tar > /dev/null 2>&1 || fail "tar is required"

case "$(uname -s)" in
  Linux) os="unknown-linux-musl" ;;
  Darwin) os="apple-darwin" ;;
  *) fail "unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac

if [ -n "${ZUNO_VERSION:-}" ]; then
  version=$(printf '%s' "$ZUNO_VERSION" | sed 's/^v//')
else
  say "Resolving the latest Zuno release..."
  tag=$(
    fetch "https://api.github.com/repos/${repo}/releases/latest" \
      | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      | head -1
  ) || fail "could not resolve the latest release for $repo"
  [ -n "$tag" ] || fail "the latest release for $repo has no tag"
  version=$(printf '%s' "$tag" | sed 's/^v//')
fi

target="${arch}-${os}"
asset="${binary}-${version}-${target}.tar.gz"
base_url="https://github.com/${repo}/releases/download/v${version}"
install_dir="${ZUNO_INSTALL_DIR:-$HOME/.local/bin}"

tmp=$(mktemp -d 2> /dev/null || mktemp -d -t zuno)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Installing Zuno v${version} for ${target}..."
download "${base_url}/${asset}" "$tmp/$asset" \
  || fail "download failed: ${base_url}/${asset}"
download "${base_url}/${checksum_file}" "$tmp/$checksum_file" \
  || fail "download failed: ${base_url}/${checksum_file}"

# The line for THIS asset, so a checksum file listing five archives cannot end up
# verifying a different one. `sub` strips the `*` a binary-mode digest emits.
expected=$(awk -v name="$asset" '{
  file = $2
  sub(/^\*/, "", file)
  if (file == name) { print $1; exit }
}' "$tmp/$checksum_file")
[ -n "$expected" ] || fail "${asset} is not listed in ${checksum_file}"

if command -v sha256sum > /dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
elif command -v shasum > /dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
else
  fail "sha256sum or shasum is required to verify the download"
fi

if [ "$actual" != "$expected" ]; then
  fail "checksum mismatch for ${asset}: expected ${expected}, got ${actual}"
fi
say "Verified ${asset} against ${checksum_file}."

tar -xzf "$tmp/$asset" -C "$tmp" || fail "could not extract $asset"
[ -f "$tmp/$binary" ] || fail "$asset does not contain $binary"

mkdir -p "$install_dir" || fail "could not create $install_dir"
mv "$tmp/$binary" "$install_dir/$binary" || fail "could not install into $install_dir"
chmod +x "$install_dir/$binary"
say "Installed $binary to $install_dir/$binary"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) say "Add $install_dir to PATH: export PATH=\"$install_dir:\$PATH\"" ;;
esac
