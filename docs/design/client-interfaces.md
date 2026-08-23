# Client interface architecture

Zuno's TUI, headless CLI, HTTP server, ACP adapter, and future GUI clients are views over the same durable agent runtime. No client owns a private turn loop.

## Shared model

Every client uses four runtime surfaces:

1. Commands admit user intent through the same command registry and real handlers.
2. Session events are the durable record of prompts, model output, tools, retries, questions, approvals, subagent reports, and lifecycle changes.
3. Projections derive current conversation and status state from those events.
4. The durable inbox accepts prompts, live steering, and `reportDelivery: nextStep` reports before work is scheduled.

The shared projection vocabulary includes:

- `ActivityProjection` for one model step's commands, reads, searches, images,
  delegations, and other tool activity;
- `WorkStateProjection` for the active goal, todos, durable jobs, memory
  candidates, and resident entries;
- `SessionUsage` for cumulative provider-accounted tokens and context-window
  state;
- `BackgroundExecutionProjection` for process-owned terminal state and bounded
  output.

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
- a composer that uses the available left pane with only a one-column gutter;
- multiline question input with bounded growth;
- visible permission, retry, diagnostics, and background-job states;
- generic rendering for unknown future events;
- a `system` theme that reads non-invasive terminal color hints when available
  and otherwise preserves the terminal's foreground and background defaults;
- a full-height ambient sidebar outside the transcript, prompt, status, and info
  column, so no left-hand band renders underneath it;
- a visible transcript scrollbar with wheel and thumb dragging, plus
  application-owned text selection that is clipped to the transcript. Releasing
  a drag copies automatically, right-click copies the retained selection, and
  success or failure is reported without clearing the highlight.
  `mouse: false` opts back into terminal-native selection and alternate-scroll
  translation;
- step-level activity summaries for completed routine commands, reads, searches,
  images, and delegations. Running work, approvals, failures, and important
  results remain visible. `Ctrl+T` opens the complete scrollable durable
  transcript and preserves manual scroll position; `Alt+T` changes reasoning
  effort. Each thinking block remains folded by default, uses muted styling when
  expanded, and owns an independent disclosure target: clicking its header opens
  the complete persisted body without changing sibling blocks. `/thinking`
  remains the keyboard-wide fallback;
- user and assistant prose use the same CommonMark renderer, including GFM
  tables, lists, headings, code, quotes, and links. Rendering never changes the
  durable source text;
- per-call tool disclosure in the complete transcript, with subagent rendering
  selected by persisted `ToolUiIntent::Subagent` rather than hard-coded tool
  names. A call refused before execution is projected as `blocked` with warning
  styling and a durable block kind; only a call that actually ran and failed is
  projected as an error;
- one subagent view for native child sessions and configured Codex or Claude Code
  product agents. It shows product/target, objective, status, elapsed time,
  session/run, job, report delivery, result, and safety diagnostics without
  exposing product-internal reasoning or tool streams. Enter toggles details;
  pressing `x` twice requests cancellation of a running job and keeps the list
  mounted for consecutive cancellations;
- a skill census that separates discovery from use: the heading reports
  `loaded/discovered`, and only a successfully completed `skill` tool call marks a
  row `✓ skill-name · loaded`;
- an independently scrollable and selectable sidebar whose location/version
  footer stays fixed. It projects goal, todos, jobs, pending memory, token usage,
  LSP, MCP, and skills from shared state rather than polling;
- `/ps` for process-owned background terminals and `/memory` for auditable
  candidate review. Both keep their list mounted after an action so several
  entries can be handled consecutively;
- the welcome screen owns only a prepared process identity. It creates no durable
  session until the first model-bound submission commits the session and user
  message together. `session.materialized` updates the in-place session catalog,
  so `/session` sees the new row without remounting;
- cumulative token and context usage comes from the durable `SessionUsage`
  projection on resume. A history whose provider accounting cannot be recovered
  displays an unavailable marker rather than a fabricated zero. Labels keep
  cumulative session totals separate from the latest whole prompt: the sidebar
  shows `session total`, input/output/cache buckets, `current prompt / model
  window`, and a decimal percentage of that model window;
- no empty LSP status or setup prompt; the sidebar adds LSP only for configured
  services or real diagnostics;
- `/session` lists active root sessions from the current durable database and
  exact working directory. Selecting another session is admitted only between
  turns and remounts the complete session composition, including transcript
  replay, permissions, cancellation ownership, LSP/MCP workers, and snapshot
  history. The physical terminal activation remains mounted throughout, so a
  switch never leaves and re-enters the alternate screen;
- `/new` performs the same in-process remount to a fresh prepared identity. The
  command itself creates no row; the first model-bound prompt materializes
  exactly one new session through the normal durable-input transaction;
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
