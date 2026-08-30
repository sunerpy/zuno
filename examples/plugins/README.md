# Zuno plugin examples

These packages exercise each supported extension tier:

- [`review-kit`](review-kit/extension.json): declarative custom subagent,
  slash-command workflow, and skill. The agent can inspect repository files,
  research the network, and request shell/environment access through native Zuno
  permissions.
- [`wasi-word-count`](wasi-word-count/README.md): in-process WASI Component
  Model tool with no host grants. Its README includes the Rust guest build.
- [`process-review`](process-review/extension.json): contained Python process
  tool using the `zuno.plugin/1` JSON-RPC protocol and explicit `host.full`
  authority.

Install a project-scoped example from the repository root:

```sh
zuno plugin add examples/plugins/review-kit --project
zuno plugin list
```

Replace or remove it:

```sh
zuno plugin update examples/plugins/review-kit --project
zuno plugin remove review-kit --project
```

Build and run the WASI integration fixture:

```sh
sh scripts/check-plugin-examples.sh
```

The script writes the ignored `wasi-word-count/plugin.wasm`, so that example is
immediately installable without copying its Cargo target directory.

See [`docs/plugins.md`](../../docs/plugins.md) for the manifest schema,
capability model, lifecycle guarantees, and protocol contracts.
