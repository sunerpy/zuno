# F2 Code Quality Review

## Review baseline

- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF2`
- Branch: `task-F2`
- HEAD: `70114aa5cbce2946d95abea2c0d6b4e8007d0e6b`
- Initial `git status --porcelain`: clean
- Toolchain: `cargo 1.96.0`, `rustc 1.96.0`, pinned by `rust-toolchain.toml`
- CodeGraph: not initialized in this worktree; direct source reads were used rather than another checkout's index.
- Expensive gates intentionally not run: the 100-minute memory gate and the two-hour soak.

## Mutation journal

Every temporary artifact below was removed or restored before the verdict.

- [x] `crates/oc-cli/src/disposition.rs` — changed one production disposition reason, observed the documentation gate fail, then restored it with the exact reverse patch.
- [x] `crates/oc-engine/src/loop.rs` — replaced awaited event delivery with a dropped send future while retaining the registry's source needle, ran both the named gate and a production-channel probe, then restored it with the exact reverse patch.
- [x] `crates/oc-engine/tests/f2_backpressure_mutant.rs` — temporary production-channel sensitivity probe; deleted after the mutation run.
- [x] `crates/oc-memory/src/snapshot.rs` — altered the memory-absent prompt bytes, observed the byte-identity test fail, then restored it with the exact reverse patch.
- [x] `crates/oc-provider-compatible/tests/f2_truncated_error.rs` — temporary loopback reproduction for a truncated non-2xx response body; deleted after capturing the observed classification.

The frozen perf surface (`crates/oc-testkit/src/perf/**`, `docs/perf-methodology.md`, and `benchmarks/ts-baseline.json`) is read-only under the review instructions and will not be mutated.

## Investigation hypotheses

1. The documentation gate is genuinely code-derived. Distinguishing evidence: a production disposition change makes `oc-cli --test docs` fail on the generated block.
2. The G5 registry is source-derived but its named per-channel behavior tests are disconnected from production. Distinguishing evidence: a behavior-changing `TurnEventSender` mutant preserves the text needles and passes `engine_turn_events_apply_backpressure`, while a direct production-channel probe observes the lost blocking.
3. The `memory: false` identity fixture is sensitive rather than convergent. Distinguishing evidence: changing the absent-memory branch by one byte fails the named integration test, while the enabled-path inequality remains non-empty.
4. The methodology lock is falsifiable without mutating frozen files. Distinguishing evidence: its committed one-byte-drift and unregistered-revision tests pass against the frozen section.

## Blocking findings

### 1. The named engine-turn backpressure test does not exercise the production channel

`crates/oc-testkit/tests/backpressure.rs:342` expands `engine_turn_events_apply_backpressure` through a registry-driven macro. The registry checks source text such as `mpsc::channel(TURN_EVENT_CHANNEL_CAPACITY)` and `self.sender.send(event)`, but the behavior probe itself uses a fresh toy `tokio::mpsc` channel (`probe_blocking_send`) rather than `oc_engine::event_channel()` and `TurnEventSender`.

This was demonstrated by mutation, not inferred from test shape:

1. `TurnEventSender::send` was changed to construct and drop the `self.sender.send(event)` future instead of awaiting it. The source needles remained present.
2. The inventory test and `engine_turn_events_apply_backpressure` both passed.
3. A temporary test using the production `event_channel()` failed because the second send no longer blocked when capacity was exhausted.

The gate can therefore remain green while the advertised production boundary loses backpressure. Replace or supplement the generic policy probe with a test instantiated through the production constructor and sender API.

### 2. A failed non-2xx response-body read is silently converted into a valid empty body

`crates/oc-provider-compatible/src/transport.rs:112` uses:

```rust
let bytes = response.bytes().await.unwrap_or_default();
```

That converts transport failure while reading an error response into successful parsing of zero bytes. A temporary loopback server sent HTTP 400 with a declared body longer than the bytes actually transmitted and then closed the connection. The body read failed, but `ReqwestTransport` returned a non-retryable `ProviderError::Fatal` whose source rendered as ``provider `f2-probe` returned HTTP 400: ``.

This both suppresses the real I/O cause and can change recovery semantics: HTTP 400 bodies are explicitly needed to distinguish `context_length_exceeded` and `content_filter`. Propagate the read failure through `ProviderError::transient` (or preserve it in an equally explicit typed error) instead of defaulting it away, and add a truncated-body transport test.

### 3. Unix containment breaks interactive PTY foreground-process-group semantics

Every guarded Unix payload unconditionally calls `setpgid(None, None)` in `crates/oc-process/src/lib.rs:214`. For ordinary pipes this creates the group that the monitor later kills. For a PTY launch, however, the guard process owns the terminal's foreground process group and the payload is moved into a different, background process group. An interactive payload that reads the controlling terminal is then stopped by `SIGTTIN`.

A real PTY reproduction launched the existing `oc-process-fixture` hidden guard around `/bin/sh -c 'read x; printf "READ:%s\n" "$x"'`, wrote `hello\n`, and waited two seconds. The terminal echoed `hello`, but the shell never emitted `READ:hello` (`PTY_READ_OK=False`). The existing G6 PTY fixture only starts a sleeping child, so it cannot detect this regression.

Containment for PTYs must preserve or deliberately transfer the terminal foreground process group while retaining a separately killable tree. Add a test that reads from the PTY (and ideally verifies terminal-generated interrupt handling), not only one that sleeps.

### 4. Windows containment can release live descendants when the guarded top-level process exits

The Windows path wraps the payload with `process_wrap::std::JobObject` (`crates/oc-process/src/lib.rs:256-269`). In the pinned `process-wrap 9.0.1`, the std wrapper calls `make_job_object(handle, false)`: `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is not enabled. When `child.try_wait()` reports that the top-level process exited, `supervise` immediately returns. Dropping the Job Object then closes its handle without terminating processes still alive in that job.

This differs from the Unix monitor, which explicitly calls `terminate_process_group(child_pid)` when the payload exits. A host that spawns a long-lived descendant and exits can therefore leave the descendant running on Windows. There is no Windows runtime containment test; the current tests are Linux-only. Explicitly terminate/wait the job on top-level exit (or use a kill-on-close wrapper) and add a Windows test for a naturally exiting parent with a live grandchild.

### 5. Four lint suppressions lack the required local justification

The review requirement is that every `#[allow(...)]` explains why the suppression is necessary. These have neither a `reason = ...` nor an adjacent justification:

- `crates/oc-tool/src/schema.rs:203` — `#[allow(dead_code)]`
- `crates/oc-engine/src/compaction.rs:415` — `#[allow(clippy::too_many_arguments)]`
- `crates/oc-server/src/api/maintenance.rs:143` — `#[allow(clippy::too_many_arguments)]`
- `crates/oc-testkit/tests/memory.rs:844` — `#[allow(clippy::too_many_arguments)]`

The suppressions in `oc-tui` do use explicit `reason` fields and are not findings. Add precise reasons to the four listed attributes or refactor the affected APIs.

## Observations

- The documentation gate is genuinely code-derived. Mutating a production disposition reason made `cargo test -p oc-cli --test docs` fail on the generated `cli-disposition` block.
- The `memory: false` control is non-vacuous and byte-sensitive. A one-newline production mutation failed `memory_false_matches_a_real_upstream_control_and_spawns_no_reflection` on byte comparison.
- Retained-history no-op behavior has five passing byte-identical cases in `oc-engine`.
- Endpoint precedence, `apiKey` handling, option exclusion, and credential redaction are covered by focused CLI tests and all passed.
- Guard activation occurs before ordinary CLI parsing. LSP, MCP, PTY, native JSON-RPC plugins, and JS hosts all route launches through `oc_process::guarded_argv`. Their local wait/kill tasks provide lifecycle reporting and zombie collection; they are complementary to tree containment rather than conflicting duplicate supervisors.
- Linux G6 tests exercise clean shutdown and parent `SIGKILL` over multiple real host kinds and passed. They do not cover an interactive PTY payload, natural host exit with a surviving descendant, or Windows behavior.
- The Windows parent-death loop invokes `tasklist` every 10 ms per guarded child. That is unusually expensive process polling and merits replacement with a native process handle wait, but it was not promoted to a blocker without Windows runtime measurement.
- Workspace lint policy forbids unsafe code. No attempted weakening of that policy was found. The only ignored test found was the documented, opt-in two-hour soak.
- The frozen performance methodology files were not modified. The methodology unit tests include byte-drift and unregistered-revision sensitivity checks and passed.

## Mutation results

| Mutation/probe | Expected sensitivity | Observed result |
|---|---|---|
| Change one production CLI disposition reason | Generated docs test must fail | **Failed as expected** with stale `cli-disposition` |
| Drop the production `TurnEventSender::send` future while retaining registry needles | Named production-policy test should fail | **Unexpected pass**: inventory and `engine_turn_events_apply_backpressure` stayed green |
| Call the mutated sender through production `event_channel()` | Second send must remain blocked at capacity | **Failed as expected**, proving the production behavior changed |
| Add an unbounded production channel | Source inventory must reject it | **Failed as expected** |
| Change absent-memory prompt output by one newline | `memory: false` identity test must fail | **Failed as expected** on byte comparison |
| Truncate a loopback HTTP 400 response body mid-read | Body-read I/O failure must remain observable | **Unexpected classification**: empty-body non-retryable `Fatal` |
| Run a guarded shell that reads from a real PTY | Shell must receive terminal input | **Failed**: input echoed but no read completion within two seconds |

All source mutations and temporary test files were removed after their individual runs.

## Verification

Passed baseline and focused checks:

- `cargo test -p oc-cli --test docs` — 1 passed before mutation; intentional mutation failed as expected.
- `OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test backpressure` — 21 passed on restored source.
- `cargo test -p oc-memory --test integration` — 2 passed.
- `OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --lib perf::methodology::tests` — 11 passed.
- `cargo test -p oc-cli --test provider_endpoint` — 8 passed.
- `cargo test -p oc-cli --test provider_options` — 7 passed.
- `cargo test -p oc-cli --lib turn::tests::` — 31 passed.
- `cargo test -p oc-engine --test loop byte_identical` — 5 passed.
- `cargo build -p oc-process --bin oc-process-fixture` — passed before the real-PTY reproduction.

Not run by instruction: the approximately 100-minute memory gate and the two-hour soak. Native Windows behavior could only be source-reviewed on this Linux host; it was not runtime-validated.

## Verdict

**REJECT**

The implementation has meaningful, sensitive tests in several high-risk areas, but approval is blocked by a production backpressure policy test that survives a behavior-breaking mutant, swallowed response-body I/O failures, a reproduced interactive PTY regression, an uncovered Windows descendant leak, and four unjustified lint suppressions.
