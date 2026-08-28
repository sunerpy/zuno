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

Input also names its session target. A root composer uses the mounted root target;
an attached child composer sends the durable child session id and never tunnels the
message through the parent transcript. The runtime validates that target, writes the
message to that child's inbox, and supervises delivery independently of the render
loop. A completed child acquires an idle run lease and reopens its `TurnHost`; a
running child receives the same admitted input as a soft steer. The wake coordinator
closes the active-to-idle race, so a message that misses the running turn remains
pending and becomes the next child turn instead of being lost.

During an active turn, ordinary text and rich content target the nearest safe
step as steering. Commands and explicit next-turn work remain queued. A steer
interrupts a provider wait or provider-retry delay, checkpoints partial assistant
output without ending the turn, and starts the next model step with the promoted
input. It does not cancel a side-effecting tool already in flight. A steer that
misses the final safe point stays durably pending and becomes the next FIFO turn;
client channel capacity or reconnect timing never decides its fate.

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
- multiline question input with bounded growth that replaces the composer area
  while the tool waits; `Esc` cancels it as a refusal rather than returning a
  synthetic answer;
- visible permission, retry, diagnostics, and background-job states. TUI presentation
  settings live in `tui.json`; authorization remains a runtime concern in the main
  `zuno.json` `permission` block. `mode: allow_all` skips every Zuno tool-approval
  ask, while explicit denies and catastrophic Shell denials remain authoritative.
  Root and child asks share one foreground queue, but every request retains its trusted
  session, assistant-message, and tool-call origin. Closing the attached TUI refuses
  outstanding asks, and an `Always` grant is scoped to the originating session;
- explicit `working`, `awaiting approval`, and `awaiting answer` states. During a
  running turn the first `Esc` arms interruption and shows the confirmation
  immediately above the composer; the second within the confirmation window
  cancels it. If a modal or autocomplete surface owns the first press, that same
  press closes the surface and arms the turn, so two physical presses remain
  sufficient. `Stopping the active turn…` remains visible until the terminal turn
  event clears it; `TurnInterrupted` then appends one session-owned interruption
  marker, and replay reconstructs that marker from a persisted assistant abort
  rather than nesting it inside partial assistant output;
- liveness animation advances from a bounded UI clock, not from provider event
  arrival. A slow provider, shell command, or MCP call therefore keeps moving
  while producing no output; the clock stops while waiting for human input and
  at the completed or interrupted turn boundary;
- generic rendering for unknown future events;
- a `system` theme that reads non-invasive terminal color hints when available
  and otherwise preserves the terminal's foreground and background defaults. Its
  unordered-list marker uses muted text without bold emphasis, while ordered
  enumerations retain their own emphasized token. Ordinary content, reasoning, service
  names, and sidebar metadata use neutral foreground hierarchy; semantic green is
  reserved for compact success or health glyphs rather than complete sentences or rows;
- a full-height ambient sidebar outside the transcript, prompt, status, and info
  column, so no left-hand band renders underneath it;
- a visible transcript scrollbar with wheel and thumb dragging, plus
  application-owned text selection that is clipped to the transcript. Releasing
  a drag copies automatically, right-click copies the retained selection, and
  success or failure is reported without clearing the highlight.
  Root and attached-child composers use the same captured drag selection and
  clipboard path, and render an inverse theme-derived caret on both empty and
  populated input buffers;
  pasting one supported local image path, or image bytes supplied by the
  clipboard backend, inserts a visible `[Image #N]` draft handle while retaining
  filename, MIME, and bytes as separate typed content. `@project/path` resolves
  bounded text or images below the project root, and the same rich submission
  shape survives queue, steer, child continuation, durable replay, and provider
  encoding. See [images and file references](../reference/attachments.md);
  `mouse: false` opts back into terminal-native selection and alternate-scroll
  translation;
- ordinary modal overlays are centred in both axes; composer-owned questions
  remain anchored to the composer region. Leader continuation help is a compact,
  centred, titled, bordered overlay: it preserves readable cell widths and reports
  `+N more` instead of filling the frame with clipped descriptions. Numeric quick-session
  bindings remain active but are omitted from this help surface so nine repeated
  `Switch to session in quick slot` rows do not obscure higher-value commands. The
  default leader timeout is five seconds, so the continuation overlay does not vanish
  before it can be read. An open modal captures pointer input
  so clicks cannot activate covered transcript or sidebar content. Picker rows
  and confirmation buttons accept left-click selection as well as keyboard
  actions. Slash autocomplete, structured questions, permissions, sessions,
  models, agents, themes, MCP servers, subagents, background jobs, and memory
  entries are mouse-selectable; list and reference panels accept the wheel as
  well as keyboard navigation;
- clicking a user prompt opens its message actions. `Copy message` writes the
  complete prompt through the shared clipboard path and reports the result.
  `Revert this turn` is offered only when the live newest prompt has a restorable
  boundary; choosing it opens the same explicit Restore/Keep confirmation as
  `/undo`, and only Restore admits the typed undo command;
- step-level activity summaries for completed routine commands, reads, searches,
  images, and delegations. Running work, approvals, failures, and important
  results remain visible. A folded group retains one bounded identifier line per
  call: shell command text, read path and requested window, search query, or the
  tool-specific summary. Full arguments and results remain behind disclosure rather
  than flooding the main answer. `Ctrl+T` opens the complete scrollable durable
  transcript and preserves manual scroll position; `Alt+T` changes reasoning
  effort. Each thinking block remains folded by default, uses muted styling in
  both states, and owns an independent disclosure target. Its header is
  explicitly labelled `◇ Thought`; tool rows are explicitly labelled `Tool`, so
  the two secondary content types do not share an ambiguous visual shape.
  Clicking a thought header opens the complete persisted body without changing
  sibling blocks. `/thinking` remains the keyboard-wide fallback. If replay
  contains both the visible reasoning event and an identical provider-reasoning
  capsule, the client projects one block while retaining both durable records;
- user and assistant prose use the same CommonMark renderer, including GFM
  tables, lists, headings, code, quotes, and links. Rendering never changes the
  durable source text. The transcript adds hierarchy without adding saturated
  panels: a speaker title is followed by a weak neutral rule, Markdown headings
  use the neutral title role, and list/quote/rule chrome uses the muted role while
  prose remains primary. Structure therefore remains visible without relying on
  purple, green, or colour alone;
- per-call tool disclosure in the complete transcript, with subagent rendering
  selected by persisted `ToolUiIntent::Subagent` rather than hard-coded tool
  names. The collapsed row stays summary-only; expanding a call renders a
  pretty-printed `Arguments` section and a distinct `Result` section, with explicit
  bounded-overflow notices for pathological inputs. A call refused before execution is projected as `blocked` with warning
  styling and a durable block kind; only a call that actually ran and failed is
  projected as an error. Tool headers are composed from separate theme roles:
  disclosure and separators are muted, `Tool` and the tool identity are neutral
  titles, the argument summary is secondary, and warning/error/running emphasis
  is confined to the status glyph. Overflow hints are muted rather than rendered
  as primary actions. These span-level distinctions retain the existing per-call
  disclosure target and remain legible in monochrome terminals;
- one subagent overview for native child sessions and configured Codex or Claude Code
  product agents. It shows product/target, objective, status, elapsed time,
  session/run, job, report delivery, result, and safety diagnostics without
  exposing product-internal reasoning or tool streams. Enter toggles details;
  each compact row separates focus, status glyph, product title, target, and muted
  objective, with the selection background applied across the complete row.
  Expanded details use a weak `Details` divider, muted field labels, readable
  values, and per-child status glyphs instead of tinting whole rows;
  pressing `x` twice requests cancellation of a running job and keeps the list
  mounted for consecutive cancellations. Native child hosts additionally publish a
  full main-pane session projection: `Ctrl+X Down` enters the first direct child,
  `Ctrl+X Up` returns to its parent, and `Ctrl+X Left`/`Right` remain available for
  sibling navigation. While the attached child's composer is empty, plain Left/Right
  cycle siblings directly; once a draft exists, those same keys return to ordinary
  cursor movement rather than stealing text editing. Mouse-wheel events scroll the
  attached child's own transcript instead of being dropped at the child composer. The
  fixed two-row child footer shows the resolved Agent/model, current context occupancy,
  direct-child position such as `3/8`, sibling keys, the parent key, and whether Enter
  will steer or continue. The
  parent host remains mounted and running while the child transcript receives live
  events, so child progress is visible before the foreground `task` call completes.
  Every attached child owns its own `InputEditor` draft. Enter steers a running child
  and continues a completed child; the text is admitted to the child inbox before
  execution. Durable child rows and retained history are projected again when a TUI
  resumes the parent after a process restart. The child continuation identity stores
  its resolved Agent, model, effort, parent Attempt, and workflow lineage in session
  metadata, so a restored child is not view-only. Child input reopens the same full
  `TurnHost` used by a root session, including tools, permissions, cancellation,
  lifecycle reporting, usage, and automatic context compaction. Child input is literal text, so `/help` and other slash-looking strings
  are not dispatched as root or host commands. Switching siblings or returning to the
  parent preserves each child draft. A child may delegate only when its selected Agent
  exposes `task`, names the target in `delegates`, passes permission, and remains below
  `subagent_depth`; using the same host does not bypass those limits. Historical
  navigation follows durable direct-child session edges rather than inferring a child
  from ordinary tool activity. When a retained session has no delegated child,
  `Ctrl+X Down` leaves the transcript in place and directs the user to the transcript
  details instead of implying that history restoration failed;
- a skill census that separates discovery from use: the heading reports
  `loaded/discovered`, and only a successfully completed `skill` tool call marks a
  row `✓ skill-name · loaded`. Expanded skills are grouped with loaded skills
  first and not-loaded skills second, with each group sorted by name and source.
  Same-named skills display their source locator, and completion marks only the
  exact source selected by the tool call;
- an independently scrollable and selectable sidebar whose current-session title
  is a fixed header and participates in the same application-owned drag-selection
  and clipboard path as the body; its location/version footer stays fixed. Only the
  projection body scrolls. Foreground transcript-backed delegations appear
  immediately under `Agents`; once a call acquires a matching durable job it yields
  to the richer `Jobs` projection rather than appearing twice. The sidebar also
  projects goal, todos, pending memory, token usage, LSP, MCP, and skills from
  shared state rather than polling;
- `/ps` for process-owned background terminals and `/memory` for auditable
  candidate review. Both keep their list mounted after an action so several
  entries can be handled consecutively;
- the welcome screen owns only a prepared process identity. It creates no durable
  session until the first model-bound submission commits the session and user
  message together. `session.materialized` updates the in-place session catalog,
  so `/session` sees the new row without remounting;
- usage comes from the durable `SessionUsage` projection on resume. A history
  whose provider accounting cannot be recovered displays an unavailable marker
  rather than a fabricated zero. Cumulative input/output/cache buckets remain
  available for accounting and history, but the live sidebar shows only the
  latest whole prompt, model-window limit, and decimal occupancy percentage;
- no empty LSP status or setup prompt; the sidebar adds LSP only for configured
  services or real diagnostics;
- `/session` lists active root sessions from the current durable database and
  exact working directory. Selecting another session is admitted only between
  turns and remounts the complete session composition, including transcript
  replay, permissions, cancellation ownership, LSP/MCP workers, and snapshot
  history. The physical terminal activation remains mounted throughout, so a
  switch never leaves and re-enters the alternate screen;
- `/new` performs the same in-process remount to a fresh prepared identity. The
  command opens a blank conversation shell directly rather than returning to the
  launch welcome page. It creates no row; the first model-bound prompt
  materializes exactly one new session through the normal durable-input
  transaction;
- `/compact` invokes the runtime compaction agent and persists the resulting
  summary. `compaction.threshold_percent` controls proactive compaction against
  the usable model window, while `compaction.auto: false` disables that proactive
  trigger without removing manual compaction or bounded context-limit recovery;
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
  enough to inspect or select. Ephemeral command guidance, such as an unknown
  slash command, is a short-lived toast and does not become durable transcript
  content;
- no blocking network, LSP, or provider work in the render loop.

## Future GUI

A GUI should reuse the HTTP event replay, projections, command registry, question and approval protocols, and attachment references. GUI-only packages may own layout, theming, caching, and richer tool renderers. They must not import provider implementations or mutate session tables directly.

The first GUI milestone therefore needs no engine fork:

1. versioned handshake and session snapshot;
2. cursor replay plus SSE or WebSocket live events;
3. idempotent input admission;
4. command, question, approval, interrupt, goal pause, and goal resume endpoints;
5. generic tool rendering with optional specialized renderers.
