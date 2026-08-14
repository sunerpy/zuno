#!/bin/sh
set -eu

repo="sunerpy/zuno"
binary="zuno"
token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

say() {
  printf '%s\n' "$1" >&2
}

if command -v curl >/dev/null 2>&1; then
  download() {
    if [ -n "$token" ]; then
      curl -fsSL -H "Authorization: Bearer $token" "$1" -o "$2"
    else
      curl -fsSL "$1" -o "$2"
    fi
  }
  fetch() {
    if [ -n "$token" ]; then
      curl -fsSL -H "Authorization: Bearer $token" "$1"
    else
      curl -fsSL "$1"
    fi
  }
elif command -v wget >/dev/null 2>&1; then
  download() {
    if [ -n "$token" ]; then
      wget -q --header="Authorization: Bearer $token" -O "$2" "$1"
    else
      wget -qO "$2" "$1"
    fi
  }
  fetch() {
    if [ -n "$token" ]; then
      wget -q --header="Authorization: Bearer $token" -O - "$1"
    else
      wget -qO - "$1"
    fi
  }
else
  fail "curl or wget is required"
fi

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
  api="https://api.github.com/repos/${repo}/releases/latest"
  say "Resolving the latest Zuno release..."
  tag=$(fetch "$api" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | sed -n '1p')
  [ -n "$tag" ] || fail "could not resolve the latest release from $api"
  version=$(printf '%s' "$tag" | sed 's/^v//')
fi

target="${arch}-${os}"
asset="${binary}-${version}-${target}.tar.gz"
url="https://github.com/${repo}/releases/download/v${version}/${asset}"
install_dir="${ZUNO_INSTALL_DIR:-$HOME/.local/bin}"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t zuno)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Installing Zuno v${version} for ${target}..."
download "$url" "$tmp/$asset" || fail "download failed: $url"
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
