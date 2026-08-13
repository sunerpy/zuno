# F2 Same-HEAD Confirmation

- Audited HEAD: `98be20aa`
- Scope: todo 179 regression confirmation against F2's Round 2 approval at `647a2d64`
- Verdict: **APPROVE**

## Evidence Log

- Review started with the verdict intentionally pending.
- Confirmed the worktree is at `98be20aae439494b47d195c07e3115ff061bfb1e`; root filesystem had 99 GiB available before mutation work.
- Reviewed `git diff 647a2d64..98be20aa -- crates/` in full (8 files, 288 insertions, 29 deletions).
- The generator is not silently permissive in its normal scan: it requires component schemas and paths, rejects duplicate operation IDs, and asserts that at least one Model/Provider-bearing arrival exists. Its emitted enum also carries exhaustive `method`, `path`, and `operation_id` matches plus `ALL`.
- `JsModelArrival::projection()` consumes `GeneratedClientArrival` exhaustively without a wildcard. The server changes replace the prior route literals with generated constants; the v1 surface and backend table both use the same `ProviderList.path()` value, so the recorded-payload adapter mapping is not split or independently rewritten by todo 179.
- No direct todo 179 regression was found in the reviewed delta. Mutation and workspace-gate evidence remain pending.
- Degradation mutation: after backing up `crates/oc-plugin-sdk/build.rs`, replaced the scanned arrival set with `Vec::new()` so the build script emitted an empty enum. `CARGO_TARGET_DIR=/tmp/opencode/f2c cargo build -p oc-plugin --offline` failed with exit 101 and 18 `E0599` errors at the exhaustive projection consumer, including missing `ProviderList`, `V2ModelList`, `V2ProviderList`, and `V2ProviderGet`. This proves an empty/trivially degraded generator is observable rather than silently accepted.
- Restored `build.rs` from the backup in the same command and confirmed `git diff --exit-code -- crates/oc-plugin-sdk/build.rs` was clean.
- Workspace gate with `CARGO_TARGET_DIR=/tmp/opencode/f2c`:
  - `cargo test --workspace --offline`: exit 0; 3473 passed, 0 failed, 2 ignored across 217 harnesses; no `FAILED` marker.
  - `cargo clippy --workspace --all-targets --offline`: exit 0; 0 warnings and 0 errors.
  - `cargo fmt --all --check`: exit 0; clean output.
- `lsp_diagnostics` cannot open paths outside the request's initial cwd, so the external `tF2` path was rejected. The main checkout is frozen at the same audited HEAD; diagnostics were therefore run against its same-HEAD copies of all six todo 179 Rust files (`build.rs`, `generated_client.rs`, `lib.rs`, `projection.rs`, `api/mod.rs`, and `compat_v1.rs`). All six returned `No diagnostics found`. The only persistent review artifact is Markdown and has no applicable Rust LSP.
- Removed `/tmp/opencode/f2c` and the temporary gate logs. Final worktree status was clean; the ignored evidence report remained present.

## Verdict

**APPROVE.** At audited HEAD `98be20aa`, todo 179 preserves the six closures F2 confirmed at `647a2d64`. The new generated-arrival guard is materially bound to compile-time consumers, fails closed when generation degrades to an empty enum, and introduces no admissible regression. The `compat_v1.rs` change keeps the recorded-payload adapter path and surface path sourced from the same generated `ProviderList` constant.

Follow-up: none.
