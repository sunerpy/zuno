# F2 Code-Quality Review — Ninth Wave (Todos 150–153)

## Review scope

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Audited HEAD: `c251665ac3b6fda21c276fe6814cf2ab17006a27`
- Method: source/contract audit, production-path tracing, adversarial fixture probes, reversible mutation testing, restoration checks, and workspace validation
- Reviewed Todo 151's shared ordinary-hook truncation boundary, Todo 150's permission-dialog argument plumbing, Todo 152's provider-identity preservation, Todo 153's live-config capture honesty, the three unresolved wave-8 plugin observations, and the opt-in G3/G4 soak gate.
- CodeGraph was unavailable for this isolated worktree because it has no local index. Source was inspected directly.
- No product or test change was retained. No commit, branch, push, merge, or remote operation was performed.
- Every temporary probe described below was restored before validation. This report is the only retained worktree change.

## Verdict summary

Todos 150–153 materially improve the artifact and their principal tests are load-bearing. The TUI permission dialog now receives the real tool arguments; removing the sole `metadata.arguments` producer makes its production-path regression fail. Ordinary JavaScript hook write-back now rejects truncation across every returned argument before selecting or committing any output, closing wave 8's silent-corruption blocker with all-or-nothing semantics. Compatible-provider factory selection now preserves the resolved provider identity, and collapsing that identity back to the generic factory key fails the named-profile regression. The live user-config capture also fails visibly when the configured live path is absent rather than silently accepting a stale fixture.

The project is nevertheless not approvable. Criterion 2 is explicitly and correctly marked **UNMET** in the frozen plan: pure-mode `debug config` still omits the live adjacent agent and command trees and `plugin_origins`. Todo 153 made this failure honest; it did not implement the missing parity. Final approval requires all success criteria to hold, so an honestly disclosed failure remains a failure.

Three plugin-contract defects from wave 8 also remain in production. `experimental.chat.messages.transform` exposes both canonical persisted `parts` and projected `info`, but the engine sends only `info` to the provider; a plugin mutation to `parts[0].text` is accepted and then discarded. `chat.message` exposes a provider-neutral `Message` as `output.message` rather than the upstream-shaped user message whose identity fields already exist in the production `MessageRecord`; a plugin requiring `id`, `sessionID`, `agent`, and `model` fails on the real lifecycle path. Finally, the public `PluginKind::Tui` selector has no production construction path: every configured plugin is created with the default server kind, including turns launched from the TUI.

The required full workspace test gate also could not be completed on this host. Two attempts reached different crates and then failed inside libtest while creating/listing test threads with OS error 11 (`Resource temporarily unavailable`). At diagnosis time the shared host had approximately 1,034 processes and 7,200 threads. No product assertion failed in either attempt, but an interrupted gate cannot be reported as passing.

## Blocking findings

### F2-B1 — Success criterion 2 is honestly measured and still unmet

**Locations:**

- `.omo/plans/opencode-rust.md:1404-1405`
- `crates/oc-config/tests/differential.rs`
- `crates/oc-config/tests/fixtures/user-config.json`

Todo 153 correctly repaired the evidence chain: the committed capture now has a byte-for-byte guard against `/config/.config/opencode/opencode.json`, and a missing live path fails loudly. The resulting measurement shows the actual remaining defect rather than hiding it. In pure mode, released `opencode` discovers nine adjacent agent files and two adjacent command files and emits `plugin_origins`; this port emits empty `agent` and `command` objects and omits that metadata. The plan records 266,233 canonical upstream bytes versus 25,581 Rust bytes and explicitly labels criterion 2 **UNMET**.

This is not a criticism of Todo 153's implementation; its honesty guard is correct and mutation-sensitive. It is a final-verification blocker because criterion 18 requires F1–F4 approval only after the executable success criteria hold. Disclosure prevents a false green but cannot turn missing config discovery into parity.

**Closure conditions:**

1. Discover the real adjacent `agent/` and `command/` trees under the same pure-mode precedence and working-directory rules as the pinned oracle.
2. Emit the required origin metadata or establish an owner-approved, executable narrowing that does not absorb unrelated future differences.
3. Re-run the live same-cwd differential and make the normalized output byte-identical.
4. Keep Todo 153's fixture-to-live drift guard and absent-file failure intact.

### F2-B2 — `experimental.chat.messages.transform` accepts canonical `parts` mutations and then discards them

**Locations:**

- `crates/oc-plugin/src/payload.rs:148-159`
- `crates/oc-cli/src/cmd/plugin_runtime.rs:288-315`
- `crates/oc-engine/src/loop.rs:689-700,1619-1635`
- `crates/oc-cli/tests/tool_turn.rs:148-151`

The hook contract deliberately exposes each entry as `{ info, parts }`, and the transport faithfully decodes mutations to both fields. `PluginRuntime::transform_messages` also copies both mutated fields back into `HookMessageWithParts`. The loss occurs at the next production boundary: `run_turn` builds the provider history with `transformed.into_iter().map(|message| message.info)`, dropping every mutated `parts` value.

The lifecycle witness mutates `user.info.content[0].text`, so it exercises only the projected compatibility representation and stays green while the canonical persisted-part representation is dead output. Changing that fixture to mutate `user.parts[0].text` made `ordinary_plugin_lifecycle_hooks_run_through_the_real_binary` fail because the provider request lacked the expected `:messages` suffix. The mutation was restored.

This violates the advertised mutable payload: a hook succeeds, its canonical output is accepted, and the continuation silently ignores it. There is no error telling an author that `parts` is read-only or decorative.

**Closure conditions:**

1. Define one authoritative message representation for this hook and make its mutation drive provider preparation.
2. If both `info` and `parts` remain mutable, reconcile them deterministically and reject conflicting mutations rather than silently preferring one.
3. Add a real-turn regression in which mutating `parts[0].text` changes the captured provider request.
4. Add a conflicting-mutation case so precedence cannot drift implicitly.

### F2-B3 — `chat.message` production output is not the advertised upstream user-message shape

**Locations:**

- `crates/oc-plugin/src/payload.rs:9-31`
- `crates/oc-plugin/src/jsonrpc.rs:967-981,1102-1106`
- `crates/oc-cli/src/cmd/turn.rs:1610-1687`
- `crates/oc-cli/tests/tool_turn.rs:119-121`

Production first constructs a complete persisted user `MessageRecord` carrying `id`, `sessionID`, `time`, `agent`, and `model`. It then projects only the content into `oc_llm::event::Message` and exposes that reduced `{ role, content }` value as `output.message`; identity and selection are split into hook input, and the mutable message cannot represent the upstream user-message contract.

An adversarial lifecycle fixture that required `output.message.id`, `sessionID`, `agent`, and `model` made `ordinary_plugin_lifecycle_hooks_run_through_the_real_binary` fail with `:missing-chat-message-shape`. The mutation was restored. The current witness mutates only `output.parts[0].text`, proving callback dispatch but not payload compatibility.

The typed Rust payload comment explicitly cites upstream `index.ts:234-243`, so this is not merely an undocumented private simplification. A plugin authored against the upstream message object can load and reach the hook, then fail because required fields are absent.

**Closure conditions:**

1. Expose an upstream-shaped user message, including its identity, session, agent, model, and time fields, or explicitly narrow and document the incompatibility as an owner-approved divergence.
2. Apply permitted message mutations back to the persisted record without allowing cross-session or cross-message identity substitution.
3. Add a real lifecycle test that reads the complete shape and verifies safe mutable fields reach persistence/provider preparation.

### F2-B4 — `PluginKind::Tui` is public and test-constructible but not production-selectable

**Locations:**

- `crates/oc-plugin/src/js/spec.rs:36-58,81-128`
- `crates/oc-cli/src/cmd/plugin_runtime.rs:45-72,643-656`
- `crates/oc-cli/src/cmd/turn.rs:136-173`

`JsPluginSpec` publicly models dual `server` and `tui` entry points, and `with_kind(PluginKind::Tui)` is the sole selector. Production `configured_plugins()` always calls `JsPluginSpec::new`, whose default kind is `Server`. There is no production call to `with_kind` or `PluginKind::Tui` anywhere in first-party crates. The TUI reaches the shared turn composition, which loads the same server-kind runtime with a headless terminal lease; changing the surface label does not change the selected plugin entry point.

This leaves TUI-only package behavior reachable in tests but unreachable for users. It is the same quality class that earlier waves found repeatedly: a transport/type surface is more complete than the production composition root.

**Closure conditions:**

1. At the TUI composition root, construct and lifecycle-manage TUI-kind plugin entry points with an interactive terminal lease.
2. Keep server hooks on the server runtime; do not replace them accidentally when adding the TUI tier.
3. Add a real `tui` entry-point fixture whose observable behavior is absent on headless `run` and present on the TUI path.
4. If TUI plugins are intentionally unsupported, remove the production-capability implication and classify the gap explicitly instead of retaining a dead selector.

## Todo audit

### Todo 150 — TUI permission arguments and answerability

- The dialog's real producer now carries tool arguments through `metadata.arguments` rather than substituting `Value::Null`.
- Mutation: changed the sole producer in `crates/oc-engine/src/dispatch.rs` to `Value::Null`.
- Result: `production_dispatch_arguments_reach_the_rendered_permission_dialog` failed and rendered a blank command detail.
- The mutation was restored. The producer-to-renderer guard is load-bearing.

### Todo 151 — ordinary JavaScript hook truncation

- `invocation_output` scans every returned argument with the shared `bridge::truncated_path` before selecting or applying any output.
- This preserves all-or-nothing behavior for multi-argument hooks and reports an argument-relative pointer.
- Mutation: removed the shared truncation scan.
- Result: `noop_tool_definition_hook_rejects_real_truncated_schema_before_provider_dispatch` failed.
- The mutation was restored. Wave 8's F2-B1 is closed.

### Todo 152 — compatible-provider identity

- Factory selection and provider identity are now separate: the compatible factory is selected by wire family while the `Spec` retains the catalog provider id needed by named profiles.
- The tests start from resolved catalog/config models and explicitly reject unknown transports.
- Mutation: changed `Spec::new(&model.provider_id)` back to `Spec::new(factory_key)`.
- Result: `every_todo_94_identity_reaches_its_profile_from_resolved_config` failed for OpenRouter (`openai-compatible` versus `openrouter`).
- The mutation was restored. The identity guard is load-bearing.

### Todo 153 — live config capture and invalid CLI surface

- The committed capture is guarded against the live configured path byte-for-byte.
- Mutation: changed `LIVE_USER_CONFIG` to a nonexistent path.
- Result: `real_user_config_capture_matches_live_file_byte_for_byte` failed visibly with OS error 2.
- The mutation was restored.
- This closes the stale-capture false green and truthfully reveals that criterion 2 remains unmet; it does not close the underlying config-tree parity defect.

## Long-running gate honesty

The G3/G4 test is deliberately `#[ignore]` because it executes 500 turns over at least two hours with two real language servers, a 50,000-file watcher, a 100 MB PTY stream, a 50 MB tool result, and a real compaction. `README.md` explicitly states that ordinary `cargo test --workspace` does not prove G1–G6 and gives the exact `--ignored --exact` command. This is an honest opt-in non-functional gate, not a hidden skip or a new blocker. It was inspected but not rerun during this code-quality wave.

## Probe and mutation ledger

1. **Shared truncation-boundary mutation:** removed the ordinary-hook truncation scan. The production deep-schema regression failed. Restored.
2. **Canonical parts probe:** changed the lifecycle plugin from `user.info.content[0].text += ":messages"` to `user.parts[0].text += ":messages"`. The real-binary lifecycle test failed because the provider request did not consume the mutation. Restored.
3. **`chat.message` shape probe:** required `output.message.id`, `sessionID`, `agent`, and `model`. The real-binary lifecycle test failed with `:missing-chat-message-shape`. Restored.
4. **Permission metadata mutation:** changed the production `metadata.arguments` producer to `Value::Null`. The rendered-dialog regression failed. Restored.
5. **Provider identity mutation:** constructed the provider spec from `factory_key`. The resolved-profile regression failed for OpenRouter. Restored.
6. **Absent live-config mutation:** pointed the capture guard at a nonexistent file. The named differential test failed visibly. Restored.
7. **TUI-kind construction audit:** no production `PluginKind::Tui` or `with_kind` call exists, so there was no production call site to mutate.

## Final validation

After all temporary mutations were restored:

- Initial restoration check: `git status --short`, `git diff --name-only`, and `git diff --check` were clean at audited HEAD `c251665ac3b6fda21c276fe6814cf2ab17006a27`.
- `cargo clippy --workspace --all-targets --offline` — passed with no warnings or errors.
- `cargo fmt --all --check` — passed.
- `cargo test --workspace --offline` — **not completed** after two permitted attempts:
  - attempt 1 reached `oc-tools` and libtest failed while listing tests with `Os { code: 11, kind: WouldBlock, message: "Resource temporarily unavailable" }`;
  - attempt 2 reached `oc-testkit` and failed at the same libtest thread-creation/listing boundary with OS error 11;
  - no product assertion failure was observed before either interruption;
  - host diagnosis measured approximately 1,034 processes and 7,200 threads. Shared processes belonging to other active sessions were not terminated.
- The required expected aggregate (`3404 passing / 0 failed`) therefore was not independently established in this wave.

## Not independently verified

- The two-hour G3/G4 opt-in soak was not rerun; its implementation and public invocation/disclosure were inspected.
- No external network or new released-binary comparison was run beyond tests that completed before the host resource interruption.
- The TUI-kind defect was established structurally and by absence of any production constructor; no nonexistent call site was fabricated for mutation testing.

F2 VERDICT: REJECT
