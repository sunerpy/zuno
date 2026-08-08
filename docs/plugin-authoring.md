# Plugin authoring

Three tiers of plugin run against this binary. They see the same hooks and the
same tool contract; they differ in where the code runs and what it is allowed to
touch.

| tier | where it runs | isolation | pick it when |
|---|---|---|---|
| JavaScript | resident JS host, in-process | none beyond the host's own limits | you are porting an existing `opencode` plugin unchanged |
| WebAssembly component | in-process, `wasmtime` | import-free linker; no filesystem, no sockets | you want the plugin unable to reach the machine at all |
| Out-of-process | a child process on stdio | OS process boundary | you want native code, a real language runtime, or a crash that cannot take the host down |

## Tier 1 — JavaScript

Declared exactly as upstream declares it, so an existing plugin needs no change:

```json
{
  "plugin": [
    "opencode-antigravity-auth",
    ["@sunerpy/opencode-kiro-auth", { "profile": "work" }]
  ]
}
```

A bare entry is an npm specifier, a `file://` URL, or a path
(`oc_config::schema::plugin::PluginSpec`). The two-element form pairs a specifier
with options handed to the plugin at load time. Beyond the config array, both
`plugin/` and `plugins/` directories are scanned for `*.ts` and `*.js` in the
global and project trees, and provenance is retained
(`oc_plugin::PluginOrigin`) so a diagnostic can name the file that contributed a
plugin.

An npm plugin whose `engines.opencode` range excludes the running version is
skipped, upstream's behaviour. That is why `--version` reports the pinned
compatibility baseline and the real build identity is exposed separately — see
the `split-version-identity` entry in [divergences.md](divergences.md).

A JavaScript plugin also gets the v1 SDK routes it calls. Only the routes with a
measured callsite are served; see the v1 table in
[compatibility-matrix.md](compatibility-matrix.md).

## Tier 2 — WebAssembly component

The capability boundary is the empty component linker: imports, **including WASI
filesystem and socket interfaces, are rejected before instantiation**. A component
that imports anything fails to load rather than loading with a surprise
capability. Granting one has to be an explicit, per-interface change to
`WasmPluginSpec`.

The guest world is `oc_plugin::wasm::WASM_HOOK_WIT`. Every export corresponds
one-for-one with a hook below and takes `(input-json, output-json) -> string`: the
guest returns the complete replacement output JSON, or `null` for hooks that have
no mutable output.

```rust
use oc_plugin::wasm::{WasmPluginSpec, load_wasm_plugins_ordered};

let load = load_wasm_plugins_ordered(vec![
    WasmPluginSpec::new("my-plugin", std::fs::read("my_plugin.wasm")?),
]);
```

This tier is behind the `wasm` cargo feature, because linking `wasmtime`
unconditionally would put a JIT in every build.

## Tier 3 — Out-of-process, over JSON-RPC

A child process speaking **newline-delimited JSON-RPC 2.0 on stdin and stdout**.
Standard output is reserved for frames; anything a plugin wants to log goes to
standard error, or one stray line corrupts the connection.

Protocol version `1.0` (`oc_plugin_sdk::PROTOCOL_VERSION`). Three methods:

| method | when |
|---|---|
| `plugin.initialize` | first, exactly once; the host offers protocol versions and the plugin returns its manifest |
| `hook.call` | per hook invocation, after initialize |
| `tool.call` | per tool invocation, after initialize |

`hook.call` or `tool.call` before initialize is `-32002 plugin is not
initialized`; a second initialize is `-32003`; an unknown method is `-32601`.
Initialization, each hook, and each tool are governed by independent deadlines
(`oc_plugin::DEFAULT_HOOK_TIMEOUT`, five seconds), and a plugin that crashes or
times out is disabled with a `PluginDiagnostic` rather than taking the turn down.

The host declares one with `oc_plugin::PluginProcessSpec`:

```rust
use oc_plugin::PluginProcessSpec;

let spec = PluginProcessSpec::new("my-plugin", "/usr/local/bin/my-plugin")
    .arg("--serve")
    .env("MY_PLUGIN_LOG", "debug");
```

### The Rust example

A complete, compiling plugin lives at
[`examples/rust_plugin.rs`](../examples/rust_plugin.rs) and is the reference for
this tier. It builds one `Plugin`, registers a tool and three hooks, and hands
the result to `serve`:

```rust
use oc_plugin_sdk::{HandlerError, Plugin, ToolDefinition, ToolOutput, serve};
use serde_json::json;

fn plugin() -> Result<Plugin, Box<dyn std::error::Error>> {
    Ok(Plugin::new("rust-example")
        .tool(
            ToolDefinition::new(
                "rust_echo",
                "Echo text from a Rust plugin",
                json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                }),
            ),
            |call| async move {
                let text = call.arguments["text"]
                    .as_str()
                    .ok_or_else(|| HandlerError::new("text must be a string"))?;
                Ok(ToolOutput::text("Rust echo", text))
            },
        )?
        .hook("shell.env", |mut call| async move {
            call.output["env"]["RUST_PLUGIN"] = json!("enabled");
            Ok(call)
        })?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(plugin()?).await?;
    Ok(())
}
```

`Plugin::tool` and `Plugin::hook` return `Result` on purpose: a duplicate tool id
or an unknown hook name is rejected at construction, before the host ever sees
the manifest.

### Prove it conforms before shipping it

`oc_plugin_sdk::ConformanceSuite` drives a plugin the way the host does —
initialize, then the hook and tool cases you declare — so a protocol mistake
surfaces in your own test run rather than as a disabled plugin in someone's
session. `examples/rust_plugin.rs` uses it; copy that shape.

## The hooks

Every tier dispatches the same 21 hooks, in upstream declaration order. Generated
from `oc_plugin::HookName::ALL`, so a hook added to the host appears here or the
documentation gate fails.

<!-- generated:BEGIN plugin-hooks -->
| hook | JavaScript / JSON-RPC name |
|---:|---|
| 1 | `dispose` |
| 2 | `event` |
| 3 | `config` |
| 4 | `tool` |
| 5 | `auth` |
| 6 | `provider` |
| 7 | `chat.message` |
| 8 | `chat.params` |
| 9 | `chat.headers` |
| 10 | `permission.ask` |
| 11 | `command.execute.before` |
| 12 | `tool.execute.before` |
| 13 | `shell.env` |
| 14 | `tool.execute.after` |
| 15 | `experimental.chat.messages.transform` |
| 16 | `experimental.chat.system.transform` |
| 17 | `experimental.provider.small_model` |
| 18 | `experimental.session.compacting` |
| 19 | `experimental.compaction.autocontinue` |
| 20 | `experimental.text.complete` |
| 21 | `tool.definition` |
<!-- generated:END plugin-hooks -->

Regenerate with:

```sh
OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs
```

## What is not available

`opencode-rust plugin` is **not registered**. Installing a plugin through the CLI
waits on the resident JavaScript host's compatibility gate; accepting installs
before plugins can load would write configuration that does not work. Declare
plugins in `opencode.json` instead. See the CLI table in
[compatibility-matrix.md](compatibility-matrix.md).
