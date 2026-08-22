# Client interface architecture

Zuno's TUI, headless CLI, HTTP server, ACP adapter, and future GUI clients are views over the same durable agent runtime. No client owns a private turn loop.

## Shared model

Every client uses four runtime surfaces:

1. Commands admit user intent through the same command registry and real handlers.
2. Session events are the durable record of prompts, model output, tools, retries, questions, approvals, subagent reports, and lifecycle changes.
3. Projections derive current conversation and status state from those events.
4. The durable inbox accepts prompts, live steering, and `reportDelivery: nextStep` reports before work is scheduled.

The server exposes cursor-based replay followed by live delivery. A reconnect sends the last committed cursor, receives every later event in order, then joins the bounded live stream. A client that misses live events must replay; it must not infer the missing state.

The only HTTP event operations are `GET /api/event` for live process-wide notifications and `GET /api/session/{sessionID}/event` for durable session replay plus live delivery. The session operation emits `sessionID:sequence` as the SSE id and accepts that value through `Last-Event-ID` on reconnect. Zuno does not mount an unscoped `/event` adapter or a second event envelope.

Routes and OpenAPI operations exist only when a real handler exists. Optional provider, credential, or client capabilities register their operations with their implementation rather than exposing a permanent placeholder that can only fail.

## Client capabilities

A client handshake should advertise only presentation and transport capabilities:

- supported event and projection versions;
- inline image and attachment rendering;
- terminal, diff, location, and generic tool renderers;
- interactive question, approval, and permission controls;
- maximum accepted snapshot and event batch sizes.

Capability negotiation must not alter agent semantics. An unsupported renderer falls back to a generic event view; it does not hide the durable event or change the tool call.

## Input admission

Every submitted input receives an admission identifier before execution. A client may optimistically render a pending row keyed by that identifier, then replace it when the committed event arrives. Reconnecting with the same identifier must not create a duplicate input.

Human input has priority over an automatic goal retry. The client may show the persisted retry deadline and reason, but cancellation, pause, and resume are explicit commands rather than local timer changes.

## Backpressure and disconnects

- Durable writes complete before a success response is returned.
- Bounded live channels may drop a subscriber, never a committed event.
- Slow clients resume from their last cursor.
- Large projections use a snapshot envelope plus later events.
- Background reports settle durably before they wake a parent.
- A client disconnect never cancels an active goal unless it issued an explicit interrupt.

## TUI

The TUI favors dense, keyboard-first operation:

- stable transcript and status-strip dimensions;
- multiline question input with bounded growth;
- visible permission, retry, diagnostics, and background-job states;
- generic rendering for unknown future events;
- a `system` theme that reads non-invasive terminal color hints when available
  and otherwise preserves the terminal's foreground and background defaults;
- terminal-native mouse selection across the transcript, sidebar, and input by
  default; `mouse: true` in `tui.json` explicitly trades selection for TUI mouse
  capture and click handling; native-selection mode enables terminal
  alternate-scroll translation so wheel input scrolls a long transcript while
  the composer is empty;
- no empty LSP status or setup prompt; the sidebar adds LSP only for configured
  services or real diagnostics;
- `/session` lists active root sessions from the current durable database and
  exact working directory. Selecting another session is admitted only between
  turns and remounts the complete session composition, including transcript
  replay, permissions, cancellation ownership, LSP/MCP workers, and snapshot
  history. The physical terminal activation remains mounted throughout, so a
  switch never leaves and re-enters the alternate screen;
- the same session list owns row actions: `Ctrl+R` opens a pre-filled rename
  prompt, while `Ctrl+D` must be pressed twice on the same row before deletion.
  Both actions are revalidated by the host and use the transactional session
  store; deleting the current session remounts the most recent remaining session
  in that directory, or creates a new one when none remains. A successful delete
  reopens the refreshed session list on that replacement so users can delete
  several sessions without invoking `/session` again; a refused delete leaves
  the existing list mounted. A current session with background subagents still
  running is refused rather than deleting state those tasks can still write;
- warning and error notices wrap inside the viewport and remain visible long
  enough to inspect or select;
- no blocking network, LSP, or provider work in the render loop.

## Future GUI

A GUI should reuse the HTTP event replay, projections, command registry, question and approval protocols, and attachment references. GUI-only packages may own layout, theming, caching, and richer tool renderers. They must not import provider implementations or mutate session tables directly.

The first GUI milestone therefore needs no engine fork:

1. versioned handshake and session snapshot;
2. cursor replay plus SSE or WebSocket live events;
3. idempotent input admission;
4. command, question, approval, interrupt, goal pause, and goal resume endpoints;
5. generic tool rendering with optional specialized renderers.
