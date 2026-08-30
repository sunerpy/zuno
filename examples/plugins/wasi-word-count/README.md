# WASI word-count plugin

Build the component from the repository root:

```sh
CARGO_TARGET_DIR=target/plugin-examples/wasi-word-count \
  cargo build \
  --manifest-path examples/plugins/wasi-word-count/guest/Cargo.toml \
  --target wasm32-wasip2 \
  --release
cp \
  target/plugin-examples/wasi-word-count/wasm32-wasip2/release/zuno_wasi_word_count_example.wasm \
  examples/plugins/wasi-word-count/plugin.wasm
```

Then install it:

```sh
zuno plugin add examples/plugins/wasi-word-count
```

The example requests no capabilities. Its component cannot see the workspace,
process environment, or network. The external target directory prevents
`zuno plugin add` from copying Cargo build intermediates into the installed
package.
