# F4 Scope Fidelity Review — Final Verification Wave 7

## Verdict: REJECT

Audited HEAD `0e1fe93b354e12705e6a83ba28f09259307e6053` (`0e1fe93b`). Todos 143–144 are focused remediation of defects already inside the promised plugin surface, the corrected 144-row ledger is honest, the tracked-`target/` cleanup is an appropriate repository-integrity gate, and the current narrowing expressly permits the ten frozen `/api` backends that still answer `503 backend_unavailable`. Wave 6's public divergence-count defect is closed, and success criterion 6 now names the real plain `models` command.

F4 still cannot approve the artifact. The checked task contract continues to promise a nonexistent `models --format json` surface in two places, one of the 17 declared divergences describes provider-family coverage that the production turn composition does not supply, and several executable differential tests bypass the centrally pinned newest release by hard-coding the old 1.18.12 installation path. In addition, the mandatory workspace test gate did not complete successfully after its one permitted retry because the host exhausted process resources.

## Blocking findings

### 1. The final criterion was corrected, but two checked task contracts still promise `models --format json`

Success criterion 6 now correctly states that released 1.18.15 has no `--format` flag and requires visibility through plain `models` (`.omo/plans/opencode-rust.md:1372`). That closes the exact final-criterion wording called out in Wave 6. The same authoritative plan nevertheless still labels todo 26 as expecting parity with `opencode models --format json` and requires that literal command in its checked acceptance criterion (`.omo/plans/opencode-rust.md:374,378`). Todo 60's checked QA scenario also still says `models --format json` (`.omo/plans/opencode-rust.md:686`).

Those are not merely old prose in a changelog: they are current checked implementation requirements. A completed ledger cannot simultaneously say that an upstream command does not exist and claim two completed items were accepted through it. The production implementation and todo 139/143 evidence use the correct surfaces; the remaining defect is contract honesty, not missing product code.

**Required to satisfy:** amend todo 26's title and executable acceptance criterion and todo 60's QA scenario to use the real plain `models`/`models --verbose` surfaces, while preserving an explicit note that the former `--format json` requirement was invalid rather than silently rewriting history.

### 2. `provider-coverage-by-wire-family` relabels an unwired production surface as an intentional divergence

`docs/divergences.toml:101-104` says provider coverage is stated per wire-protocol family and explains why SigV4/EventStream, Gemini/Vertex, and OpenAI-compatible traffic need different request builders. That would be a reasoned behavioral choice if the production composition selected the corresponding implemented family. It does not: `TurnRuntime::open_with_interrupt` creates one `ProviderRegistry` and registers only `oc_provider_compatible::factory` (`crates/oc-cli/src/cmd/turn.rs:482-489`). `supports_compatible_transport` admits only `@ai-sdk/openai-compatible`, `@ai-sdk/openai`, and `@openrouter/ai-sdk-provider` (`crates/oc-cli/src/cmd/turn.rs:846-851`). The Anthropic, Bedrock, Google/Gemini, and Vertex wire families named by the declaration therefore remain unreachable through this production turn path.

The divergence registry's own rule is explicit: a merely unimplemented surface is a known gap, never a divergence (`docs/divergences.toml:11-14`). The current entry states a design rationale for multiple families but exempts the absence of their production wiring. This is exactly the omission-laundering rule the registry says it prevents.

**Required to satisfy:** either wire each promised provider family into the real turn composition with production-path tests proving selection and request dispatch, or move every unavailable family into an explicit frozen known-gap inventory under an owner-approved scope narrowing. Keep a divergence only for behavior that is intentionally different after both sides are implemented.

### 3. Active differential tests bypass the newest-release oracle pin

The central oracle correctly declares `PINNED_RELEASE = "1.18.15"`, discovers its executable without pinning a package-manager path, and refuses a binary that self-reports another version (`crates/oc-testkit/src/oracle.rs:58-79`). This matches success criterion 1's amendment to use the latest installed release (`.omo/plans/opencode-rust.md:1365`). Several active differential tests still hard-code `/config/.local/share/mise/installs/opencode/1.18.12/opencode`, including `crates/oc-tools/tests/registry.rs`, `crates/oc-tools/tests/search_differential.rs`, `crates/oc-lsp/tests/live_servers.rs`, `crates/oc-db/tests/{schema,session,message_export}.rs`, `crates/oc-cli/tests/{differential,rollback}.rs`, and `crates/oc-llm/tests/catalog_differential.rs`.

Historical comments and a fixture whose bytes were explicitly proven unchanged are legitimate. Executable tests that select a different release by absolute path are not: they can measure 1.18.12 while the report attributes current compatibility to 1.18.15, or silently skip when that machine-specific path is absent. That weakens the standing newest-release constraint and recreates the version-attribution seam the centralized oracle was introduced to close.

**Required to satisfy:** route every active installed-binary differential through `Oracle::discover_pinned` (or an equivalent helper that verifies 1.18.15) and remove package-manager-specific executable paths. Retain versioned historical fixtures only where their provenance and byte-equivalence to the current pin are executable assertions.

## Wave 6 closure audit

1. **Public divergence count: closed.** `docs/divergences.md:3` says “Seventeen,” explains the historical thirteen-to-seventeen progression, and states that the headline is checked against `DECLARED_COUNT`. The TOML registry and generated details both contain 17 entries.
2. **Invalid final success criterion: partially closed.** Criterion 6 is corrected to plain `models`, but blocker 1 above identifies the remaining checked task-level contradictions.
3. **Ledger count: closed honestly.** Before correction there were 145 checked rows but only 142 unique identifiers because 124, 125, and 129 were duplicated. The current ledger has 144 rows, 144 unique contiguous identifiers `1..144`; the correction is disclosed rather than represented as newly implemented work.

## Scope judgments for the latest work

### Todo 143

**In scope and honestly diagnostic.** Its experiment established that removing antigravity changed zero bytes in the 2,944-line `models --verbose` output, so fixture-backed provider presence could not prove plugin execution. The resulting proof was moved to antigravity's actual observable auth-hook behavior instead of fabricating a model row. Kiro's independently observable contribution remained protected.

### Todo 144

**In scope and necessary.** Production callers now dispatch `Auth` and `Tool` hooks, plugin tools enter the governed registry, and tests cover real execution plus permission enforcement. This closes a seam where dispatcher unit tests were more complete than the user path; it does not add an unrelated plugin system.

### Tracked `target/` cleanup and merge gate

**In scope.** Removing 48,148 accidentally tracked build artifacts and adding `.omo/premerge.sh` protection restores source-review integrity and prevents recurrence. The incident and count are explicitly disclosed, and current `git ls-files target/` is empty.

### Ten `/api` backend gaps

**Not an F4 blocker under the current frozen scope.** Success criterion 4 was explicitly narrowed to require all 58 upstream operations to be registered and invoked, 48 to have compared local backends, and exactly ten named operations to return explicit `503 backend_unavailable` (`.omo/plans/opencode-rust.md:1368`). `compat_suite.rs:1888-1932` freezes both the 48/10 accounting and every gap by method/path. Treating those ten as missing from the original broad ambition would ignore the owner's current falsifiable narrowing.

## Divergence audit

Sixteen declarations are real, reasoned behavioral choices with live or source-backed boundaries: session ordering, attributable tool-output names, deferred directory creation, split compatibility identity, the structured `execute` contract, C8 endpoints, cross-session memory, literal applied subpaths, `CONTEXT.md` exclusion, malformed-auth refusal, formatter rollback, non-pure generated trees, plain CLI presentation, causal diagnostics, session-list output shape, and absolute non-VCS plan globs.

The seventeenth, `provider-coverage-by-wire-family`, fails the registry's decision-versus-gap rule for the reason in blocker 2. The count itself is correct; the classification is not.

## Standing requested properties

- **No first-party `unsafe`: satisfied.** Workspace policy and the release-surface scanner cover the first-party crate roster.
- **Rust plugin without JavaScript: satisfied.** The example Rust plugin registers tools/hooks and uses the conformance suite independently of the JS host.
- **Slim agent design: satisfied.** Built-ins retain negative delegation boundaries, temperature, deny-by-default permissions, output envelopes, model inheritance/override behavior, and no shipping model-id literal in `oc-agent`.
- **Goal behavior: satisfied.** Objective/counters survive two compactions, guarded idle continuation is exactly once, status is system-owned, and objective edits round-trip while status edits are rejected.
- **Cross-session memory and structured `execute`: satisfied.** Memory is integrated and removable through the strict-parity switch; `execute` deliberately follows jcode's structured sub-call shape and declares the model-visible difference.
- **Deprecated config handling: satisfied.** Deprecated keys are rejected except for the three documented upstream-compatible legacy TUI exceptions.

## Verification

- `cargo clippy --workspace --all-targets`: **PASS**.
- `cargo fmt --all --check`: **PASS**.
- `cargo test --workspace --offline`: **INCONCLUSIVE/FAIL (host resource exhaustion)**. The initial run hit a host-transient failure. The one permitted retry progressed through the workspace with all completed test binaries reporting zero assertion failures, then `oc-tools` libtest failed while listing tests with `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`; subsequent `SendError` panics were cascading harness failures. No third run was made. This is not evidence of a product assertion defect, but the mandatory test gate is not green and therefore cannot support approval.
- The prohibited approximately 100-minute memory gate and two-hour soak were not rerun.
- Windows-only containment behavior was not executed on this Linux host.

This review changed no source, tests, plan, documentation, commit, branch, or remote state. `.omo/evidence/F4-REPORT-wave7.md` is the only intended worktree modification.

F4 VERDICT: REJECT
