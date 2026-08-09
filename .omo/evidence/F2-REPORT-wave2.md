# F2 Code Quality Review

## Review baseline

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Branch: `task-F2`
- Reviewed HEAD: `3d68d7a`
- Scope: remediation tasks 115–123, including the five blockers from the previous F2 report and a search for an additional production defect or vacuous regression test.
- CodeGraph was not initialized in this worktree, so source files were inspected directly.
- The frozen performance surface was not edited. The approximately 100-minute memory gate and the two-hour soak were not rerun, as instructed.
- Native Windows execution was unavailable on this Linux host.

## Verdict summary

The five prior F2 blockers are remediated. Their focused tests are connected to the relevant production behavior, and mutation checks reproduced the old failures. However, task 116 introduced a structural command-handler guard whose central claim is false: the guard inspects parsed `DispatchArguments`, not the production `HeadlessCommandDispatcher`. A behavior-breaking mutation can route an implemented command to `PendingCommandDispatcher` while both advertised handler guards remain green. This is a release-blocking vacuous test because it recreates the exact class of defect task 116 claims can no longer pass.

## Blocking finding

### 1. The implemented-command handler guard never exercises the production dispatcher

`crates/oc-cli/tests/surface.rs:145-153` defines `dispatch_request`, which stops after parsing the CLI and constructing a `DispatchRequest`. The two claimed structural guards then inspect only `request.args.is_pending()` and the static `PENDING_COMMANDS` roster:

- `surface_no_implemented_disposition_routes_to_the_pending_handler` (`surface.rs:167-193`)
- `surface_every_implemented_command_actually_has_a_handler` (`surface.rs:198-217`)

Neither test invokes `crates/oc-cli/src/cmd/mod.rs:35-66`, where the production `HeadlessCommandDispatcher` chooses the actual handler. `DispatchArguments::is_pending()` proves only that parsing produced a non-`Pending` enum variant; it cannot prove that the corresponding production match arm calls the correct handler.

The gap was demonstrated with a production mutation:

```rust
// crates/oc-cli/src/cmd/mod.rs
DispatchArguments::Agent(_) => PendingCommandDispatcher.dispatch(request),
```

With that mutation in place:

1. `surface_no_implemented_disposition_routes_to_the_pending_handler` passed.
2. `surface_every_implemented_command_actually_has_a_handler` passed.
3. The real binary's `agent list` command exited 1 and reported:

   ```text
   `agent` is registered, but its handler is pending todo 57
   ```

The mutation was then restored. This is not a naming or coverage preference: the tests remain green while an implemented command is routed to the pending handler in production. It is the same failure class as the original `export` defect.

The fix should make the invariant observable at the production dispatch seam. Suitable options include executing every implemented probe through the real top-level dispatcher with isolated fixtures, or factoring the exhaustive routing decision into a side-effect-free production function that the dispatcher and guard both call. Merely adding more `IMPLEMENTED_PROBES` or asserting `is_pending()` again will not close this gap.

## Re-audit of the five previous blockers

### 1. Production turn-event backpressure — remediated

`engine_turn_events_apply_backpressure` now constructs `oc_engine::event_channel()` and publishes through `TurnEventSender::publish`. Temporarily changing `TurnEventSender::send` to construct and drop the send future caused the named gate to fail at the production-channel blocking assertion. The mutation was restored. This closes the prior toy-channel gap.

### 2. Truncated non-2xx provider response bodies — remediated

The compatible transport now propagates `response.bytes().await` failures as retryable transport errors. Temporarily restoring `unwrap_or_default()` caused `a_truncated_error_body_surfaces_the_read_failure_as_transient` to fail: it observed an empty-body `Fatal` classification instead of the reqwest body-read cause. The mutation was restored.

### 3. Unix interactive PTY foreground semantics — remediated

The Unix guard now performs the stop/foreground-transfer/resume handshake before exec. Temporarily removing `tcsetpgrp` caused both `guarded_pty_payload_can_read_from_the_terminal` and `terminal_ctrl_c_reaches_the_guarded_payload` to fail, while the two process-tree reaping tests continued to pass. The mutation was restored.

### 4. Windows descendants after top-level exit — remediated in source; native test not executed

The Windows supervision path now preserves the top-level status, calls `start_kill()` on the Job Object, and waits for job completion before returning. The cfg-windows regression test covers a naturally exiting top-level process with a live descendant. Source review confirms the prior early-return leak is closed. Runtime verification remains limited because this review host is Linux; this limitation is reported rather than presented as a passing Windows run.

### 5. Lint suppressions without local reasons — remediated

First-party `allow` attributes now carry local `reason` fields, with the frozen memory-harness exception pinned explicitly by the release-surface policy test. Removing the reason from `crates/oc-tool/src/schema.rs` caused `every_first_party_lint_suppression_has_a_reason` to fail and identify the exact file and line. The mutation was restored.

## Tasks 115–123 review notes

- **115 — session model shape:** the session writer uses the upstream `{id, providerID}` shape while message records retain `{modelID, providerID}`. The helper centralizes the otherwise opaque JSON column, and binary/database compatibility tests cross the real rollback boundary.
- **116 — export/import:** export, sanitize, and import production behavior is implemented and has differential and round-trip coverage. The blocker is limited to the advertised all-command structural handler guard, not the export/import data path itself.
- **117 — live configuration:** exactly `theme`, `keybinds`, and `tui` are ignored before strict top-level validation; near-miss keys remain rejected. The exemption is bounded and does not widen unknown-key acceptance generally.
- **118 — SSE operations:** the global and per-session routes share the production `EventService`; tests cover connection, publication, durable replay, cursor ordering, legacy sequence-zero omission, reconnect behavior, lag diagnostics, and concurrent subscribers.
- **119 — divergence and crate rosters:** the formerly inverted divergence assertion now fails when a nominated behavior is undeclared. The workspace roster is compared bidirectionally with `cargo metadata`, rather than guarded only by a minimum count.
- **120 — backpressure and provider errors:** both previous defects have direct production-path sensitivity tests, confirmed by mutation during this review.
- **121 — process containment:** Linux PTY and reaping behavior is covered by real process tests. The Windows implementation was reviewed statically but could not be executed here.
- **122–123 — memory regression:** the owned compaction projection now estimates the complete provider-visible message before reducing one message at a time, preventing aggregate full tool outputs from coexisting. The frozen gate's committed task-123 evidence reports a passing median and all five W-real repetitions below the ceiling; this review did not rerun the prohibited long gate.

## Compaction-tail investigation

`transcript_owned` applies `summary_safe_message_owned` before `run_compaction` selects and returns its retained tail, so the returned `CompactedTranscript.messages` can contain truncated tool results or image placeholders. This initially appeared to contradict the documented “verbatim tail.” It is not promoted to a production blocker for the current prelude path:

1. `compact_if_overflowing` consumes only whether the outcome is `Compacted`; it does not send `CompactedTranscript.messages` to the next turn.
2. Successful compaction persists `tail_start_id` and the summary.
3. The next production turn calls `hydrate_retained_history`, which hydrates the original stored suffix from SQLite after `tail_start_id`.
4. Ordinary provider projection therefore receives the complete retained tool and image parts.

The direct `CompactedTranscript.messages` consumer in `oc-memory` is a compaction/memory preservation test helper, not the production turn entry point. Focused compaction tests pass and preserve full token charging before summary-safe reduction. The API's naming/documentation could be clearer, but no user-visible data loss was established.

## Mutation journal

Every mutation was restored before closeout:

| Mutation | Expected detector | Result |
|---|---|---|
| Drop the awaited production turn-event send | Production backpressure gate | Failed as expected |
| Restore non-2xx body `unwrap_or_default()` | Truncated-body transport test | Failed as expected |
| Remove Unix PTY `tcsetpgrp` transfer | PTY read and Ctrl-C tests | Both failed as expected |
| Remove one lint-suppression reason | Release-surface lint policy | Failed as expected and named the source location |
| Route `DispatchArguments::Agent` to `PendingCommandDispatcher` | Implemented-handler structural guards | **Both unexpectedly passed**; real `agent list` failed |
| Restore aggregate-first compaction projection | Owned compaction invariant test | Test remained green for token charge/reduction semantics; not used as blocker evidence |

Temporary probes and all production mutations were removed or reversed.

## Verification

Focused checks run during the review passed on restored source, including:

- production event-channel backpressure
- truncated non-2xx body-read propagation
- all four Linux containment tests
- lint-suppression reason policy
- owned compaction token-charge/reduction invariant
- overflowing-session compaction behavior

The final targeted test/build chain and `git diff --check` passed. The MCP `lsp_diagnostics` endpoint rejected `/config/workspace/ProdDir/AI/oc-wt/tF2/F2-REPORT.md` because it is rooted at the main checkout; the report is the only changed file and Markdown has no configured source-language diagnostic pass. No Rust source file is changed. The long memory gate and soak were intentionally not rerun; native Windows execution was unavailable.

F2 VERDICT: REJECT
