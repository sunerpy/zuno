#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
package="$repository/examples/plugins/wasi-word-count"
guest="$package/guest"
build="$repository/target/plugin-examples/wasi-word-count"
artifact="$build/wasm32-wasip2/release/zuno_wasi_word_count_example.wasm"

CARGO_TARGET_DIR="$build" cargo build \
  --manifest-path "$guest/Cargo.toml" \
  --target wasm32-wasip2 \
  --release \
  --locked

cp "$artifact" "$package/plugin.wasm"

ZUNO_WASI_TEST_COMPONENT="$package/plugin.wasm" \
  cargo test \
    --manifest-path "$repository/Cargo.toml" \
    -p zuno-extension \
    --test runtime_hosts \
    wasi_plugin_fixture_negotiates_invokes_and_stops \
    -- \
    --ignored \
    --exact
