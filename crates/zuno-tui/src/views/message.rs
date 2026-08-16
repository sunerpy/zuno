//! The chat transcript: messages, their parts, and how a provider stream becomes
//! rendered rows.
//!
//! # Parts, and why the transcript is a fold over engine events
//!
//! The engine emits [`TurnEvent`]; it never hands over a rendered message. So the
//! transcript is a **fold**: [`Transcript::observe`] takes one event and mutates
//! the part list, and [`TranscriptView`] draws whatever the fold has produced so
//! far. That is what makes incremental rendering testable — draw, feed one delta,
//! draw again, and compare the two buffers — and it keeps the direction of the
//! dependency right: rendering consumes engine values and cannot call the engine.
//!
//! # Reasoning gets its own affordance
//!
//! Upstream treats reasoning as a distinct part with a collapse state and a
//! separate colour derived from `thinkingOpacity`
//! (`packages/tui/src/routes/session/index.tsx:1567,1586-1591,1645-1652`). Collapsed
//! shows a one-line summary; expanded shows the text. The distinction matters
//! because reasoning is frequently longer than the answer, so a transcript that
//! rendered it like text would bury the reply.
//!
//! # A tool call renders its status, not just its name
//!
//! The four states are the oracle's (`index.tsx:1715,2345`): `pending` while the
//! model is still writing the arguments, `running` once dispatched, then
//! `completed` or `error`. Each carries a distinct glyph so the transcript reads at
//! a glance, and the per-tool icons are the oracle's own
//! (`index.tsx:1808,2090,2124,2138,2163,2186,2198,2206,2296`).
//!
//! # `RetryRollback` discards, and that is a correctness requirement
//!
//! [`zuno_llm::event::StreamEvent::RetryRollback`] documents that a consumer must
//! discard text, tool calls, and reasoning accumulated for the interrupted attempt.
//! A transcript that appended instead would show the model's answer twice. The fold
//! honours it and a test asserts the discard.

use crate::app::{AppEvent, Component, EventResult};
use crate::views::{ViewContext, fill, padded};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, symbols};
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::StreamEvent;

#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The human.
    User,
    /// The model.
    Assistant,
    /// The session itself, for something neither party said.
    System,
}

impl Role {
    /// The rule drawn down the left edge of every row this role owns.
    ///
    /// Upstream distinguishes the two sides by drawing the user's turn as a panel
    /// with a coloured left rule and leaving the assistant's prose unmarked
    /// (`routes/session/index.tsx:1395-1420`). A rule on all three sides is better
    /// here for a reason upstream does not have to care about: an off-screen buffer
    /// assertion can then tell the roles apart *positionally*, at column zero,
    /// instead of searching for a label that any wrapped body line might also
    /// contain. Three distinct glyphs rather than one in three colours, because a
    /// colour is invisible to the row-text assertions every view test is built on.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::User => "▌",
            Self::Assistant => "│",
            Self::System => "▲",
        }
    }
}

/// How far a tool call has got.
///
/// `packages/tui/src/routes/session/index.tsx:1715,2345`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// The model is still writing the arguments.
    Pending,
    /// Dispatched and running.
    Running,
    /// Finished successfully.
    Completed,
    /// Finished with a failure.
    Error,
}

impl ToolStatus {
    /// The glyph that renders this status.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "~",
            Self::Running => "…",
            Self::Completed => "✓",
            Self::Error => "✗",
        }
    }

    /// Whether the call is still in flight.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// The per-tool icon and the placeholder shown before the arguments arrive.
///
/// Verbatim from the oracle's `InlineTool` call sites: bash `$`/"Writing
/// command...", glob `✱`/"Finding files...", grep `✱`/"Searching content...",
/// read `→`/"Reading file...", write `→`/"Preparing write...", webfetch
/// `%`/"Fetching from the web...", websearch `◈`/"Searching web...", task
/// `#`/"Delegating..." (`index.tsx:2090,2138,2186,2163,2124,2198,2206,2296`), with
/// `⚙` as the generic fallback (`index.tsx:1808`).
#[must_use]
pub fn tool_affordance(name: &str) -> (&'static str, &'static str) {
    match name {
        "bash" => ("$", "Writing command..."),
        "glob" => ("✱", "Finding files..."),
        "grep" => ("✱", "Searching content..."),
        "read" => ("→", "Reading file..."),
        "write" | "edit" | "patch" => ("→", "Preparing write..."),
        "webfetch" => ("%", "Fetching from the web..."),
        "websearch" => ("◈", "Searching web..."),
        "task" => ("#", "Delegating..."),
        _ => ("⚙", "Preparing..."),
    }
}

/// Whether a reasoning block shows its text or only a summary.
///
/// Two states rather than upstream's three-way cycle
/// (`context/thinking.ts`): the third upstream state is a per-session preference,
/// and this is the per-part affordance the `display_thinking` action toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDisplay {
    /// One summary line.
    Collapsed,
    /// The full reasoning text.
    Expanded,
}

impl ThinkingDisplay {
    /// The glyph that says whether there is more text behind this block.
    ///
    /// Upstream uses `+ `/`- ` (`routes/session/index.tsx:1671-1675`). Those are the
    /// wrong two characters *here*, because this transcript renders unified diffs
    /// inline: a row beginning `+ ` already means "an added line", and reusing it for
    /// "expandable" would make the two indistinguishable at a glance. The triangles
    /// carry the same meaning, are already the sidebar's collapse vocabulary, and
    /// collide with nothing.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Collapsed => "▸",
            Self::Expanded => "▾",
        }
    }
}

/// Whether a tool call shows all of its output or only the first few rows.
///
/// Tool output is the most variable content in a transcript — a `read` of a large
/// file is thousands of rows — so it is capped by default and the cap states how much
/// it hid. The `tool_details` action lifts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDisplay {
    /// The first [`TOOL_OUTPUT_PREVIEW_ROWS`] rows, and a count of the rest.
    Collapsed,
    /// Up to [`TOOL_OUTPUT_MAX_ROWS`] rows.
    Expanded,
}

impl ToolDisplay {
    /// How many rows of output this display shows.
    #[must_use]
    pub const fn rows(self) -> usize {
        match self {
            Self::Collapsed => TOOL_OUTPUT_PREVIEW_ROWS,
            Self::Expanded => TOOL_OUTPUT_MAX_ROWS,
        }
    }
}

/// Rows of tool output shown before the collapse notice.
///
/// Three is enough to see that a command produced the shape of output expected and
/// short enough that four tool calls still fit on one screen.
pub const TOOL_OUTPUT_PREVIEW_ROWS: usize = 3;

/// Rows of tool output shown when expanded.
///
/// A ceiling rather than everything, because the transcript wraps and counts every
/// row it produces in order to scroll, and a single unbounded `read` result would
/// make that arithmetic dominate the frame.
pub const TOOL_OUTPUT_MAX_ROWS: usize = 60;

/// Whether `text` is a unified diff this transcript should render as one.
///
/// A hunk header is required rather than merely leading `+`/`-` runs, because a
/// tool that printed a bulleted list would otherwise be recoloured as a patch — and
/// a false diff is worse than a plain one, since its colours assert a meaning the
/// content does not have.
#[must_use]
pub fn looks_like_diff(text: &str) -> bool {
    text.lines().any(|line| line.starts_with("@@"))
        && text
            .lines()
            .any(|line| line.starts_with('+') || line.starts_with('-'))
}

/// The braille spinner frames, in order.
///
/// A moving glyph is the cheapest honest signal that a turn is alive: a static word
/// `working` is indistinguishable from a hung process, which is the single most
/// expensive ambiguity an interactive surface can have.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One renderable piece of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum MessagePart {
    /// Assistant- or user-visible prose.
    Text {
        /// The accumulated text.
        text: String,
    },
    /// A reasoning block.
    Reasoning {
        /// The accumulated reasoning text.
        text: String,
        /// Measured duration, once the provider reports it.
        duration_secs: Option<f64>,
        /// Whether the block is still receiving deltas.
        streaming: bool,
    },
    /// A tool call.
    Tool {
        /// The provider's call id.
        call_id: String,
        /// The tool's wire name.
        name: String,
        /// The human-readable title, once the call completes.
        title: Option<String>,
        /// How far it has got.
        status: ToolStatus,
        /// The tool's output, once it completes.
        output: Option<String>,
    },
    /// A file the user attached or the model produced.
    Attachment {
        /// The display name.
        filename: String,
        /// The MIME type, when known.
        mime: Option<String>,
    },
    /// A provider replay that is waiting or starting now.
    Retry { attempt: u32, max: u32 },
    /// Something the session needs the user to know, from neither party.
    ///
    /// A part of its own rather than text on an assistant or user message, because a
    /// warning attributed to either is a lie about who said it — and a warning the
    /// host writes to stderr instead is one the alternate screen hides.
    Notice {
        /// The message, already human-readable.
        text: String,
    },
}

impl MessagePart {
    /// The accumulated text of a text part, for tests and copy actions.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// One message and its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Who produced it.
    pub role: Role,
    /// The engine's message id, when it has one.
    pub id: Option<String>,
    /// Its parts, in arrival order.
    pub parts: Vec<MessagePart>,
}

impl Message {
    /// An empty message from `role`.
    #[must_use]
    pub fn new(role: Role) -> Self {
        Self {
            role,
            id: None,
            parts: Vec::new(),
        }
    }

    /// A user message carrying one block of text.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            id: None,
            parts: vec![MessagePart::Text { text: text.into() }],
        }
    }

    /// A session notice carrying one line the user has to see.
    #[must_use]
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            id: None,
            parts: vec![MessagePart::Notice { text: text.into() }],
        }
    }

    /// Append an attachment.
    pub fn attach(&mut self, filename: impl Into<String>, mime: Option<String>) {
        self.parts.push(MessagePart::Attachment {
            filename: filename.into(),
            mime,
        });
    }
}

/// The transcript: every message, folded from engine events.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    messages: Vec<Message>,
    /// The index of the assistant message currently receiving deltas.
    streaming: Option<usize>,
    /// Whether the turn is still running, for the status affordance.
    running: bool,
    /// Session-cumulative provider token accounting.
    ///
    /// The same [`TokenUsage`] the status strip carries, folded from the same event.
    /// One type rather than two: a sidebar with its own accumulator is a second
    /// running total free to disagree with the strip's, and two token figures on one
    /// screen that differ is worse than either alone.
    tokens: TokenUsage,
    /// The model's context ceiling, when the catalog states one.
    context_limit: u64,
    /// How many events have been folded, which is what advances the spinner.
    ticks: usize,
}

impl Transcript {
    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The messages so far.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether a turn is in flight.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// The session's cumulative token accounting.
    #[must_use]
    pub const fn tokens(&self) -> TokenUsage {
        self.tokens
    }

    /// State the model's context ceiling, so a percentage can be computed.
    pub const fn set_context_limit(&mut self, limit: u64) {
        self.context_limit = limit;
    }

    /// The model's context ceiling, or zero when the catalog states none.
    #[must_use]
    pub const fn context_limit(&self) -> u64 {
        self.context_limit
    }

    /// How full the context window is, as a percentage, when a ceiling is known.
    ///
    /// Computed from the last prompt's billed input plus its cache reads, never from
    /// the cumulative total: the total exceeds any window as soon as a second turn
    /// happens, so a percentage derived from it climbs past 100 and means nothing.
    /// A zero ceiling is "no window declared" — see `token_count` in the CLI's turn
    /// plan, which maps a non-finite catalog limit to zero — so it yields `None`
    /// rather than dividing.
    #[must_use]
    pub const fn context_used(&self) -> Option<u64> {
        if self.context_limit == 0 {
            return None;
        }
        let used = self.tokens.input.saturating_add(self.tokens.cache_read);
        Some(used.saturating_mul(100) / self.context_limit)
    }

    /// The spinner frame this transcript is on.
    #[must_use]
    pub const fn spinner(&self) -> &'static str {
        SPINNER[self.ticks % SPINNER.len()]
    }

    /// Append a message written locally, such as the user's own prompt.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Fold one engine event into the transcript.
    ///
    /// Returns whether anything visible changed, which is what the component turns
    /// into a redraw request.
    pub fn observe(&mut self, event: &TurnEvent) -> bool {
        self.ticks = self.ticks.wrapping_add(1);
        match event {
            TurnEvent::TurnStarted { .. } => {
                self.running = true;
                true
            }
            TurnEvent::AssistantMessageCreated { message_id, .. } => {
                self.messages.push(Message {
                    role: Role::Assistant,
                    id: Some(message_id.clone()),
                    parts: Vec::new(),
                });
                self.streaming = Some(self.messages.len() - 1);
                true
            }
            TurnEvent::Provider { event, .. } => self.observe_stream(event),
            TurnEvent::ToolDispatchStarted { call_id, name, .. } => {
                self.update_tool(call_id, |part| {
                    if let MessagePart::Tool { status, .. } = part {
                        *status = ToolStatus::Running;
                    }
                }) || {
                    // A dispatch with no `ToolUseStart` seen — the provider stream
                    // was not observed, e.g. after a reconnect. Materialise the
                    // call rather than dropping it from the transcript.
                    self.append(MessagePart::Tool {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        title: None,
                        status: ToolStatus::Running,
                        output: None,
                    });
                    true
                }
            }
            TurnEvent::ToolDispatchCompleted {
                call_id,
                title,
                output,
                is_error,
                ..
            } => self.update_tool(call_id, |part| {
                if let MessagePart::Tool {
                    status,
                    title: slot,
                    output: body,
                    ..
                } = part
                {
                    *status = if *is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    };
                    *slot = Some(title.clone());
                    *body = Some(output.clone());
                }
            }),
            TurnEvent::TurnCompleted { .. } | TurnEvent::TurnInterrupted { .. } => {
                self.running = false;
                self.streaming = None;
                self.close_reasoning();
                true
            }
            // Everything else is bookkeeping the transcript does not render:
            // history repair, agent/model resolution, tool snapshots, request
            // starts, checkpoints, per-step results. Reporting "nothing changed"
            // keeps them from forcing a frame.
            _ => false,
        }
    }

    fn observe_stream(&mut self, event: &StreamEvent) -> bool {
        match event {
            StreamEvent::TextDelta(delta) => {
                if let Some(MessagePart::Text { text }) = self.last_part_mut() {
                    text.push_str(delta);
                } else {
                    self.append(MessagePart::Text {
                        text: delta.clone(),
                    });
                }
                true
            }
            StreamEvent::ReasoningStart => {
                self.append(MessagePart::Reasoning {
                    text: String::new(),
                    duration_secs: None,
                    streaming: true,
                });
                true
            }
            StreamEvent::ReasoningDelta(delta) => {
                if let Some(MessagePart::Reasoning { text, .. }) = self.last_part_mut() {
                    text.push_str(delta);
                } else {
                    self.append(MessagePart::Reasoning {
                        text: delta.clone(),
                        duration_secs: None,
                        streaming: true,
                    });
                }
                true
            }
            StreamEvent::ReasoningEnd => {
                self.close_reasoning();
                true
            }
            StreamEvent::ReasoningDone { duration_secs } => {
                if let Some(index) = self.streaming {
                    for part in self.messages[index].parts.iter_mut().rev() {
                        if let MessagePart::Reasoning {
                            duration_secs: slot,
                            streaming,
                            ..
                        } = part
                        {
                            *slot = Some(*duration_secs);
                            *streaming = false;
                            break;
                        }
                    }
                }
                true
            }
            StreamEvent::ToolUseStart { id, name } => {
                self.append(MessagePart::Tool {
                    call_id: id.clone(),
                    name: name.clone(),
                    title: None,
                    status: ToolStatus::Pending,
                    output: None,
                });
                true
            }
            StreamEvent::GeneratedImage { path, .. } => {
                self.append(MessagePart::Attachment {
                    filename: path.clone(),
                    mime: Some(String::from("image/*")),
                });
                true
            }
            StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
            } => {
                self.tokens.add(
                    input_tokens.unwrap_or_default(),
                    output_tokens.unwrap_or_default(),
                    cache_read_input_tokens.unwrap_or_default(),
                    cache_write_input_tokens.unwrap_or_default(),
                );
                true
            }
            StreamEvent::RetryRollback { attempt, max } => {
                // The provider will replay from the beginning. Discarding is not an
                // optimisation: keeping the parts would render the answer twice.
                if let Some(index) = self.streaming {
                    self.messages[index].parts.clear();
                }
                self.append(MessagePart::Retry {
                    attempt: *attempt,
                    max: *max,
                });
                true
            }
            // Warnings go in the transcript, not only on the status strip: the strip
            // holds one detail and the next one overwrites it, so a suppressed tool
            // would appear for a moment and then be gone. The rest of the details are
            // transient by nature and stay on the strip alone.
            StreamEvent::StatusDetail { detail } if detail.starts_with("warning: ") => {
                self.messages.push(Message::notice(detail.clone()));
                self.streaming = None;
                true
            }
            _ => false,
        }
    }

    fn append(&mut self, part: MessagePart) {
        match self.streaming {
            Some(index) => self.messages[index].parts.push(part),
            None => {
                // A delta with no `AssistantMessageCreated` before it. Open a
                // message rather than dropping the content on the floor.
                let mut message = Message::new(Role::Assistant);
                message.parts.push(part);
                self.messages.push(message);
                self.streaming = Some(self.messages.len() - 1);
            }
        }
    }

    fn last_part_mut(&mut self) -> Option<&mut MessagePart> {
        let index = self.streaming?;
        self.messages.get_mut(index)?.parts.last_mut()
    }

    fn close_reasoning(&mut self) {
        if let Some(index) = self.streaming {
            for part in self.messages[index].parts.iter_mut().rev() {
                if let MessagePart::Reasoning { streaming, .. } = part {
                    *streaming = false;
                    break;
                }
            }
        }
    }

    fn update_tool(&mut self, call_id: &str, mutate: impl FnOnce(&mut MessagePart)) -> bool {
        for message in self.messages.iter_mut().rev() {
            for part in message.parts.iter_mut() {
                if matches!(part, MessagePart::Tool { call_id: id, .. } if id == call_id) {
                    mutate(part);
                    return true;
                }
            }
        }
        false
    }
}

/// The chat transcript as a component.
///
/// Owns the fold, the reasoning affordance, and the scroll offset. Scrolling itself
/// lives in [`crate::views::scroll`]; this view only applies the offset it is told.
pub struct TranscriptView {
    context: ViewContext,
    transcript: Transcript,
    thinking: ThinkingDisplay,
    tool_output: ToolDisplay,
    /// First rendered row, from the top of the produced line list.
    offset: usize,
    /// Rows the last render produced, so a scroller can clamp against content.
    content_height: usize,
    /// Rows the last render had room for.
    viewport_height: usize,
    /// Whether the viewport follows the newest row as content arrives.
    ///
    /// True until the user scrolls away, which is the only reading of "follow" that
    /// does not fight them: a transcript that always jumped to the bottom would make
    /// scrolling back through a long tool result impossible while a turn ran.
    following: bool,
}

impl TranscriptView {
    /// A transcript view over `context`.
    #[must_use]
    pub fn new(context: ViewContext) -> Self {
        Self {
            context,
            transcript: Transcript::new(),
            thinking: ThinkingDisplay::Collapsed,
            tool_output: ToolDisplay::Collapsed,
            offset: 0,
            content_height: 0,
            viewport_height: 0,
            following: true,
        }
    }

    /// Flip the tool-output affordance, the `tool_details` action.
    pub const fn toggle_tool_output(&mut self) {
        self.tool_output = match self.tool_output {
            ToolDisplay::Collapsed => ToolDisplay::Expanded,
            ToolDisplay::Expanded => ToolDisplay::Collapsed,
        };
    }

    /// The current tool-output affordance.
    #[must_use]
    pub const fn tool_output(&self) -> ToolDisplay {
        self.tool_output
    }

    /// Whether the viewport is pinned to the newest row.
    #[must_use]
    pub const fn is_following(&self) -> bool {
        self.following
    }

    /// Pin the viewport back to the newest row.
    pub const fn follow(&mut self) {
        self.following = true;
    }

    /// The folded transcript.
    #[must_use]
    pub const fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The folded transcript, mutably, for locally composed messages.
    pub const fn transcript_mut(&mut self) -> &mut Transcript {
        &mut self.transcript
    }

    /// Flip the reasoning affordance, the `display_thinking` action.
    pub const fn toggle_thinking(&mut self) {
        self.thinking = match self.thinking {
            ThinkingDisplay::Collapsed => ThinkingDisplay::Expanded,
            ThinkingDisplay::Expanded => ThinkingDisplay::Collapsed,
        };
    }

    /// The current reasoning affordance.
    #[must_use]
    pub const fn thinking(&self) -> ThinkingDisplay {
        self.thinking
    }

    /// The first rendered row.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Rows the transcript would occupy at the last rendered width.
    #[must_use]
    pub const fn content_height(&self) -> usize {
        self.content_height
    }

    /// Rows the last render had room for.
    #[must_use]
    pub const fn viewport_height(&self) -> usize {
        self.viewport_height
    }

    /// Move the viewport, clamped to the content produced by the last render.
    ///
    /// Landing on the last row re-arms following, so a user who scrolls back and then
    /// returns to the bottom does not have to keep scrolling to watch a live turn.
    pub const fn set_offset(&mut self, offset: usize) {
        let max = self.content_height.saturating_sub(self.viewport_height);
        self.offset = if offset > max { max } else { offset };
        self.following = self.offset >= max;
    }

    /// The rendered rows, before the viewport is applied.
    ///
    /// Public because it is the transcript's testable surface: an assertion over
    /// lines is readable where the same assertion over cells is not, and the
    /// off-screen buffer test then proves the lines reach cells.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut previous: Option<Role> = None;
        for message in &self.transcript.messages {
            let rule = self.rule_style(message.role);
            // A multi-step turn opens one assistant message per step, so a header per
            // message printed `Assistant` five times for what the user experienced as
            // one reply. The header marks a change of speaker, which is what it was
            // always for; the left rule already runs down every row of the turn.
            if previous != Some(message.role) {
                lines.push(self.ruled(
                    message.role,
                    rule,
                    self.role_label(message.role),
                    self.context.title(),
                    width,
                ));
            }
            previous = Some(message.role);
            for part in &message.parts {
                self.part_lines(message.role, rule, part, width, &mut lines);
            }
            lines.push(padded("", width, self.context.surface()));
        }
        if self.transcript.running {
            lines.push(padded(
                &format!("{} working", self.transcript.spinner()),
                width,
                self.context.accent(),
            ));
        }
        lines
    }

    const fn role_label(&self, role: Role) -> &'static str {
        match role {
            Role::User => "You",
            Role::Assistant => "Assistant",
            Role::System => "Session",
        }
    }

    fn rule_style(&self, role: Role) -> Style {
        match role {
            Role::User => self.context.accent(),
            Role::Assistant => Style::new()
                .fg(self.context.palette.border_subtle.into())
                .bg(self.context.palette.background_panel.into()),
            Role::System => self.context.warning(),
        }
    }

    /// One row carrying the role's left rule, then `body` in `style`.
    ///
    /// Two spans rather than one padded string because the rule and the body are
    /// different colours; a single span could only be one, which is precisely how the
    /// old renderer ended up with a transcript that had no visible structure.
    fn ruled(
        &self,
        role: Role,
        rule: Style,
        body: &str,
        style: Style,
        width: u16,
    ) -> Line<'static> {
        let marker = role.marker();
        let gutter = marker.chars().count() + 1;
        let columns = usize::from(width);
        let room = columns.saturating_sub(gutter);
        let mut text = body.chars().take(room).collect::<String>();
        let used = text.chars().count();
        if used < room {
            text.extend(std::iter::repeat_n(' ', room - used));
        }
        Line::from(vec![
            Span::styled(format!("{marker} "), rule),
            Span::styled(text, style),
        ])
    }

    fn part_lines(
        &self,
        role: Role,
        rule: Style,
        part: &MessagePart,
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let gutter = u16::try_from(role.marker().chars().count() + 1).unwrap_or(2);
        let body_width = width.saturating_sub(gutter);
        let push = |body: &str, style: Style, out: &mut Vec<Line<'static>>| {
            out.push(self.ruled(role, rule, body, style, width));
        };
        match part {
            MessagePart::Text { text } => {
                for row in wrap(text, body_width) {
                    push(&row, self.context.text(), out);
                }
            }
            MessagePart::Reasoning {
                text,
                duration_secs,
                streaming,
            } => {
                let hidden = text.lines().filter(|line| !line.trim().is_empty()).count();
                let elapsed = match (duration_secs, streaming) {
                    (Some(secs), _) => format!(" ({secs:.1}s)"),
                    (None, true) => String::from("…"),
                    (None, false) => String::new(),
                };
                let header = match self.thinking {
                    // The row states the size of what it hides, so the affordance says
                    // "there is more here" without the reader having to open it to find
                    // out whether opening it was worth doing.
                    ThinkingDisplay::Collapsed if hidden > 1 => format!(
                        "{} Thinking{elapsed} · {hidden} lines",
                        self.thinking.glyph()
                    ),
                    _ => format!("{} Thinking{elapsed}", self.thinking.glyph()),
                };
                push(&header, self.context.thinking(), out);
                match self.thinking {
                    ThinkingDisplay::Collapsed => {
                        if let Some(summary) = summary(text) {
                            push(&format!("  {summary}"), self.context.thinking(), out);
                        }
                    }
                    ThinkingDisplay::Expanded => {
                        for row in wrap(text, body_width.saturating_sub(2)) {
                            push(&format!("  {row}"), self.context.thinking(), out);
                        }
                    }
                }
            }
            MessagePart::Tool {
                name,
                title,
                status,
                output,
                ..
            } => {
                let (icon, placeholder) = tool_affordance(name);
                let label = match (title, status) {
                    (Some(title), _) => title.clone(),
                    (None, ToolStatus::Pending) => placeholder.to_owned(),
                    (None, _) => name.clone(),
                };
                // Only a *dispatched* call spins. `Pending` keeps the oracle's `~`
                // because the two states differ in a way a user acts on: pending means
                // the model is still writing the arguments, running means the tool is
                // executing, and collapsing both into one animation would hide which.
                let glyph = if *status == ToolStatus::Running {
                    self.transcript.spinner()
                } else {
                    status.glyph()
                };
                let style = match status {
                    ToolStatus::Error => self.context.error(),
                    ToolStatus::Completed => self.context.success(),
                    ToolStatus::Pending | ToolStatus::Running => self.context.muted(),
                };
                push(&format!("{glyph} {icon} {label}"), style, out);
                if let Some(output) = output {
                    self.tool_output_lines(role, rule, output, width, out);
                }
            }
            MessagePart::Attachment { filename, mime } => {
                let label = match mime {
                    Some(mime) => format!("⎘ {filename} ({mime})"),
                    None => format!("⎘ {filename}"),
                };
                push(&label, self.context.accent(), out);
            }
            MessagePart::Retry { attempt, max } => {
                push(
                    &format!("↻ Retrying provider request (attempt {attempt}/{max})"),
                    self.context.error(),
                    out,
                );
            }
            MessagePart::Notice { text } => {
                for row in wrap(text, body_width.saturating_sub(2)) {
                    push(&format!("! {row}"), self.context.warning(), out);
                }
            }
        }
    }

    /// A tool's output, as a diff when it is one and as capped prose otherwise.
    ///
    /// The diff branch is why an `edit` is worth reading in the transcript at all: an
    /// unstyled patch is a wall of text whose `+` and `-` a reader has to scan for,
    /// and the same patch with line numbers and the theme's eleven diff colours is a
    /// review surface.
    fn tool_output_lines(
        &self,
        role: Role,
        rule: Style,
        output: &str,
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let gutter = u16::try_from(role.marker().chars().count() + 3).unwrap_or(4);
        let body_width = width.saturating_sub(gutter);
        let marker = role.marker();
        if looks_like_diff(output) {
            let mut view = crate::views::diff::DiffView::new(self.context.clone(), output);
            let rows = view.lines(body_width);
            let total = rows.len();
            for row in rows.into_iter().take(self.tool_output.rows()) {
                let mut spans = vec![Span::styled(format!("{marker}   "), rule)];
                spans.extend(row.spans);
                out.push(Line::from(spans));
            }
            self.push_overflow(role, rule, total, width, out);
            return;
        }
        let rows = wrap(output, body_width);
        let total = rows.len();
        for row in rows.into_iter().take(self.tool_output.rows()) {
            out.push(self.ruled(role, rule, &format!("  {row}"), self.context.muted(), width));
        }
        self.push_overflow(role, rule, total, width, out);
    }

    fn push_overflow(
        &self,
        role: Role,
        rule: Style,
        total: usize,
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let shown = self.tool_output.rows();
        if total <= shown {
            return;
        }
        let hidden = total - shown;
        let notice = match crate::views::key_label("tool_details", &self.context) {
            Some(key) => format!(
                "  {} {hidden} more lines · {key}",
                ThinkingDisplay::Collapsed.glyph()
            ),
            None => format!(
                "  {} {hidden} more lines",
                ThinkingDisplay::Collapsed.glyph()
            ),
        };
        out.push(self.ruled(role, rule, &notice, self.context.accent(), width));
    }
}

impl Component for TranscriptView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        let lines = self.lines(area.width);
        self.content_height = lines.len();
        self.viewport_height = usize::from(area.height);
        let max = self.content_height.saturating_sub(self.viewport_height);
        if self.offset > max {
            self.offset = max;
        }
        let visible = lines
            .into_iter()
            .skip(self.offset)
            .take(self.viewport_height)
            .collect::<Vec<_>>();
        Paragraph::new(visible)
            .style(self.context.surface())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        match event {
            AppEvent::Engine(turn) => {
                if self.transcript.observe(turn) {
                    EventResult::REDRAW
                } else {
                    EventResult::IGNORED
                }
            }
            AppEvent::Terminal(_) => EventResult::IGNORED,
        }
    }
}

/// A live status strip: what the turn is doing right now.
///
/// Separate from the transcript because it is derived state that changes on events
/// the transcript deliberately ignores — model resolution, request starts, step
/// completion — and because upstream draws it in its own region
/// (`routes/session/footer.tsx`).
pub struct StatusView {
    context: ViewContext,
    running: bool,
    agent: Option<String>,
    model: Option<String>,
    step: u32,
    detail: Option<String>,
    /// What the session was configured with, shown before the first turn resolves.
    ///
    /// Separate from `agent`/`model`, which are what the *engine* resolved: carrying
    /// the configured pair in the same fields would make the strip claim a turn had
    /// resolved a model before one had run, and clearing them at the end of a turn
    /// would then blank a row that is still true.
    configured_agent: Option<String>,
    configured_model: Option<String>,
    usage: TokenUsage,
}

/// Token counts for the session, accumulated across every step of every turn.
///
/// Cumulative rather than per-step because the number a user is watching for is what
/// the session has cost so far; a per-step count resets to a small number at every
/// step boundary and reads as if usage went down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt tokens sent.
    pub input: u64,
    /// Completion tokens received.
    pub output: u64,
    /// Prompt tokens served from the provider's cache.
    pub cache_read: u64,
    /// Prompt tokens written into the provider's cache.
    pub cache_write: u64,
}

impl TokenUsage {
    /// Whether anything has been counted yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }

    /// Every token the session has been billed for, cache included.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    /// Add one step's report.
    ///
    /// A provider that reports the same step twice — a retry replaying its usage
    /// event — would double-count, so the accumulator is fed from
    /// [`StreamEvent::TokenUsage`] only, which the engine emits once per completed
    /// step (`zuno-engine/src/loop.rs:530`).
    pub const fn add(&mut self, input: u64, output: u64, cache_read: u64, cache_write: u64) {
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_write += cache_write;
    }

    /// The compact form the status strip carries.
    ///
    /// Cache is named only when the provider reported some: a permanent `cache 0` on
    /// a provider that does not support caching is a column of noise.
    #[must_use]
    pub fn compact(&self) -> String {
        let cached = self.cache_read + self.cache_write;
        if cached == 0 {
            format!("↑{} ↓{}", thousands(self.input), thousands(self.output))
        } else {
            format!(
                "↑{} ↓{} ⚡{}",
                thousands(self.input),
                thousands(self.output),
                thousands(cached)
            )
        }
    }
}

/// Group `value` in thousands so a six-figure token count stays readable.
#[must_use]
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

impl StatusView {
    /// The word the strip shows when nothing is running.
    pub const IDLE: &'static str = "idle";

    /// The word the strip shows the instant a turn starts, before anything resolves.
    pub const WORKING: &'static str = "working";

    /// The key shown as the way out, and what it does while a turn is running.
    ///
    /// The strip is the one row always on screen, so it is where a binding a user
    /// cannot otherwise guess belongs. An application whose exit key is undiscoverable
    /// is only marginally better than one that has none.
    pub const EXIT_HINT: &'static str = "ctrl+c cancel/exit";

    /// A status strip over `context`.
    #[must_use]
    pub fn new(context: ViewContext) -> Self {
        Self {
            context,
            running: false,
            agent: None,
            model: None,
            step: 0,
            detail: None,
            configured_agent: None,
            configured_model: None,
            usage: TokenUsage::default(),
        }
    }

    /// Adopt the configured agent and model, so the idle strip is not just `idle`.
    ///
    /// Before this the strip's only pre-turn state was the literal word `idle`, which
    /// answers none of the questions a user has before pressing enter: which agent
    /// will run, and against which model. Both are known at launch.
    /// An empty string is treated as "not resolved" rather than adopted, because a
    /// blank agent on the strip is indistinguishable from one that failed to resolve.
    pub fn describe(&mut self, agent: &str, model: &str) {
        if !agent.is_empty() {
            self.configured_agent = Some(agent.to_owned());
        }
        if !model.is_empty() {
            self.configured_model = Some(model.to_owned());
        }
    }

    /// Replace the configured model, after the user picked a different one.
    pub fn set_configured_model(&mut self, model: impl Into<String>) {
        self.configured_model = Some(model.into());
    }

    /// Replace the configured agent, after the user picked a different one.
    pub fn set_configured_agent(&mut self, agent: impl Into<String>) {
        self.configured_agent = Some(agent.into());
    }

    /// Whether a turn is in flight, as the strip is reporting it.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// The tokens counted so far.
    #[must_use]
    pub const fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// Replace the palette and settings, for a live theme change.
    pub fn set_context(&mut self, context: ViewContext) {
        self.context = context;
    }

    /// Report a turn as running before the engine's first event arrives.
    ///
    /// A prompt has to be persisted before [`TurnEvent::TurnStarted`] can be sent, so
    /// there is a window in which work has started and no event has. Closing it here
    /// is not cosmetic: for that window the strip would otherwise read
    /// [`Self::IDLE`], which is the one thing it must never say while a turn is
    /// under way.
    pub fn mark_running(&mut self) {
        self.reset(true);
    }

    /// Discard everything a turn resolved and record whether one is now running.
    ///
    /// The strip reports what is happening, not what last happened. Carrying the
    /// previous turn's agent, model and step past its end would leave the one row a
    /// user glances at claiming a state the process is not in — the same defect as a
    /// strip that stayed [`Self::IDLE`] through a running turn, in the other
    /// direction.
    fn reset(&mut self, running: bool) {
        self.running = running;
        self.agent = None;
        self.model = None;
        self.step = 0;
        self.detail = None;
    }

    /// The right-hand group: token usage, then the exit hint.
    fn trailer(&self) -> String {
        if self.usage.is_empty() {
            return Self::EXIT_HINT.to_owned();
        }
        format!("{}  {}", self.usage.compact(), Self::EXIT_HINT)
    }

    /// The rendered row, with [`Self::EXIT_HINT`] right-aligned when it fits.
    ///
    /// The hint is dropped rather than truncated on a narrow terminal: half a key
    /// name is worse than none, and the turn state it shares the row with is what a
    /// user needs more.
    #[must_use]
    pub fn line(&self, width: u16) -> Line<'static> {
        let state = format!(" {}", self.state());
        let columns = usize::from(width);
        // The trailer is tried whole first, then the exit hint alone, then nothing.
        // Dropping the token counts before the exit key is the deliberate order: the
        // counts are informational and the key is the only way out.
        for trailer in [self.trailer(), Self::EXIT_HINT.to_owned()] {
            let used = state.chars().count() + trailer.chars().count() + 1;
            if used < columns {
                return Line::from(vec![
                    Span::styled(state, self.context.element()),
                    Span::styled(" ".repeat(columns - used), self.context.element()),
                    Span::styled(
                        trailer,
                        Style::new()
                            .fg(self.context.palette.text_muted.into())
                            .bg(self.context.palette.background_element.into()),
                    ),
                    Span::styled(String::from(" "), self.context.element()),
                ]);
            }
        }
        padded(&state, width, self.context.element())
    }

    fn state(&self) -> String {
        let mut text = String::new();
        if let Some(agent) = self.agent.as_ref().or(self.configured_agent.as_ref()) {
            text.push_str(agent);
        }
        if let Some(model) = self.model.as_ref().or(self.configured_model.as_ref()) {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(model);
        }
        if self.step > 0 {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(&format!("step {}", self.step));
        }
        if let Some(detail) = &self.detail {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(detail);
        }
        if text.is_empty() {
            text.push_str(if self.running {
                Self::WORKING
            } else {
                Self::IDLE
            });
        }
        text
    }
}

impl Component for StatusView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.element());
        Paragraph::new(vec![self.line(area.width)])
            .style(self.context.element())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        let AppEvent::Engine(turn) = event else {
            return EventResult::IGNORED;
        };
        match turn {
            TurnEvent::TurnStarted { .. } => {
                self.reset(true);
                EventResult::REDRAW
            }
            TurnEvent::AgentResolved { agent, .. } => {
                self.agent = Some(agent.clone());
                EventResult::REDRAW
            }
            TurnEvent::ModelResolved {
                provider_id,
                model_id,
                ..
            } => {
                self.model = Some(format!("{provider_id}/{model_id}"));
                EventResult::REDRAW
            }
            TurnEvent::StepCompleted { step, .. } => {
                self.step = *step;
                EventResult::REDRAW
            }
            TurnEvent::Provider {
                event: StreamEvent::StatusDetail { detail },
                ..
            } => {
                self.detail = Some(detail.clone());
                EventResult::REDRAW
            }
            TurnEvent::Provider {
                event:
                    StreamEvent::TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_write_input_tokens,
                    },
                ..
            } => {
                self.usage.add(
                    input_tokens.unwrap_or(0),
                    output_tokens.unwrap_or(0),
                    cache_read_input_tokens.unwrap_or(0),
                    cache_write_input_tokens.unwrap_or(0),
                );
                EventResult::REDRAW
            }
            TurnEvent::Provider {
                event: StreamEvent::Error { message, .. },
                ..
            } => {
                self.detail = Some(message.clone());
                EventResult::REDRAW
            }
            TurnEvent::TurnCompleted { .. } | TurnEvent::TurnInterrupted { .. } => {
                self.reset(false);
                EventResult::REDRAW
            }
            _ => EventResult::IGNORED,
        }
    }
}

/// A one-column vertical scrollbar, drawn beside a scrollable region.
///
/// Its own component so the `scrollbar_toggle` action can drop it without the
/// scrollable view knowing.
pub struct ScrollbarView {
    context: ViewContext,
    /// Total rows of content.
    pub total: usize,
    /// Rows visible.
    pub viewport: usize,
    /// First visible row.
    pub offset: usize,
}

impl ScrollbarView {
    /// A scrollbar over `context`.
    #[must_use]
    pub const fn new(context: ViewContext) -> Self {
        Self {
            context,
            total: 0,
            viewport: 0,
            offset: 0,
        }
    }
}

impl Component for ScrollbarView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let track = Style::new()
            .fg(self.context.palette.border_subtle.into())
            .bg(self.context.palette.background.into());
        let thumb = Style::new()
            .fg(self.context.palette.border_active.into())
            .bg(self.context.palette.background.into());
        fill(frame.buffer_mut(), area, track);
        let height = usize::from(area.height);
        if height == 0 || area.width == 0 {
            return;
        }
        for y in area.top()..area.bottom() {
            frame.buffer_mut()[(area.left(), y)].set_symbol(symbols::line::VERTICAL);
        }
        if self.total <= self.viewport || self.total == 0 {
            return;
        }
        let span = (self.viewport * height / self.total).max(1);
        let travel = height.saturating_sub(span);
        let scrollable = self.total.saturating_sub(self.viewport).max(1);
        let start = self.offset.min(scrollable) * travel / scrollable;
        for row in 0..span {
            let Ok(dy) = u16::try_from(start + row) else {
                break;
            };
            if dy >= area.height {
                break;
            }
            let cell = &mut frame.buffer_mut()[(area.left(), area.top() + dy)];
            cell.set_style(thumb);
            cell.set_symbol(symbols::block::FULL);
        }
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

/// The summary line a collapsed reasoning block shows.
///
/// The oracle's `reasoningSummary` takes the first meaningful line
/// (`context/thinking.ts`). Reasoning frequently opens with a markdown heading, so
/// the leading `#` run is dropped: a collapsed summary reading `### Plan` wastes
/// three of its few columns on punctuation.
#[must_use]
pub fn summary(text: &str) -> Option<String> {
    text.lines()
        .map(|line| line.trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(|line| {
            if line.chars().count() > 72 {
                let head = line.chars().take(71).collect::<String>();
                format!("{head}…")
            } else {
                line.to_owned()
            }
        })
}

/// Break `text` into rows no wider than `width`, on word boundaries where possible.
///
/// ratatui can wrap for us, but not while also letting the transcript *count* the
/// rows it will occupy — which the scroll offset and the scrollbar both need. So
/// the wrap happens here and the produced lines are handed over already broken.
#[must_use]
pub fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width).max(1);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        for word in paragraph.split(' ') {
            let mut word = word;
            // A word longer than the row is broken rather than allowed to overflow;
            // paths and URLs are common here and both are unbreakable on spaces.
            while word.chars().count() > width {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                let head = word.chars().take(width).collect::<String>();
                let consumed = head.len();
                rows.push(head);
                word = &word[consumed..];
            }
            if row.is_empty() {
                row.push_str(word);
            } else if row.chars().count() + 1 + word.chars().count() <= width {
                row.push(' ');
                row.push_str(word);
            } else {
                rows.push(std::mem::replace(&mut row, word.to_owned()));
            }
        }
        rows.push(row);
    }
    rows
}
