# F4 Scope Fidelity Review — Final Verification Wave 8

## Verdict: REJECT

Audited HEAD `2e57e490c84224f44ff3ba8469cf9dd8dfa1b9e8` (`2e57e490`). The implementation ledger now contains exactly 149 checked, unique, contiguous numeric todos (`1..149`). Wave 7's three concrete remediation defects are closed: the nonexistent `models --format json` contract was corrected, installed-binary differential tests now go through the pinned 1.18.15 oracle, and all declared wire-protocol families are registered in the production turn. Todos 147 and 149 also close real corruption and lifecycle-trigger gaps without weakening their contracts. All mandatory workspace gates are green: 3,390 tests passed, Clippy emitted no warnings, and formatting is clean.

F4 still cannot approve the artifact. Todo 94 promises named OpenAI-compatible provider identities and identity-dependent Azure, GitHub Copilot, OpenRouter, Groq, and Mistral behavior. The compatible crate implements those profiles, but the production selector either rejects the catalog transport or replaces the provider identity with the generic string `openai-compatible`. The remaining `provider-coverage-by-wire-family` divergence therefore continues to classify part of a promised, unreachable production surface as an intentional difference even though its own registry forbids laundering an implementation gap into a divergence.

## Blocking finding

### 1. Production selection does not deliver todo 94's named compatible-provider profiles

**Requested scope.** Todo 94 requires one production-capable profile for OpenRouter, xAI, Mistral, Groq, DeepInfra, Cerebras, Cohere, TogetherAI, Perplexity, Vercel, Alibaba, GitLab, Venice, Azure, and GitHub Copilot, including Azure's selector and Copilot's model-dependent endpoint rule. It explicitly requires unknown providers to be rejected rather than silently routed through the compatible profile (`.omo/plans/opencode-rust.md:414-418`). The declaration for the seventeenth divergence similarly says all declared wire families are implemented and that only unknown `api.npm` transports are rejected (`docs/divergences.toml:101-104`).

**Delivered scope.** The compatible crate contains the requested identities: `CLAIMED` includes Azure and Azure Cognitive Services, GitHub Copilot, Groq, Mistral, OpenRouter, xAI, and the other named profiles, with provider-specific surface rules (`crates/oc-provider-compatible/src/family.rs:165-229`). `CompatibleProvider::new` resolves behavior from `Spec::provider`, making that identity semantically necessary rather than descriptive metadata (`crates/oc-provider-compatible/src/provider.rs:55-65,85-96`). Production model selection does not preserve that contract:

1. `provider_key_for_npm` accepts the seven dedicated wire-family transports plus only `@ai-sdk/openai-compatible` and `@openrouter/ai-sdk-provider` for the compatible family; every other npm transport returns `None` (`crates/oc-cli/src/cmd/turn.rs:997-1008`).
2. The pinned catalog contains a real `groq` provider whose transport is `@ai-sdk/groq` and a real `mistral` provider whose transport is `@ai-sdk/mistral` (`crates/oc-llm/tests/fixtures/models-dev-pinned.json:215-220,277-278,402-407,471-472`). Both are promised by todo 94 and claimed by the compatible crate, but both are rejected by the production allow-list before that crate can resolve them.
3. Even accepted compatible transports lose their identity. `model_spec` obtains the generic registry key and constructs `Spec::new(registry_key)` (`crates/oc-cli/src/cmd/turn.rs:1337-1350`). For OpenRouter this turns the actual provider into `openai-compatible`; `family::resolve` consequently selects the generic row instead of the `openrouter` profile whose `routes_upstreams` behavior is different (`crates/oc-provider-compatible/src/family.rs:208-214,276-288`). The same identity collapse makes the Azure and Copilot rules unreachable even if their transport is admitted later.
4. Todo 148's production replay coverage proves the generic compatible transport and the dedicated Anthropic, Bedrock, Gemini/Vertex, and OpenAI wire families. Its compatible case uses `@ai-sdk/openai-compatible`, and the transport table covers only the accepted npm values (`crates/oc-cli/src/cmd/turn_tests.rs:165-175,335-340`). It does not enter production selection as Groq, Mistral, Azure, GitHub Copilot, or an identity-preserving OpenRouter model.

This is not a request to guess an unknown protocol. Groq and Mistral are frozen named members of todo 94 and concrete rows in the pinned catalog, while OpenRouter already has an admitted transport but loses its identity. The divergence registry says a merely unimplemented surface is a known gap, not a divergence (`docs/divergences.toml:11-14`), and the entry itself says implemented families are not being recorded as gaps (`docs/divergences.toml:101-104`). The current checked state therefore overstates delivered scope.

**Required to satisfy:**

1. Separate factory selection from provider identity: select the compatible registry factory by wire family while constructing the `Spec` with the resolved provider identity needed by `family::resolve`.
2. Make every provider identity frozen by todo 94 reachable from the production catalog/config path. For SDK-specific npm values such as `@ai-sdk/groq` and `@ai-sdk/mistral`, either map the explicitly promised identities to the compatible wire family or obtain an owner-approved scope narrowing that moves them to the frozen known-gap inventory. Do not add a catch-all that guesses unknown transports.
3. Add production-path tests that start from resolved catalog/config models and prove identity-preserving selection and dispatch for the promised compatible profiles, including at minimum four distinct providers, an admitted OpenRouter identity, Azure's selector, GitHub Copilot's model-dependent rule, the real Groq and Mistral transport spellings, and an unknown-transport refusal.
4. Keep `provider-coverage-by-wire-family` only after it describes a behavioral difference between implemented surfaces. If any promised identity remains unavailable, remove it from the divergence and record it honestly as an owner-approved known gap instead.

## Wave 7 and latest-todo closure audit

### Todo 145 — closed

The authoritative plan now uses the real plain `models` and `models --verbose` surfaces and explicitly records that released 1.18.15 has no `--format` option (`.omo/plans/opencode-rust.md:1350-1352`). This closes the contradictory checked contracts from todos 26 and 60 rather than silently preserving an impossible acceptance command.

### Todo 146 — closed

Active installed-binary differentials were routed through centralized pinned-oracle discovery. Comparing the ten affected differential files between Wave 7's `0e1fe93b` and audited HEAD found no removed test cases or weakened assertions. The added structural guard rejects hard-coded installed-oracle paths, wrong-version binaries, and package-manager launchers (`crates/oc-testkit/tests/no_pinned_oracle_paths.rs`). Historical fixture provenance remains separate from executable oracle discovery.

### Todo 147 — closed

The JS shim now encodes mutated arguments independently from the depth-zero payload while retaining the bounded maximum depth. The bridge rejects `$truncated` before provider deserialization or write-back. Tests prove byte-identical preservation when no mutation is needed, refusal of lossy plugin output while retaining the original provider, and bounded arbitrary return graphs (`crates/oc-plugin/src/js/shim.mjs`; `crates/oc-plugin/src/js/bridge.rs`). This satisfies the requested “refusal or preservation, never corruption” contract.

### Todo 148 — closed for its stated wire-family remediation

Production registration and replay tests now cover Anthropic, Bedrock, Bedrock Mantle, Google/Gemini, Vertex Gemini, Vertex Anthropic, OpenAI, and generic OpenAI-compatible dispatch. `provider_registry` registers the compatible factory and continues with the dedicated family factories (`crates/oc-cli/src/cmd/turn.rs:1011-1024` and following registrations), while `every_declared_wire_transport_selects_its_production_registry_key` covers the declared transport table (`crates/oc-cli/src/cmd/turn_tests.rs:165-175`). This closes Wave 7 blocker 2 as todo 148 framed it. It does not close the older, narrower todo 94 identity contract described in blocker 1 above.

### Todo 149 — closed

`HookName::ALL` contains all 21 advertised hooks, and `PluginRuntime` dispatches all 21 at real lifecycle boundaries. Consumption reaches the CLI/turn path, `TurnEventSender`, permission and tool dispatch, shell child environment, request preparation, and compaction. Acceptance tests enter through the real binary, real dispatcher, real shell process, and real compaction path (`crates/oc-cli/src/cmd/plugin_runtime.rs`; `crates/oc-cli/tests/tool_turn.rs`). No advertised hook remains a dispatcher-only capability.

## Full ledger and scope assessment

The plan has 149 checked implementation rows, 149 unique numeric identifiers, and no gaps in `1..149`. Todos 145–149 are focused remediation of requirements already in the frozen compatibility target; they do not add unrelated product scope. Review of the complete checked ledger, its standing constraints, known-gap tables, and executable acceptance surfaces found one materially unsatisfied checked contract: todo 94's production reachability for named compatible-provider identities. A checked box and green generic-family tests do not supersede that explicit requirement.

The ten frozen `/api` backend gaps remain non-blocking under the owner-approved scope narrowing: they are registered and invoked but deliberately return explicit `503 backend_unavailable`, while the other 48 operations have compared local backends. They are represented as known gaps rather than divergences and therefore do not violate the decision-versus-gap rule.

## Divergence audit — all 17 entries

Sixteen declarations remain bounded, intentional behavioral choices with live or source-backed witnesses:

1. `session-list-default-sort` — accepted.
2. `tool-output-filename-carries-session` — accepted.
3. `no-eager-directory-creation` — accepted.
4. `split-version-identity` — accepted.
5. `execute-parameter-contract` — accepted; the live schema is asserted.
6. `c8-maintenance-endpoints` — accepted; this is explicit added scope.
7. `provider-coverage-by-wire-family` — **rejected as currently classified** for blocker 1.
8. `cross-session-resident-memory` — accepted; strict parity can disable all three surfaces.
9. `session-subpath-is-applied` — accepted; literal matching is a property of the one declared difference.
10. `context-md-excluded` — accepted.
11. `malformed-auth-json-is-an-error` — accepted as a deliberate data-preservation policy.
12. `failed-format-restores-pre-format-bytes` — accepted.
13. `non-pure-plugin-generated-trees` — accepted within the explicitly narrowed pure-mode parity criterion.
14. `plain-cli-presentation` — accepted; normalization is bounded by negative controls.
15. `diagnostics-name-their-cause` — accepted; exemptions retain two-sided witnesses.
16. `session-list-output-shape` — accepted as a measured content difference.
17. `non-vcs-plan-glob-is-absolute` — accepted.

The registry count and generated documentation are correct. The problem is the seventh entry's classification, not an undeclared eighteenth divergence.

## Standing requested properties

- **No first-party `unsafe`: satisfied.** Workspace lint inheritance and the release-surface scanner cover the first-party crate roster.
- **Rust plugin without JavaScript: satisfied.** The Rust plugin example and conformance path do not depend on the JS host.
- **Slim agent design: satisfied.** Built-ins retain deny-by-default permission boundaries, delegation restrictions, output envelopes, temperature behavior, and model inheritance/override semantics without shipping a model-id literal in `oc-agent`.
- **Goal behavior: satisfied.** Objectives and counters survive compaction, guarded idle continuation is bounded, status is system-owned, and objective edits round-trip while status edits are rejected.
- **Cross-session memory and structured `execute`: satisfied.** Memory is integrated and removable through the strict-parity switch; `execute` intentionally exposes the declared jcode-shaped structured sub-call contract.
- **Deprecated config handling: satisfied.** Deprecated keys are rejected apart from the documented upstream-compatible legacy TUI exceptions.
- **Implement rather than declare: not fully satisfied.** The dedicated provider wire families are now implemented in production, but named compatible identities remain declared in todo 94 and `family::CLAIMED` without complete production reachability.

## Verification

- `cargo test --workspace --offline`: **PASS on the one permitted retry — 3,390 passed, 0 failed, 2 ignored across 211 test/doc-test binaries.** The first attempt failed while listing `oc-config` tests with `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`, exactly the allowed `EAGAIN` host condition; no product assertion failed before it, and no third run was made.
- `cargo clippy --workspace --all-targets --offline`: **PASS — 0 warnings**.
- `cargo fmt --all --check`: **PASS — no diff**.
- The prohibited approximately 100-minute memory gate and two-hour soak were not rerun.
- Windows-only containment behavior was not executed on this Linux host.

This review changed no source, tests, plan, product documentation, commit, branch, or remote state. `.omo/evidence/F4-REPORT-wave8.md` is the only intended worktree modification.

F4 VERDICT: REJECT
