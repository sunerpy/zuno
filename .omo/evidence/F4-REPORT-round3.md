# F4 Scope Fidelity Review — Round 3

- Audited HEAD: `ed249af7`
- Scope: ledger entry 6 only
- Verdict: **APPROVE**

## Evidence log

- Pre-probe filesystem check: `/` had 74 GiB available (75% used).
- Initial worktree check: clean.
- CodeGraph was unavailable for this sibling worktree (`No indexed project found`), so I read only the todo 179 files and their direct binding points.
- `crates/oc-plugin-sdk/build.rs` reads the pinned OpenAPI capture, follows transitive component-schema references from each HTTP 200 response, and generates one `GeneratedClientArrival` variant for every operation reaching `Model`, `ModelRef`, `ModelV2Info`, `Provider`, or `ProviderV2Info`.
- `crates/oc-plugin/src/js/projection.rs` matches `JsModelArrival::GeneratedClient(GeneratedClientArrival)` exhaustively with no wildcard. The generated `ALL` array is diagnostic/test support; it is not the enforcement mechanism.
- The existing Model/Provider registrations are connected to generated variants: v1 `/provider` uses `ProviderList.path()`, and v2 `/api/model`, `/api/provider`, and `/api/provider/{providerID}` use `V2ModelList`, `V2ProviderList`, and `V2ProviderGet` paths.
- Capture provenance is executable: `compat_suite.rs::the_committed_openapi_capture_is_what_the_pinned_release_serves` fetches `/doc` from `oc_testkit::PINNED_RELEASE` and requires byte identity with the committed capture. `no_pinned_oracle_paths.rs` permits the historical filename on that explicit basis.
- Independent compile-fail probe: after a successful backup, I injected `GET /__f4r3/probe` with operationId `f4r3.probe` and a 200 response referencing `#/components/schemas/Model`, then ran `CARGO_TARGET_DIR=/tmp/opencode/f4r3 cargo build -p oc-plugin --offline`.
- The build failed at `projection.rs:49` with rustc `E0004`: `JsModelArrival::GeneratedClient(GeneratedClientArrival::F4r3Probe)` was not covered. This is the required compiler obligation, produced by the generated enum and the wildcard-free projection match.
- I restored the capture from the pre-write backup. `cmp` succeeded, its restored SHA-256 was `c3a9f94af0c3324d97b482b14c692e810ce7ccac3136319ba46334de972b4cf1`, and `git status --porcelain` was empty.
- Workspace test attempt 1 hit the documented host-transient failure in `oc-config`: `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }` while listing tests. This was an `EAGAIN`, followed by secondary `SendError` panics; it was not a product assertion failure.
- The single permitted constrained retry, `CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=/tmp/opencode/f4r3 cargo test --workspace --offline --quiet -- --test-threads=1`, completed the full workspace and doctest run with the expected 3473-test baseline and 0 failures.
- `CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=/tmp/opencode/f4r3 cargo clippy --workspace --all-targets --offline` completed with 0 warnings.
- `cargo fmt --all --check` completed with no output.
- The LSP service rejected sibling-worktree paths, so I SHA-256-compared the six todo 179 Rust files with the main checkout first; all six were byte-identical. `lsp_diagnostics` on those identical files reported no diagnostics.

## Durability judgment

Entry 6 is closed. A future generated-client contract update that adds a Model/Provider-bearing arrival to the pinned OpenAPI input creates a new `GeneratedClientArrival` variant before `oc-plugin` is compiled. Because `JsModelArrival::projection()` exhaustively matches that generated enum without a wildcard, the new arrival is a compile error until explicitly projected. My independent `F4r3Probe` reproduced that exact causal chain.

The historical `1.18.12` capture name is not a bypass. The compatibility suite compares the committed bytes with `/doc` served by the currently pinned release, so a stale capture cannot remain green after a pin change. The current v1 and v2 Model/Provider registrations also consume the generated operation paths rather than disconnected literals. A purely local route absent from the generated-client OpenAPI contract is not a generated SDK arrival; once such a route is added to that contract, the generated variant and compiler obligation apply. I found no concrete in-scope bypass and no admissible regression introduced by todo 179.

## Verdict

**APPROVE. Ledger entry 6 is closed at audited HEAD `ed249af7`.** Todo 179 satisfies the frozen criterion that a new unprojected generated-client Model/Provider arrival is a compile error. This round produced no new threshold-passing Blocker.

## Follow-up (non-blocking)

- The previously recorded unguarded legacy `release_date` restoration in `projection.rs` remains a Follow-up only. It was already ruled non-blocking in Round 2 and was not re-audited here.
