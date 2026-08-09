# F2 Code Quality Review — Final Verification Wave 3

## Decision

**REJECT.** The filesystem, bounded-history, session-mutation, and production-dispatch guards are materially improved and mostly sensitive to the behavior they claim to protect. However, todo 128's permission/question request lifecycle is not connected to the production HTTP turn path. The compatibility matrix and generated documentation classify successful empty-state responses as implemented even though a real HTTP turn cannot publish a request or accept a reply. A second defect makes the route-level PTY expiry assertion pass for the wrong reason.

## Blocking findings

### 1. Permission and question HTTP operations have no production request/reply lifecycle

**Severity: Blocker**

Todo 128 requires the permission and question operations to reuse the SSE or long-poll pattern, and its happy-path QA requires a client to stream a permission request. The current implementation cannot produce that behavior:

- `crates/oc-server/src/api/request.rs:44-65` returns an unconditional empty `data` array for permission requests, question requests, saved permissions, and session questions; deletion unconditionally returns `204`.
- `crates/oc-server/src/api/mod.rs:171-189` still routes every session-scoped permission/question read or reply operation through `unsupported`, which returns `503 backend_unavailable`.
- `crates/oc-server/src/server.rs:148-176` gives `ServerServices` only run, event, maintenance-event, and mutation services. There is no permission/question request broker whose pending state the handlers could read or resolve.
- `crates/oc-cli/src/cmd/serve.rs:33-50` opens every HTTP-driven turn with `HeadlessApproval`; `crates/oc-cli/src/cmd/tool_runtime.rs:142-164` immediately denies every permission ask. The only real request broker is TUI-local (`crates/oc-cli/src/cmd/tui_permission.rs:69-149`) and is not shared with the server.

This is observable production behavior, not an architectural preference: an HTTP prompt that reaches an `ask` rule is denied immediately, `GET /api/permission/request` remains empty, and there is no successful reply route that could unblock the turn. Question requests have the same missing server-side bridge.

The tests and generated status conceal the gap:

- `crates/oc-server/tests/api.rs:575-589` only proves that empty-state endpoints return empty arrays/`204`.
- `crates/oc-testkit/tests/compat_suite.rs:917-1003` compares status and normalized body only in an isolated empty state and explicitly exempts process-local side effects.
- `crates/oc-cli/tests/docs.rs:389-450` treats every response other than `503` or `501` as implemented, so `docs/compatibility-matrix.md:157-169,191` labels the fixed-empty handlers as implemented.

**Required resolution:** add a process-local permission/question broker to the server services; inject collaborators backed by that broker into HTTP-driven turns; expose pending requests through the declared list/stream operations; implement reply/reject resolution with session/request identity checks and fail-closed disconnect handling; and add a production-path test in which an HTTP prompt parks on a real permission or question request, a client observes it, replies, and the same turn resumes. Until that exists, these operations must be reported as explicit gaps rather than implemented backends.

### 2. The route-level expired PTY ticket test rejects on scope mismatch, not expiry

**Severity: Major test defect**

Production mint and connect code binds tickets to `TicketScope { pty_id, directory: Some(resolved_directory), workspace_id }` (`crates/oc-server/src/api/pty.rs:108-163`). The expired fixture instead issues `TicketScope::for_session`, whose directory is `None` (`crates/oc-server/tests/api.rs:994-1007`; `crates/oc-pty/src/ticket.rs:61-70`). The route redeems it using `Some("/repo")`, so it returns `403` even if expiry is completely disabled.

Mutation evidence was decisive: replacing `prune_expired` with a no-op left `api_pty_connect_requires_a_single_use_unexpired_ticket_without_echoing_it` green, while `ticket::tests::an_expired_ticket_is_rejected_and_pruned` failed. The mutation was then restored. The library expiry rule is correct, but the acceptance-level route test is a false positive.

**Required resolution:** issue the expired fixture with the exact production route scope (including `directory: Some("/repo")` and the matching workspace value), or inject a controllable ticket clock and mint through `connect-token`. Keep the store-level test as the independent unit guard.

## Reviewed areas that passed

- **Filesystem containment is production-sensitive.** `Sandbox::resolve` performs lexical and canonical containment. Temporarily removing only the canonical check made `api_fs_read_refuses_every_shape_of_escape_from_the_session_directory` fail with `200 OK` and the marker `OUT-OF-ROOT-SECRET` through the escaping symlink. The check was restored exactly.
- **History is bounded consistently with the current oracle.** The Rust route rejects limits above 100. A live probe against the installed OpenCode 1.18.15 binary returned `200` for `limit=100` and `400` for `101`, `150`, `200`, and `201`; therefore raising the cap to 200 would be a regression, not a fix.
- **Todo 129 uses the real turn composition.** Production `serve` creates `TurnHost` and calls `drive_with_message_id`; the CLI integration test compares HTTP and CLI assistant output and persisted rows against the same recorded provider bytes. Interrupt and wait are exercised through the production binary.
- **No new session-sized resident capture was found.** The prompt task owns only its single request, bounded event channel, executor/fanout handles, and `SessionRunGuard`; dropping the guard removes the active entry and wakes waiters (`crates/oc-server/src/api/session.rs:637-691`, `crates/oc-engine/src/status.rs:236-247,327-334`).
- **`compact` and `wait` exemptions are explicit rather than false parity.** The isolated upstream fixture cannot prove them because it returns `503` without a provider. Dedicated server tests cover compact dispatch and non-polling wait, and production CLI tests cover wait completion and interruption. This is an acceptable limitation of that differential fixture.
- **Production CLI routing and generated documentation guards are non-vacuous for their stated scopes.** Every implemented command is driven through the shipped binary with handler-specific evidence, and the docs gate derives route tables from the served OpenAPI/router. The docs classifier itself needs the semantic correction described in finding 1.

## Validation and constraints

- Reviewed HEAD: `8628937ab3ee79b8208a6b5610837cc26ac93ce2` on branch `task-F2`.
- The frozen performance files were not edited.
- The approximately 100-minute G1/G2 memory gate and the two-hour soak were intentionally not rerun, per instruction.
- Temporary mutations to PTY expiry and filesystem canonical containment were restored before final validation.
- No commit, push, or merge was performed.

F2 VERDICT: REJECT
