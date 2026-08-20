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
(`zuno_config::schema::plugin::PluginSpec`). The two-element form pairs a specifier
with options handed to the plugin at load time.

<!-- generated:BEGIN plugin-config-paths -->
Beyond the config array, Zuno scans every configuration directory for `plugin/*.{ts,js}` and `plugins/*.{ts,js}`. The directory chain is `$XDG_CONFIG_HOME/zuno`, project `.zuno` directories, `$HOME/.zuno`, then `OPENCODE_CONFIG_DIR`; files are sorted within `plugin/` and then `plugins/`. `OPENCODE_CONFIG_DIR` deliberately keeps its upstream spelling because installed npm plugins consume it as one of the six retained plugin-ABI environment names. Provenance is retained (`zuno_plugin::PluginOrigin`), successful discovery is visible at `DEBUG`, and scan or load failures are warnings that name the affected directory or plugin.
<!-- generated:END plugin-config-paths -->

### Tool name collisions

<!-- generated:BEGIN tool-source-precedence -->
The registry assembles sources in increasing precedence order. If tool ids collide, the later source replaces the earlier implementation in its existing provider-visible position and emits a suppression diagnostic naming both sources.

| order | source |
|---:|---|
| 1 | `built-in` |
| 2 | `config-directory` |
| 3 | `plugin` |
| 4 | `MCP` |

Highest-to-lowest winner precedence: `MCP > plugin > config-directory > built-in`.
<!-- generated:END tool-source-precedence -->

An npm plugin may declare its supported host range in
`package.json.engines.opencode`. The production loader checks that range against
the pinned compatibility baseline before importing the module: an excluding or
invalid range skips the plugin and reports `Plugin requires opencode <range> but
running <version>`, while a satisfying or absent range loads normally. Local
`file:` plugins bypass this package gate, matching upstream's development-code
exception. This is why `--version` reports the compatibility baseline and the
real build identity is exposed separately — see the `split-version-identity`
entry in [divergences.md](divergences.md). The excluding and satisfying cases
are executable production-loader regressions in `crates/zuno-plugin/tests/js.rs`.

A JavaScript plugin also gets the v1 SDK routes it calls. Only the routes with a
measured callsite are served; see the v1 table in
[compatibility-matrix.md](compatibility-matrix.md).

## Tier 2 — WebAssembly component

The capability boundary is the empty component linker: imports, **including WASI
filesystem and socket interfaces, are rejected before instantiation**. A component
that imports anything fails to load rather than loading with a surprise
capability. Granting one has to be an explicit, per-interface change to
`WasmPluginSpec`.

The guest world is `zuno_plugin::wasm::WASM_HOOK_WIT`. Every export corresponds
one-for-one with a hook below and takes `(input-json, output-json) -> string`: the
guest returns the complete replacement output JSON, or `null` for hooks that have
no mutable output.

```rust
use zuno_plugin::wasm::{WasmPluginSpec, load_wasm_plugins_ordered};

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

Protocol version `1.0` (`zuno_plugin_sdk::PROTOCOL_VERSION`). Three methods:

| method | when |
|---|---|
| `plugin.initialize` | first, exactly once; the host offers protocol versions and the plugin returns its manifest |
| `hook.call` | per hook invocation, after initialize |
| `tool.call` | per tool invocation, after initialize |

`hook.call` or `tool.call` before initialize is `-32002 plugin is not
initialized`; a second initialize is `-32003`; an unknown method is `-32601`.
Initialization, each hook, and each tool are governed by independent deadlines
(`zuno_plugin::DEFAULT_HOOK_TIMEOUT`, five seconds), and a plugin that crashes or
times out is disabled with a `PluginDiagnostic` rather than taking the turn down.

The host declares one with `zuno_plugin::PluginProcessSpec`:

```rust
use zuno_plugin::PluginProcessSpec;

let spec = PluginProcessSpec::new("my-plugin", "/usr/local/bin/my-plugin")
    .arg("--serve")
    .env("MY_PLUGIN_LOG", "debug");
```

### Install it: drop the executable in a directory

No manifest, no config entry, no install step. Put an executable file in
`plugin/` or `plugins/` under any configuration directory and it is a plugin:

```sh
install -m 755 ./my-plugin ~/.zuno/plugin/my-plugin     # every project
install -m 755 ./my-plugin .zuno/plugin/my-plugin       # this project only
```

The scan matches the JavaScript tier's: both child directories, one level deep,
**not** recursive, sorted by filename, and symlinks are followed — a symlink into
a build directory is the normal way to iterate on a plugin.

What counts as a candidate:

| | |
|---|---|
| **Unix** | the executable bit, for any user (`0o111`). It is what `PATH` lookup itself uses, so no extension convention is imposed on a language that has no build step. |
| **Windows** | there is no executable bit, so the extension carries the meaning: `.exe`, `.com`, `.bat`, `.cmd`. `.ps1` is **excluded** — it needs an interpreter argument the host cannot infer. |

`.js` and `.ts` are excluded on every platform even when executable. They belong
to the JavaScript tier, which speaks a different protocol; a file that is both a
script and executable would otherwise be started twice. Name a JavaScript
process-tier plugin without an extension and give it a shebang, as
[`examples/js_plugin`](../examples/js_plugin) does — weighing first what
[the shebang costs at deployment time](#the-javascript-example) against shipping a
compiled file, because that is the choice this row is really between.

The plugin's name in diagnostics is the file stem, until `plugin.initialize`
returns a manifest id. A plugin that dies before answering has no id, and this is
what lets the failure still name a file you can find.

This tier is **on by default**, unlike the JavaScript tier. A discovered
executable needs no runtime installed and no package fetched, so there is no cost
left to consent to. Turn it off with:

```json
{ "plugin_runtime": { "process": false } }
```

`--pure` disables it too: that flag means no external plugins, whatever the tier.

### The Go example

[`examples/go_plugin/main.go`](../examples/go_plugin/main.go) — standard library
only, no module required:

```sh
go build -o ~/.zuno/plugin/go-example ./examples/go_plugin
```

### The JavaScript example

[`examples/js_plugin`](../examples/js_plugin) — no dependencies, and no build
step, because the shebang is what makes it runnable:

```sh
install -m 755 examples/js_plugin ~/.zuno/plugin/js-example
```

The shebang does not remove that build step so much as move it to deployment
time: the interpreter it names has to resolve when the host spawns the file
**directly**, which is a stronger requirement than being runnable from an
interactive shell. Nothing a shell would have arranged is in play here, and a
version-manager shim — mise, asdf, volta — can satisfy the interpreter
interactively and still fail at this spawn. Zuno cannot tell that apart from a
plugin that crashed on its own: the child closes stdout with nothing on stderr,
and the user sees

```text
disabled plugin ... after startup failed: plugin connection is closed
```

with the file stem in place of the ellipsis. Two remedies, both removing the
lookup rather than repairing it. Name the interpreter by absolute path in the
shebang, which takes the `PATH` search and any shim out of the picture; or ship a
compiled executable, which removes the interpreter entirely — [the Go
example](#the-go-example) has nothing left to resolve, so this class of failure
cannot reach it.

### The Rust example

A complete, compiling plugin lives at
[`examples/rust_plugin.rs`](../examples/rust_plugin.rs) and is the reference for
this tier. It builds one `Plugin`, registers a tool and three hooks, and hands
the result to `serve`:

```rust
use zuno_plugin_sdk::{HandlerError, Plugin, ToolDefinition, ToolOutput, serve};
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

`zuno_plugin_sdk::ConformanceSuite` drives a plugin the way the host does —
initialize, then the hook and tool cases you declare — so a protocol mistake
surfaces in your own test run rather than as a disabled plugin in someone's
session. `examples/rust_plugin.rs` uses it; copy that shape.

The SDK is **not published to any registry**: this workspace is `publish = false`.
Vendor `crates/zuno-plugin-sdk` or copy the example, and for Go and JavaScript
copy the example outright — those two speak the wire directly and depend on
nothing.

### What this tier does not support

Stated plainly, because a capability that half-works costs more than one that is
absent:

| not supported | why |
|---|---|
| the `auth` hook | the Rust SDK rejects the name at construction (`crates/zuno-plugin-sdk/src/lib.rs:112`), and `JsonRpcPlugin` implements only `manifest`, `tools`, and `call`. A process plugin cannot contribute a credential loader. |
| the `provider` hook | same rejection. A process plugin cannot contribute a model list. |
| interactive flows | the transport is response-only. A frame from the plugin that is not a reply to a host request is logged and dropped (`crates/zuno-plugin/src/jsonrpc.rs:446`), so a plugin cannot prompt the user, request a terminal, or call back into the host. |
| sub-turn orchestration | a plugin cannot start a turn, spawn an agent, or drive the loop. Hooks observe and amend the data they are handed. |

Those four are the JavaScript tier's, and only the JavaScript tier's. Everything
else in the hook table below works here.

## The hooks

Every tier dispatches from the same 21 hooks, in upstream declaration order. The
table is generated from `zuno_plugin::hook_support()`, whose exhaustive mapping
requires every advertised hook to name its production lifecycle trigger.

Rows 5 (`auth`) and 6 (`provider`) are JavaScript-only; see
[what this tier does not support](#what-this-tier-does-not-support) above. The
other 19 are dispatched to every tier.

<!-- generated:BEGIN plugin-hooks -->
| hook | JavaScript / JSON-RPC name | production trigger |
|---:|---|---|
| 1 | `dispose` | runtime shutdown, before the plugin host is torn down |
| 2 | `event` | each event published on the real turn event stream |
| 3 | `config` | configuration finalization before turn composition |
| 4 | `tool` | executable tool-registry assembly |
| 5 | `auth` | provider-catalog credential enrichment |
| 6 | `provider` | provider-catalog model enrichment |
| 7 | `chat.message` | user-message construction before persistence |
| 8 | `chat.params` | provider request preparation after model resolution |
| 9 | `chat.headers` | provider request preparation after model resolution |
| 10 | `permission.ask` | tool permission decision before interactive approval |
| 11 | `command.execute.before` | command expansion before generated parts are persisted |
| 12 | `tool.execute.before` | tool dispatch before validation, permission, and execution |
| 13 | `shell.env` | shell child-process environment construction |
| 14 | `tool.execute.after` | tool completion before result persistence |
| 15 | `experimental.chat.messages.transform` | history projection before provider request preparation |
| 16 | `experimental.chat.system.transform` | system-prompt assembly before provider request preparation |
| 17 | `experimental.provider.small_model` | internal-agent model resolution |
| 18 | `experimental.session.compacting` | compaction request assembly before summary generation |
| 19 | `experimental.compaction.autocontinue` | overflow decision before automatic compaction |
| 20 | `experimental.text.complete` | completed text part before its final checkpoint |
| 21 | `tool.definition` | tool-definition snapshot before provider request preparation |
<!-- generated:END plugin-hooks -->

Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs
```

## What is not available

`zuno plugin` is **not registered**. Installing a plugin through the CLI
waits on the resident JavaScript host's compatibility gate; accepting installs
before plugins can load would write configuration that does not work. Declare
plugins in `zuno.json` instead. See the CLI table in
[compatibility-matrix.md](compatibility-matrix.md).
