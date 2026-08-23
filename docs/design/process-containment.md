# Resident process containment

This note records why Zuno uses one resident guard process instead of copying
another harness's process topology. The Codex comparison is pinned to official
`openai/codex` commit `4582c0a498158063760309c48214a0416a81488a`
(2026-08-23); paths below refer to that checkout.

## Codex reference

Codex launches a local MCP server directly as a new process-group leader in
`codex-rs/rmcp-client/src/stdio_server_launcher.rs`. A separately owned process
handle sends `SIGTERM` to the group, schedules `SIGKILL` after a two-second
grace, and repeats that behavior from `Drop`. Windows uses a Job Object, with a
process-handle fallback. `local_stdio_transport.rs` closes stdin, waits three
seconds, and then kills the direct child if it has not exited.

Shell-tool children use a different path. `codex-rs/core/src/spawn.rs` combines
`kill_on_drop` with the Linux parent-death signal implemented in
`codex-rs/utils/pty/src/process_group.rs`.

This is a good zero-helper design for ordinary MCP close and owner drop. At the
pinned revision, however, the local MCP launcher does not install the Linux
parent-death signal. An uncatchable orchestrator death therefore cannot execute
its process-handle `Drop`, and the MCP process group has no surviving owner that
can perform delayed group cleanup.

## Zuno decision

Zuno keeps exactly one guard for every resident external payload:

```text
zuno owner -> zuno guard -> payload process group -> descendants
```

The extra process buys an explicit owner that survives abrupt parent death long
enough to settle the payload group. On Linux the guard blocks on `SIGCHLD`,
`SIGTERM`, `SIGINT`, and `SIGHUP`; it does not poll. `PR_SET_PDEATHSIG` converts
parent death into the same cleanup path. Other Unix platforms use a 250 ms
parent-liveness fallback, and Windows retains Job Object containment.

The prior `guard -> monitor -> payload` resident topology was rejected because
the second Zuno process added memory and idle wakeups without adding a distinct
ownership boundary. The foreground-editor path remains separate because it must
transfer and restore terminal foreground process-group ownership.

## Required invariants

- A resident payload has exactly one Zuno guard.
- Owners request catchable guard shutdown and then reap it; they do not
  immediately hard-kill the guard.
- Natural payload exit, explicit cancellation, and Linux parent `SIGKILL` all
  settle descendants before the guard exits.
- Tool, MCP, LSP, PTY, extension, and product-agent callers share this contract.
- `crates/zuno-process/tests/containment.rs` pins topology, idle waiting, clean
  shutdown, and abrupt-parent cleanup.
