#!/bin/sh
set -eu

repo="sunerpy/zuno"
binary="zuno"

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

say() {
  printf '%s\n' "$1" >&2
}

command -v gh >/dev/null 2>&1 || fail "GitHub CLI (gh) is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

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
  tag=$(gh release view --repo "$repo" --json tagName --jq .tagName) \
    || fail "could not resolve the latest release for $repo"
  [ -n "$tag" ] || fail "the latest release for $repo has no tag"
  version=$(printf '%s' "$tag" | sed 's/^v//')
fi

target="${arch}-${os}"
asset="${binary}-${version}-${target}.tar.gz"
install_dir="${ZUNO_INSTALL_DIR:-$HOME/.local/bin}"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t zuno)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Installing Zuno v${version} for ${target}..."
gh release download "v${version}" --repo "$repo" --pattern "$asset" --dir "$tmp" \
  || fail "download failed: ${repo} v${version} asset ${asset}"
tar -xzf "$tmp/$asset" -C "$tmp" || fail "could not extract $asset"
[ -f "$tmp/$binary" ] || fail "$asset does not contain $binary"

mkdir -p "$install_dir"
mv "$tmp/$binary" "$install_dir/$binary"
chmod +x "$install_dir/$binary"
say "Installed $binary to $install_dir/$binary"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) say "Add $install_dir to PATH: export PATH=\"$install_dir:\$PATH\"" ;;
esac
