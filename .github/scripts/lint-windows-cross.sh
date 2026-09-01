#!/usr/bin/env bash
# Catch cfg(windows), test-compilation, and Windows-only Clippy failures from a
# Linux workstation before spending a native GitHub-hosted Windows runner. This
# is a predictive source gate; native MSVC/ConPTY execution remains authoritative.
set -euo pipefail

target=x86_64-pc-windows-gnu
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
script_path="$script_dir/$(basename -- "${BASH_SOURCE[0]}")"
invoked_as=$(basename -- "$0")

filter_zig_arguments() {
  filtered_arguments=()
  for argument in "$@"; do
    case "$argument" in
      "--target=$target")
        # cc-rs already selects the target through this shim.
        ;;
      "-Wl,--disable-auto-image-base")
        # rustc's GNU target spec passes this PE/COFF ld.bfd switch. Zig uses
        # LLD, which rejects it; test executables do not depend on automatic
        # DLL image-base assignment, so the predictive link gate drops it.
        ;;
      *)
        filtered_arguments+=("$argument")
        ;;
    esac
  done
}

# The same checked-in file doubles as the four compiler-driver shims that
# cc-rs and Cargo discover for the GNU Windows target.
case "$invoked_as" in
  x86_64-w64-mingw32-gcc)
    filter_zig_arguments "$@"
    exec zig cc -target x86_64-windows-gnu "${filtered_arguments[@]}"
    ;;
  x86_64-w64-mingw32-g++)
    filter_zig_arguments "$@"
    exec zig c++ -target x86_64-windows-gnu "${filtered_arguments[@]}"
    ;;
  x86_64-w64-mingw32-ar)
    exec zig ar "$@"
    ;;
  x86_64-w64-mingw32-ranlib)
    exec zig ranlib "$@"
    ;;
  x86_64-w64-mingw32-dlltool)
    exec zig dlltool "$@"
    ;;
esac

if [[ "$(uname -s)" != "Linux" ]]; then
  printf '%s\n' \
    "lint-windows-cross is a Linux preflight; run native workspace Clippy on Windows"
  exit 2
fi

for required_tool in cargo cargo-zigbuild rustup zig; do
  if ! command -v "$required_tool" >/dev/null 2>&1; then
    printf 'missing required tool: %s\n' "$required_tool" >&2
    exit 2
  fi
done

if ! rustup target list --installed | grep -Fxq "$target"; then
  printf 'missing Rust target %s; install it with:\n' "$target" >&2
  printf '  rustup target add %s\n' "$target" >&2
  exit 2
fi

wrapper_dir=$(mktemp -d "${TMPDIR:-/tmp}/zuno-windows-gnu.XXXXXX")
cleanup() {
  rm -rf -- "$wrapper_dir"
}
trap cleanup EXIT

for wrapper in \
  x86_64-w64-mingw32-gcc \
  x86_64-w64-mingw32-g++ \
  x86_64-w64-mingw32-ar \
  x86_64-w64-mingw32-ranlib \
  x86_64-w64-mingw32-dlltool; do
  ln -s "$script_path" "$wrapper_dir/$wrapper"
done

target_base=${CARGO_TARGET_DIR:-"$PWD/target"}
cross_target_dir=${ZUNO_WINDOWS_CLIPPY_TARGET_DIR:-"$target_base/windows-gnu-clippy"}
zig_cache_dir=${ZUNO_WINDOWS_ZIG_CACHE_DIR:-"$cross_target_dir/zig-cache"}
mkdir -p "$cross_target_dir" "$zig_cache_dir/global" "$zig_cache_dir/local"

export PATH="$wrapper_dir:$PATH"
export CARGO_TARGET_DIR="$cross_target_dir"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export RANLIB_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ranlib
export AWS_LC_SYS_NO_JITTER_ENTROPY=1
export ZIG_GLOBAL_CACHE_DIR="$zig_cache_dir/global"
export ZIG_LOCAL_CACHE_DIR="$zig_cache_dir/local"

cargo_arguments=(
  clippy
  --workspace
  --all-targets
  --target
  "$target"
)
if [[ "${ZUNO_CARGO_OFFLINE:-1}" != "0" ]]; then
  cargo_arguments+=(--offline)
fi
cargo_arguments+=(-- -D warnings)

printf '%s\n' \
  "running Windows cfg/Clippy preflight with Zig; native MSVC CI is still required"
cargo "${cargo_arguments[@]}"

unset CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER
unset CC_x86_64_pc_windows_gnu
unset CXX_x86_64_pc_windows_gnu
unset AR_x86_64_pc_windows_gnu
unset RANLIB_x86_64_pc_windows_gnu

test_arguments=(
  zigbuild
  --workspace
  --tests
  --target
  "$target"
  --target-dir
  "$target_base/windows-gnu-zigbuild"
)
if [[ "${ZUNO_CARGO_OFFLINE:-1}" != "0" ]]; then
  test_arguments+=(--offline)
fi

printf '%s\n' \
  "linking every Windows GNU test binary with cargo-zigbuild without executing it"
cargo "${test_arguments[@]}"
