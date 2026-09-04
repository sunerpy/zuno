# Resident process containment

This note records where Zuno uses direct process groups and where it retains a
resident guard. The Codex comparison is pinned to official
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

Local stdio MCP adopts Codex's direct-child topology:

```text
zuno owner -> MCP process-group leader -> descendants
```

Each configured MCP server still requires its own protocol process, but Zuno no
longer inserts any `__zuno_child_guard` process in front of or beside it.
The owner starts the configured command as a process-group leader, closes it with
`SIGTERM`, escalates to `SIGKILL`, and reaps the direct child. Final-handle `Drop`
synchronously kills the group before its detached task reaps the child, so Tokio
runtime shutdown does not leave descendants alive during ordinary owner teardown.

This deliberately matches Codex's zero-helper MCP topology. If the Zuno owner is
itself killed with an uncatchable signal, no destructor can run and direct MCP
descendants may survive. Zuno does not hide that operating-system limit behind an
always-resident helper solely for MCP.

LSP, foreground editor, process extension, product-agent, and shell paths retain
their dedicated guards where terminal ownership or a surviving per-tree owner is
part of the contract. Unix PTYs use the foreground guard too. On Linux those
guards use `PR_SET_PDEATHSIG` and wait on the payload's `pidfd`, so natural exit
is a kernel-reported readiness event rather than a lossy userspace `SIGCHLD`
wake. Termination signals set an atomic shutdown flag; the pidfd wait has a
bounded interval so a signal arriving between the flag check and the blocking
syscall cannot strand the guard. If the kernel or container policy does not
permit `pidfd_open`, the same loop falls back to bounded child and parent checks.
Other Unix platforms use the bounded parent-liveness path.

Windows ConPTY is a separate ownership topology:

```text
zuno PTY owner -> requested ConPTY child -> descendants
```

The requested terminal program is the direct ConPTY child even when the Zuno
guard executable is active. Nesting the resident Windows Job Object guard inside
ConPTY prevented input delivery and kept an already-exited command observable as
running. The PTY owner already has the direct PID, closes the ConPTY writer and
master in exit order, and uses `taskkill /T` through
`request_contained_process_shutdown` for explicit teardown. Other Windows
resident hosts retain Job Object containment. The portable ConPTY backend asks
its host for an inherited cursor position with `ESC[6n` before the child accepts
ordinary input. Zuno consumes that one startup query and answers `ESC[1;1R`;
the control exchange is not retained or forwarded as terminal output.

Shell has an additional security layer before this lifecycle ownership begins.
`zuno-sandbox` compiles raw argv plus immutable authority into
`PreparedCommand`; `zuno-pty` accepts only that prepared value. In `read-only`
and `workspace-write` on Linux, the prepared launch enters bubblewrap and a
first-party seccomp helper before the requested interpreter executes. In
explicit `danger-full-access`, under a trusted `sandbox.backend: native`
selection, and in the eligible `run-unconfined` fallback, the native backend
preserves the requested program and arguments while still attaching durable
authority — which records the requested contract and how the backend was
selected — and using the same process-tree lifecycle. Process containment
therefore owns cancellation, timeouts, output, and restart reconciliation in
every mode. The OS sandbox owns
filesystem, network, capability, and syscall authority only in confined modes;
the native backend does not pretend to provide that boundary.

The prior `guard -> monitor -> payload` resident topology was rejected because
the second Zuno process added memory and idle wakeups without adding a distinct
ownership boundary. The foreground-editor path remains separate because it must
transfer and restore terminal foreground process-group ownership.

## Required invariants

- MCP commands are direct children and never receive a per-server Zuno wrapper.
- Starting local MCP does not create any additional Zuno helper process.
- Owners request catchable guard shutdown and then reap it; they do not
  immediately hard-kill the guard.
- Linux natural-exit observation uses the payload pidfd when available and
  always returns to `wait`/reaping before the guard reports completion.
- Linux pidfd denial or absence falls back to bounded lifecycle checks instead
  of weakening process-tree cleanup.
- Natural MCP exit and explicit cancellation clean its complete process group.
- Windows ConPTY launches the requested program directly rather than nesting a
  Job Object guard inside the pseudoterminal; PTY teardown terminates the direct
  child's complete process tree.
- On Windows, a ConPTY child waiter closes Zuno's input writer and master handle
  after `child.wait()` and before waiting for reader drain. Waiting for EOF while
  either host-side handle remains alive is a circular wait; publishing exit
  before the reader drains can instead lose final output.
- On Windows, the PTY owner answers and removes the backend's one inherited-cursor
  startup query before exposing output. Failure to answer can block `cmd.exe` and
  PowerShell before their first command; failure to write the response preserves
  the query in retained output and emits a diagnostic instead of hiding it.
- Dedicated guards still clean their own payload groups after Linux parent
  `SIGKILL`; direct MCP explicitly makes no such promise.
- `crates/zuno-process/tests/containment.rs` pins topology, idle waiting, clean
  shutdown, natural payload reaping, and abrupt-parent cleanup for guarded hosts.
  The mixed-host G6 fixture additionally pins zero MCP helpers and normal MCP
  process-group cleanup.
