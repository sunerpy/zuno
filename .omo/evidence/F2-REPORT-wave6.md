# F2 Code-Quality Review — Sixth Wave (Todos 138–142)

## Review scope

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Audited HEAD: `b753fb950e5376f6f93e51024e3539900bc544ce`
- Method: targeted source audit, mutation testing, restoration checks, and clean-state validation
- Frozen performance files were not touched.
- No commit, push, or merge was performed.
- Every temporary mutation described below was restored before final validation.

## Verdict summary

Todos 138, 140, 141, and 142 have meaningful failure-sensitive coverage for the reviewed claims. Todo 139 does not: its end-to-end `models` test can remain green after the antigravity plugin is removed from the configured plugin list because the pinned catalog independently preloads the same `google/gemini-test` provider/model that the assertion attributes to antigravity. This is a false-positive test fixture and leaves the antigravity-to-plain-`models` production path unproved.

That surviving non-equivalent mutation is blocking because the test's stated purpose is specifically to prove that both real auth plugins contribute providers through the production CLI surface.

## Blocking finding

### F2-B1 — `plugin_models` cannot prove antigravity contributes Google

**Location:** `crates/oc-cli/tests/plugin_models.rs:44-64,103-117`

The test writes a pinned catalog that already contains:

```json
"google": {
  "models": {
    "gemini-test": { "id": "gemini-test", "name": "Gemini Test" }
  }
}
```

It then asserts only that plain `models` output contains provider `google`. Removing `supported_spec(ANTIGRAVITY_PACKAGE)` from `OPENCODE_CONFIG_CONTENT` left the test green:

```text
test real_auth_plugin_providers_reach_the_plain_models_surface ... ok
test result: ok. 1 passed; 0 failed
```

The Google row came from the catalog/auth fixture rather than proving antigravity's contribution. The same test did correctly fail when all user-tool-directory JS runtime discovery was removed, because `kiro-auth` disappeared while catalog-backed Google remained. Consequently, this test protects the kiro-auth/runtime-fallback half but not the antigravity half named in its assertions.

**Required correction:** make the antigravity contribution distinguishable from the base catalog. For example, omit Google from the initial catalog if production behavior permits it, or assert a provider/model/option/metadata contribution that exists only after antigravity executes. Add a negative control that runs the same production CLI setup without antigravity and proves the antigravity-specific evidence is absent.

## Mutation ledger

### Todo 138 — Fresh subject binaries for source-coupled tests

1. **Mutation:** removed the `build_subject()?` call from `Subject::discover_or_build`.
   - **Expected regression:** a pre-existing workspace binary can be reused after source changes.
   - **Result:** caught by `subject_freshness`; the stale-binary assertion failed.
   - **Restored:** yes; the file matched HEAD afterward.
2. **Clean-state result:** `OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test subject_freshness --offline` passed (`2 passed`).

**Assessment:** adequate. The test detects the exact stale-subject failure mode rather than merely checking that some executable exists.

### Todo 139 — Real JS auth plugins reach plain `models`

1. **Mutation:** restricted JS runtime discovery to `PATH`, removing fallback discovery in user tool directories.
   - **Expected regression:** the real cached plugins cannot execute under the test's intentionally minimal `PATH`.
   - **Result:** caught. `kiro-auth` disappeared and the test failed; Google remained from the catalog fixture.
   - **Restored:** yes.
2. **Mutation:** removed antigravity from the configured plugin list while leaving kiro-auth configured.
   - **Expected regression:** the asserted antigravity-backed Google contribution should disappear.
   - **Result:** **survived**. The test still passed because the pinned catalog already supplied `google/gemini-test`.
   - **Restored:** yes.
3. **Clean-state result:** `cargo test -p oc-cli --test plugin_models --offline` passed (`1 passed`).

**Assessment:** blocking false-positive fixture. Kiro/runtime discovery is protected; antigravity contribution is not.

### Todo 140 — Turn-part gap classification and generated documentation

1. **Mutation:** added a new `KnownGap` entry without regenerating the compatibility matrix.
   - **Expected regression:** executable known-gap inventory and generated docs diverge.
   - **Result:** caught by `docs_compatibility_matrix_matches_every_code_table`.
   - **Restored:** yes.
2. **Behavior witness:** `the_recorded_turn_part_gap_matches_what_a_turn_actually_persists` passed and observed a real assistant turn containing `text` but no `step-start`/`step-finish` parts.
3. **Source audit:** `StreamProjector` can model upstream step parts, but the production checkpoint path writes text, reasoning, and tool parts only; no production caller wires the projector into that path. Treating this as an unwired known gap, rather than an intentional divergence, is consistent with the implementation.

**Assessment:** adequate. Both stale generated documentation and stale behavioral classification are guarded.

### Todo 141 — Durable-before-live events and fail-closed questions

1. **Mutation:** retained event sequence allocation but removed the event-row insert before live fanout.
   - **Expected regression:** SSE exposes an event that `/history` cannot replay.
   - **Result:** caught by `session_sse_never_outpaces_the_history_route`; history was empty for observed sequence `0`.
   - **Restored:** yes.
2. **Mutation:** removed claim/drop cleanup when an owned question reply body fails to parse.
   - **Expected regression:** the question asker remains blocked and the pending request remains live.
   - **Result:** caught by `malformed_owned_question_reply_rejects_and_removes_the_request` via timeout.
   - **Restored:** yes.
3. **Mutation:** removed `spawn_question_watchdog` from `ask_question`.
   - **Expected regression:** an unobserved question has no finite fail-closed deadline.
   - **Result:** caught by `question_without_an_observer_is_rejected_by_the_deadline` via timeout.
   - **Restored:** yes.
4. **Equivalent mutation:** removed only the explicit `QuestionDecision::Rejected` send during observer cleanup.
   - **Result:** survived for a valid reason: removing the pending entry drops the oneshot sender, and `receiver.await.unwrap_or(QuestionDecision::Rejected)` still rejects. This did not weaken behavior.
   - **Restored:** yes.
5. **Non-equivalent replacement mutation:** prevented observer cleanup from collecting/removing pending questions for the session.
   - **Expected regression:** dropping the final observer neither releases the asker nor removes the question.
   - **Result:** caught by `dropping_the_only_session_observer_rejects_a_question` via timeout.
   - **Restored:** yes.

**Assessment:** adequate. The three fail-closed routes—malformed owned reply, deadline, and last-observer disconnect—are independently failure-sensitive, and durable-before-live ordering is tested through public SSE/history routes.

### Todo 142 — Two-sided documented diagnostics witnesses

1. **Mutation:** removed `run` from `DIAGNOSTICS_SURFACES` while retaining its `DocumentedDiagnostics` witness.
   - **Expected regression:** a two-sided diagnostic exemption exists without being declared by the surface inventory.
   - **Result:** caught by `every_declared_diagnostics_surface_carries_a_two_sided_witness`; witnessed and declared sets differed.
   - **Restored:** yes.
2. **Mutation:** changed the production empty-message diagnostic from `a message is required` to `message missing`.
   - **Expected regression:** the subject no longer emits the diagnostic text its divergence witness promises.
   - **Result:** caught by `every_exemption_states_a_reason_and_keeps_a_witness` against the running subject and oracle.
   - **Restored:** yes.
3. **Clean-state result:** all ten `cli_parity` tests passed, including live two-process witnesses.

**Assessment:** adequate. Structural coverage prevents undeclared/stale surfaces, and live two-sided assertions prevent diagnostic wording from drifting while a weaker shared-failure check remains green.

## Final validation

After all mutations were restored:

- `cargo check --workspace --all-targets --offline` — passed.
- `OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test subject_freshness --offline` — passed, 2 tests.
- `OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test session_interop --offline the_recorded_turn_part_gap_matches_what_a_turn_actually_persists -- --exact` — passed, 1 test.
- `cargo test -p oc-cli --test plugin_models --offline` — passed, 1 test.
- `cargo test -p oc-server --test events --offline` — passed, 14 tests.
- `cargo test -p oc-server --test api --offline` — passed, 38 tests.
- `cargo test -p oc-cli --test cli_parity --offline` — passed, 10 tests.

The requested `lsp_diagnostics` calls could not attach to this isolated worktree: the tool is bound to `/config/workspace/ProdDir/AI/opencode-rust` and rejected every `/config/workspace/ProdDir/AI/oc-wt/tF2/...` path as outside its request cwd. The workspace-wide all-target `cargo check` above was used as the compiler-diagnostic substitute. No source mutation remained at validation time; this report is the only retained worktree change.

F2 VERDICT: REJECT
