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
use crate::keybind::APP_EXIT;
use crate::views::{ViewContext, display_width, fill, key_label, padded, truncate};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, symbols};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
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
/// The first eight rows are verbatim from the oracle's `InlineTool` call sites: bash
/// `$`/"Writing command...", glob `✱`/"Finding files...", grep `✱`/"Searching
/// content...", read `→`/"Reading file...", write `→`/"Preparing write...", webfetch
/// `%`/"Fetching from the web...", websearch `◈`/"Searching web...", task
/// `#`/"Delegating..." (`index.tsx:2090,2138,2186,2163,2124,2198,2206,2296`), with `⚙`
/// as the generic fallback (`index.tsx:1808`).
///
/// The rest are this project's, because this project registers tools the oracle does not
/// (`memory`, the three goal tools) and exposes slots the oracle reaches only through a
/// palette. They are added rather than left on `⚙` for the reason
/// [`crate::views::tool::summary`] exists at all: a column of identical `⚙` rows is a
/// column a reader cannot scan.
///
/// **`apply_patch`, not `patch`.** The arm here read `"patch"` and could therefore never
/// match: `BuiltinSlot::Patch::wire_id()` is `apply_patch`, so every patch call in this
/// project's history rendered as the generic `⚙`. That is the same class of defect as
/// `editor_open`'s unreachable binding — a hand-written name that no longer agreed with
/// the table it was copied from — which is why
/// `tool_summaries_cover_every_tool_the_registry_can_expose` now reads the names out of
/// the registry's own source instead of trusting this list.
///
/// Every icon is one terminal column wide, so a column of tool rows aligns without the
/// caller having to measure. Widths are still measured downstream; this only means the
/// table does not *depend* on that.
#[must_use]
pub fn tool_affordance(name: &str) -> (&'static str, &'static str) {
    match name {
        "bash" => ("$", "Writing command..."),
        "glob" => ("✱", "Finding files..."),
        "grep" => ("✱", "Searching content..."),
        "read" => ("→", "Reading file..."),
        "write" | "edit" => ("→", "Preparing write..."),
        "webfetch" => ("%", "Fetching from the web..."),
        "websearch" => ("◈", "Searching web..."),
        "task" => ("#", "Delegating..."),
        // A patch is a write, so it shares the write arrow rather than inventing a glyph:
        // the two differ in how the change is expressed, not in what happens to the file.
        "apply_patch" => ("→", "Preparing patch..."),
        // A ballot box for the plan, which is what a todo list is here.
        "todowrite" => ("☑", "Updating plan..."),
        // The only tool that is about to block on the user, so it gets the one glyph that
        // reads as a question.
        "question" => ("?", "Asking..."),
        "skill" => ("✦", "Loading skill..."),
        "lsp" => ("⌁", "Querying language server..."),
        // Leaving plan mode is a transition, and the tab arrow is this codebase's
        // vocabulary for one.
        "plan_exit" => ("⇥", "Leaving plan mode..."),
        // Nested calls, so a nesting mark: `execute` is the only tool whose arguments are
        // other tools.
        "execute" => ("»", "Batching..."),
        // Not the status glyph `✗`, which says *this call* failed. `invalid` is a call the
        // model should not have made at all, and the two are worth telling apart.
        "invalid" => ("!", "Rejecting..."),
        "memory" => ("≡", "Updating memory..."),
        // One glyph for all three goal tools: they read, set and amend one object, and
        // three glyphs would imply three subjects.
        "get_goal" | "create_goal" | "update_goal" => ("◎", "Reading the goal..."),
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

/// Diagnostics a collapsed report lists.
///
/// Fewer than a tool result's rows because a diagnostic row is dense and the summary
/// above it already carries the counts; the `tool_details` action expands both together,
/// since a user asking for detail wants it everywhere rather than per part.
pub const DIAGNOSTICS_PREVIEW_ROWS: usize = 4;

/// Diagnostics an expanded report lists.
pub const DIAGNOSTICS_MAX_ROWS: usize = 200;

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

/// Columns a notice's `! ` marker takes out of the body.
///
/// Subtracted before wrapping rather than after, because the marker is prefixed to every
/// row: wrapping to the full body width and then adding two columns puts the last two
/// columns of each row past the frame, and against the ambient sidebar that reads as the
/// panel having cut the sentence.
const NOTICE_MARKER_COLS: u16 = 2;

/// A notice's colour for `level`, on the transcript's own background.
///
/// Deliberately not [`crate::views::toast::ToastLevel`]'s own style: a toast floats and takes
/// the inset background with it, while a notice is a transcript row and must sit on the same
/// surface as the message above it. The *foreground* mapping is the one `§11.5` fixes, and it
/// is the same one on both surfaces.
fn notice_style(level: crate::views::toast::ToastLevel, context: &ViewContext) -> Style {
    use crate::views::toast::ToastLevel;
    match level {
        ToastLevel::Info => context.muted(),
        ToastLevel::Success => context.success(),
        ToastLevel::Warning => context.warning(),
        ToastLevel::Error => context.error(),
    }
}

/// Rows one notice may occupy before the rest is counted instead of drawn.
///
/// Derived, not picked. The longest notice this crate composes is the 72-column
/// `MCP server ... is busy or unavailable`; at 40 columns — the narrowest width the layout
/// is accepted at — that wraps to three rows, so five clears every authored notice with
/// headroom and bites only on text this crate did not write, such as a provider error
/// interpolated into one of these sentences. That case is the one worth bounding because it
/// is unbounded: measured without a cap, one long MCP failure took 17 of the 21 transcript
/// rows on a 24-column pane, evicting the reply it was annotating.
///
/// The fifth row states the count rather than showing more text, because a reader cannot
/// tell a wrapped row from a cut one and an honest `… N more lines` says which it was.
pub const NOTICE_MAX_ROWS: usize = 5;

/// The mark that says content was cut rather than absent.
///
/// Named so the transcript's collapse notice and [`crate::views::truncate`]'s cut make
/// the same promise with the same character: one vocabulary for "there is more here"
/// across every surface, which is what lets a reader learn it once.
pub const ELIDED: &str = "…";

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
        /// The raw JSON arguments, accumulated from the provider's input deltas.
        ///
        /// Raw and not parsed, because the deltas arrive as a byte stream and a *prefix*
        /// of a JSON object is not a JSON object: parsing per delta would fail on every
        /// one but the last. [`crate::views::tool::summary`] parses when it renders and
        /// treats failure as "not yet", which is what makes a half-written argument
        /// render as the placeholder instead of as an error.
        ///
        /// This is the transcript's own accumulator rather than a field on the engine
        /// event, and that is the whole reason the per-tool summary is renderable at all.
        /// [`TurnEvent::ToolDispatchCompleted`] carries `title`, `output` and `diff` and
        /// **not** the arguments, so nothing downstream of the engine knew which file a
        /// `read` had read. But [`StreamEvent::ToolInputDelta`] does reach here, through
        /// [`TurnEvent::Provider`], and was being dropped on the floor. Folding it costs
        /// one `String` per call and keeps the change inside this crate.
        arguments: String,
        /// The human-readable title, once the call completes.
        title: Option<String>,
        /// How far it has got.
        status: ToolStatus,
        /// The tool's output, once it completes.
        output: Option<String>,
        /// The unified patch it produced, when it changed a file.
        ///
        /// Separate from `output` because a mutating tool's output is a sentence — the
        /// patch travels beside it as `metadata["diff"]` and arrives here through
        /// [`zuno_engine::r#loop::TurnEvent::ToolDispatchCompleted`]. Keeping the two
        /// apart is what lets the transcript print one line while the diff viewer opens
        /// on the whole change.
        diff: Option<String>,
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
        /// What kind of fact it is, which decides its glyph and its colour.
        ///
        /// [`crate::views::toast::ToastLevel`] and not a second enum, so a notice and a
        /// toast cannot disagree about what `§11.5`'s four levels look like. Every notice
        /// was drawn with the warning marker before this field existed, which announced
        /// `model set to …` — a confirmation — as `!` in warning colour.
        level: crate::views::toast::ToastLevel,
    },
    /// What a language server said about a file the turn touched.
    ///
    /// Its own part rather than a notice because it has structure a notice does not —
    /// severity, position, and the distinction between "checked and clean" and "nobody
    /// checked" — and because it is styled by severity rather than as one warning.
    Diagnostics {
        /// The report, already sorted worst-first.
        report: crate::views::lsp::Report,
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

    /// A session message carrying one language-server report.
    #[must_use]
    pub fn diagnostics(report: crate::views::lsp::Report) -> Self {
        Self {
            role: Role::System,
            id: None,
            parts: vec![MessagePart::Diagnostics { report }],
        }
    }

    /// A session notice at `level`, carrying one line the user has to see.
    #[must_use]
    pub fn noticed(level: crate::views::toast::ToastLevel, text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            id: None,
            parts: vec![MessagePart::Notice {
                text: text.into(),
                level,
            }],
        }
    }

    /// A session notice reporting something the user can act on.
    ///
    /// [`ToastLevel::Warning`](crate::views::toast::ToastLevel::Warning), because that is
    /// what the majority of this crate's notices are — a refusal, an unknown command, an
    /// unavailable worker. Anything that *succeeded* or that merely states a fact says so
    /// with [`Self::noticed`]; the levels are not interchangeable, and `§11.5` reserves the
    /// warning colour for something the user has to do something about.
    #[must_use]
    pub fn notice(text: impl Into<String>) -> Self {
        Self::noticed(crate::views::toast::ToastLevel::Warning, text)
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
    /// The whole prompt the most recent request sent.
    ///
    /// Separate from [`Self::tokens`], and replaced rather than accumulated, because the
    /// two answer different questions: `tokens` is what the session has been billed for
    /// so far, and this is what is currently in the window. Deriving the second from the
    /// first is the defect [`Transcript::context_used`] documents — a cumulative figure
    /// passes any window on the second turn.
    last_prompt_tokens: u64,
    /// The model's context ceiling, when the catalog states one.
    context_limit: u64,
    /// How many events have been folded, which is what advances the spinner.
    ticks: usize,
    /// Whether a permission prompt is asking the user to decide right now.
    ///
    /// Not folded from an engine event, because a parked ask produces none: the
    /// dispatcher blocks inside `ctx.ask` and the engine's last word was
    /// `ToolDispatchStarted`, which is equally true of a shell command that is simply
    /// slow. The dialog stack is the only thing that knows the difference, so it is what
    /// sets this.
    awaiting_permission: bool,
}

impl Transcript {
    /// What the transcript says instead of the spinner while a permission ask is open.
    ///
    /// Phrased as an instruction rather than a state — "waiting" alone leaves the user
    /// looking for what to wait for, when they are the thing being waited on.
    pub const AWAITING_PERMISSION: &'static str = "△ waiting for your approval";

    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record whether a permission prompt is asking the user to decide.
    ///
    /// Returns whether the answer changed, which is what a caller turns into a redraw.
    pub const fn set_awaiting_permission(&mut self, awaiting: bool) -> bool {
        let changed = self.awaiting_permission != awaiting;
        self.awaiting_permission = awaiting;
        changed
    }

    /// Whether a permission prompt is currently asking the user to decide.
    #[must_use]
    pub const fn is_awaiting_permission(&self) -> bool {
        self.awaiting_permission
    }

    /// The messages so far.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether either party has actually said anything yet.
    ///
    /// [`Role::System`] is excluded, and that exclusion is the whole point: a session
    /// notice is something the *program* said about itself, not a turn. Startup
    /// diagnostics — a theme that fell back, an unreadable prompt history — are pushed
    /// before the first frame, so a predicate that merely asked whether the buffer held
    /// any message reported "the conversation has begun" on a session where nothing had
    /// happened. The welcome surface is drawn under exactly that predicate, so one
    /// recoverable warning silently cost the wordmark, the hint grid, the hidden sidebar
    /// and the composer's centring all at once — reported by the owner as a screen of
    /// warnings with no welcome screen behind them.
    ///
    /// Defined here, on the transcript, rather than at either of the two places in
    /// `session.rs` that need it: those two must never disagree about whether the
    /// welcome screen is in force, because one sizes the centring tail and the other
    /// decides what fills the body.
    #[must_use]
    pub fn conversation_started(&self) -> bool {
        self.messages
            .iter()
            .any(|message| matches!(message.role, Role::User | Role::Assistant))
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

    /// The whole prompt the most recent request sent, as the provider counted it.
    ///
    /// Replaced on every usage report rather than accumulated, which is the distinction
    /// [`Self::context_used`] needs and [`Self::tokens`] cannot provide.
    #[must_use]
    pub const fn last_prompt_tokens(&self) -> u64 {
        self.last_prompt_tokens
    }

    /// How full the context window is, as a percentage, when a ceiling is known.
    ///
    /// Computed from [`Self::last_prompt_tokens`] — the last request's whole prompt —
    /// never from [`Self::tokens`], which is cumulative over the session. This used to
    /// read `tokens.input + tokens.cache_read`, and because `TokenUsage::add` is `+=`
    /// per step, two 80k-token prompts against a 128k window displayed `125%`: a number
    /// that cannot happen, on the models where the figure matters most.
    ///
    /// A zero ceiling is "no window declared" — see `token_count` in the CLI's turn
    /// plan, which maps a non-finite catalog limit to zero — so it yields `None`
    /// rather than dividing.
    #[must_use]
    pub const fn context_used(&self) -> Option<u64> {
        if self.context_limit == 0 {
            return None;
        }
        Some(self.last_prompt_tokens.saturating_mul(100) / self.context_limit)
    }

    /// The spinner frame this transcript is on.
    #[must_use]
    pub const fn spinner(&self) -> &'static str {
        SPINNER[self.ticks % SPINNER.len()]
    }

    /// The most recent patch a tool reported.
    ///
    /// Searched newest-first because the interesting patch is the one just produced. Two
    /// sources, in this order:
    ///
    /// 1. the tool's own `diff`, which every file-mutating tool now attaches — this is
    ///    the only source that works for `edit`, `write` and `patch`, whose output is a
    ///    sentence rather than a patch;
    /// 2. failing that, an output that *is* a patch, recognised with the same
    ///    [`looks_like_diff`] the transcript colours by — which is how a `git diff` run
    ///    through the shell tool still opens here.
    ///
    /// Before the first source existed this method could only ever return the second, so
    /// the viewer was permanently empty for the one tool that edits code.
    #[must_use]
    pub fn latest_diff(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .flat_map(|message| message.parts.iter().rev())
            .find_map(|part| match part {
                MessagePart::Tool {
                    diff: Some(patch), ..
                } => Some(patch.clone()),
                MessagePart::Tool {
                    output: Some(output),
                    ..
                } if looks_like_diff(output) => Some(output.clone()),
                _ => None,
            })
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
                        arguments: String::new(),
                        title: None,
                        status: ToolStatus::Running,
                        output: None,
                        diff: None,
                    });
                    true
                }
            }
            TurnEvent::ToolDispatchCompleted {
                call_id,
                title,
                output,
                diff,
                is_error,
                ..
            } => self.update_tool(call_id, |part| {
                if let MessagePart::Tool {
                    status,
                    title: slot,
                    output: body,
                    diff: patch,
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
                    *patch = diff.clone();
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
                    arguments: String::new(),
                    title: None,
                    status: ToolStatus::Pending,
                    output: None,
                    diff: None,
                });
                true
            }
            // The provider writes the arguments one fragment at a time, and this is the
            // only place they are ever visible to the view layer — see
            // [`MessagePart::Tool::arguments`]. Reporting a redraw on every fragment is
            // right rather than chatty: the summary grows as the JSON completes, so the
            // row genuinely changes.
            StreamEvent::ToolInputDelta(delta) => {
                if let Some(MessagePart::Tool { arguments, .. }) = self.last_tool_mut() {
                    arguments.push_str(delta);
                    true
                } else {
                    false
                }
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
                accounting,
            } => {
                let input = input_tokens.unwrap_or_default();
                let cache_read = cache_read_input_tokens.unwrap_or_default();
                let cache_write = cache_write_input_tokens.unwrap_or_default();
                // Replaced, not added: this is the window's current occupancy.
                self.last_prompt_tokens = accounting.prompt_total(input, cache_read, cache_write);
                self.tokens.add(
                    accounting.uncached_input(input, cache_read, cache_write),
                    output_tokens.unwrap_or_default(),
                    cache_read,
                    cache_write,
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

    /// The tool part currently receiving argument fragments.
    ///
    /// Searched backwards for a `Tool` rather than taking the last part outright, the same
    /// way [`StreamEvent::ToolUseSignature`] is handled: a provider that interleaves a
    /// text delta between `ToolUseStart` and the input deltas would otherwise append the
    /// arguments to the prose.
    fn last_tool_mut(&mut self) -> Option<&mut MessagePart> {
        let index = self.streaming?;
        self.messages
            .get_mut(index)?
            .parts
            .iter_mut()
            .rev()
            .find(|part| matches!(part, MessagePart::Tool { .. }))
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
    /// Recalled rows, per message.
    ///
    /// Owned by the view rather than shared, so its bound is per view and one screen's
    /// transcript cannot evict another's. See [`Self::cached_lines`] for the key and
    /// [`MAX_CACHED_ROWS`] for the bound.
    cache: RowCache,
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
            cache: RowCache::default(),
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

    #[cfg(test)]
    const fn cache(&self) -> &RowCache {
        &self.cache
    }

    #[cfg(test)]
    fn cached_lines_for_test(&mut self, width: u16) -> Vec<Line<'static>> {
        self.cached_lines(width)
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
    ///
    /// # This is the specification, and [`Self::cached_lines`] is the implementation
    ///
    /// It consults no cache and mutates nothing, so it stays a pure function of the
    /// transcript, the width and the palette. That is deliberate and it is what
    /// [`Self::cached_lines`] is checked against: `views_transcript_cache_returns_what_the_uncached_path_would`
    /// renders every state both ways and requires span-for-span equality. A cache whose
    /// only description of correct output is the cache itself cannot be checked at all,
    /// which is why the uncached path is kept in the shipping code rather than deleted
    /// once the cache worked.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut previous: Option<Role> = None;
        for message in &self.transcript.messages {
            lines.append(&mut self.message_rows(message, previous, width));
            previous = Some(message.role);
        }
        self.push_trailer(&mut lines, previous.is_some(), width);
        lines
    }

    /// One message's rows: its separator or header, then each of its parts.
    ///
    /// Factored out of [`Self::lines`] rather than duplicated into the cached path,
    /// because "the two paths produce the same rows" is a property worth having
    /// structurally rather than by inspection. Both callers reach the frame through
    /// exactly this function, so the only difference between them is whether the rows
    /// were computed now or recalled.
    fn message_rows(
        &self,
        message: &Message,
        previous: Option<Role>,
        width: u16,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let rule = self.rule_style(message.role);
        // A multi-step turn opens one assistant message per step, so a header per
        // message printed `Assistant` five times for what the user experienced as
        // one reply. The header marks a change of speaker, which is what it was
        // always for; the left rule already runs down every row of the turn.
        if previous == Some(message.role) {
            // The same speaker again, which is the *inside* of one reply rather than a
            // boundary between two. Its separator carries the rule, because a bare
            // blank row here cut the rule into one fragment per step and so broke the
            // very continuity the header was suppressed in order to preserve — the
            // claim above was false for every multi-step turn.
            lines.push(self.ruled(message.role, rule, "", self.context.surface(), width));
        } else {
            // A change of speaker is the one boundary a reader scans for, so it gets
            // the stronger of the two separators: a row with no rule at all. Two
            // grades of gap is what lets the eye tell "the other party is talking now"
            // from "this reply took another step" without reading either row.
            if previous.is_some() {
                lines.push(padded("", width, self.context.surface()));
            }
            lines.push(self.ruled(
                message.role,
                rule,
                self.role_label(message.role),
                self.context.title(),
                width,
            ));
        }
        for part in &message.parts {
            self.part_lines(message.role, rule, part, width, &mut lines);
        }
        lines
    }

    /// The bottom margin and whichever liveness row the turn's state calls for.
    ///
    /// Never cached, and it is the reason the cache is per message rather than per
    /// frame: the spinner advances on every folded event, so a frame-level entry would
    /// miss on every single delta of a streaming turn — the exact case the cache exists
    /// to make cheap.
    fn push_trailer(&self, lines: &mut Vec<Line<'static>>, any_message: bool, width: u16) {
        if any_message {
            // The transcript's own bottom margin, so the spinner or the approval notice
            // below is not flush against the last row of the reply.
            lines.push(padded("", width, self.context.surface()));
        }
        // `working` and `waiting for you` are mutually exclusive claims about who the
        // turn is blocked on, so exactly one of them may be on screen. A spinner beside
        // an open permission prompt says the process is busy while it is in fact idle,
        // waiting for the very key press the prompt is asking for.
        if self.transcript.awaiting_permission {
            lines.push(padded(
                Transcript::AWAITING_PERMISSION,
                width,
                self.context.warning(),
            ));
        } else if self.transcript.running {
            lines.push(padded(
                &format!("{} working", self.transcript.spinner()),
                width,
                self.context.accent(),
            ));
        }
    }

    /// The same rows as [`Self::lines`], recalling every message whose inputs are unchanged.
    ///
    /// # What this fixes, measured
    ///
    /// Every frame re-rendered every message, so both of the transcript's hot paths cost
    /// the whole transcript. Measured on this project at 100 columns over five runs
    /// (`crates/zuno-tui/tests/render_cost.rs`, recorded in `docs/perf-methodology.md`),
    /// with the syntax-highlight configuration already memoised: one frame of a
    /// 931-message transcript took a median 63.128 ms, while the tail a streaming delta
    /// actually changed accounted for 0.24% of it. A keystroke in the editor forces the
    /// same frame and changes no message at all.
    ///
    /// That is the plan's O(n²): F frames of a streaming turn each doing O(n) work for an
    /// O(1) change. Recalling per message removes it without a longest-common-prefix
    /// search — exact, prefix and suffix reuse all fall out of keying per message, which
    /// is why §6.2's four-way `build_body_from_base` shape is not reproduced here.
    ///
    /// # The invalidation key, and why each part of it is present
    ///
    /// An entry is used only when every one of these matches:
    ///
    /// * **the resolved theme, by `Arc` identity.** [`crate::views::ViewContext`] holds
    ///   `Arc<RwLock<Arc<Resolved>>>` and `set_theme` installs a *new* `Arc`, so a pointer
    ///   comparison is a complete test for "the palette changed" — including
    ///   `thinking_opacity`, which `Palette::entries` does not report and which a
    ///   field-by-field hash would therefore have missed. Comparing addresses is only
    ///   sound because the entry *holds* the `Arc` it rendered with: a held `Arc` cannot be
    ///   freed, so its address cannot be reused by a later theme, which is what would
    ///   otherwise make this an ABA hazard.
    /// * **the width**, since every row is laid out and padded to it.
    /// * **the two display affordances**, `thinking` and `tool_output`, which decide how
    ///   many rows a reasoning block and a tool result produce.
    /// * **the preceding role**, which decides whether the message opens with a header or
    ///   with a same-speaker separator.
    /// * **a fingerprint of the message's content**, via [`fingerprint`]. Derived from the
    ///   parts rather than tracked as a revision counter on purpose: the fold mutates
    ///   parts in place from several places (`observe_stream`, `update_tool`,
    ///   `close_reasoning`), and a counter is a thing a future edit can forget to bump,
    ///   whose failure mode is a frame showing content the transcript no longer holds. A
    ///   fingerprint cannot be forgotten because it is read from the content itself. It is
    ///   the same reasoning that derives the modal banner every frame instead of pushing it
    ///   on open and close.
    ///
    /// One dependency is deliberately **not** in the key: `self.context.config`, which
    /// reaches the rows through the diff style and through the key spelling in a collapsed
    /// tool result's overflow notice. [`crate::views::ViewContext`] documents that the
    /// resolved configuration is owned per clone and never changes after startup, so it is
    /// constant for this view's whole life. If that ever stops being true this key becomes
    /// incomplete.
    ///
    /// A message whose tool call is [`ToolStatus::Running`] is never stored, because its
    /// glyph is the spinner and so depends on the fold's event count rather than on the
    /// message. `Pending` is stored: its glyph is a constant.
    fn cached_lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let theme = self.context.theme();
        let mut lines = Vec::new();
        let mut previous: Option<Role> = None;
        for index in 0..self.transcript.messages.len() {
            let message = &self.transcript.messages[index];
            let key = RowKey {
                width,
                thinking: self.thinking,
                tool_output: self.tool_output,
                previous,
                content: fingerprint(message),
            };
            previous = Some(message.role);
            if let Some(rows) = self.cache.get(index, &key, &theme) {
                lines.extend(rows.iter().cloned());
                continue;
            }
            let rows = self.message_rows(&self.transcript.messages[index], key.previous, width);
            lines.extend(rows.iter().cloned());
            if is_recallable(&self.transcript.messages[index]) {
                self.cache.put(index, key, Arc::clone(&theme), rows);
            } else {
                self.cache.forget(index);
            }
        }
        self.cache.truncate_to(self.transcript.messages.len());
        self.push_trailer(&mut lines, previous.is_some(), width);
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
                .fg(self.context.palette().border_subtle.into())
                .bg(self.context.palette().background_panel.into()),
            Role::System => self.context.warning(),
        }
    }

    /// One row carrying the role's left rule, then `body` in `style`.
    ///
    /// Two spans rather than one padded string because the rule and the body are
    /// different colours; a single span could only be one, which is precisely how the
    /// old renderer ended up with a transcript that had no visible structure.
    ///
    /// Both spans are measured in terminal columns, the same rule [`padded`] follows and
    /// for the same reason. Taking `room` *characters* of a CJK body produced a row about
    /// twice as wide as the frame, which ratatui then clipped: the user's own Chinese
    /// prompt lost its tail on screen while the wrap above had already accounted for it,
    /// so the missing text was not on the next row either.
    fn ruled(
        &self,
        role: Role,
        rule: Style,
        body: &str,
        style: Style,
        width: u16,
    ) -> Line<'static> {
        let marker = role.marker();
        let gutter = display_width(marker) + 1;
        let columns = usize::from(width);
        let room = columns.saturating_sub(gutter);
        let mut text = truncate(body, room);
        let used = display_width(&text);
        if used < room {
            text.extend(std::iter::repeat_n(' ', room - used));
        }
        Line::from(vec![
            Span::styled(format!("{marker} "), rule),
            Span::styled(text, style),
        ])
    }

    /// One row carrying the role's left rule, then pre-styled `spans`.
    ///
    /// The [`Self::ruled`] shape for content that is already several colours: markdown
    /// prose is a heading, a bullet and a code frame in one row, and a `&str` plus one
    /// `Style` cannot say that. Both functions pad to the same width and both measure in
    /// terminal columns, so a markdown row and a plain one occupy the frame identically.
    fn ruled_spans(
        &self,
        role: Role,
        rule: Style,
        spans: Vec<Span<'static>>,
        width: u16,
    ) -> Line<'static> {
        let marker = role.marker();
        let gutter = display_width(marker) + 1;
        let room = usize::from(width).saturating_sub(gutter);
        let mut out = vec![Span::styled(format!("{marker} "), rule)];
        let mut body = crate::views::markdown::truncate_row(spans, room);
        let used = crate::views::markdown::row_width(&body);
        if used < room {
            body.push(Span::styled(
                " ".repeat(room - used),
                self.context.surface(),
            ));
        }
        out.append(&mut body);
        Line::from(out)
    }

    fn part_lines(
        &self,
        role: Role,
        rule: Style,
        part: &MessagePart,
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let gutter = u16::try_from(display_width(role.marker()) + 1).unwrap_or(2);
        let body_width = width.saturating_sub(gutter);
        let push = |body: &str, style: Style, out: &mut Vec<Line<'static>>| {
            out.push(self.ruled(role, rule, body, style, width));
        };
        match part {
            // Only the assistant's prose is parsed as markdown. A user's prompt is taken
            // literally: someone typing `**maybe**` about a shell glob meant the
            // asterisks, and re-rendering their own input in a shape they did not write
            // is the surface editing them. A session notice is composed here rather than
            // by a model, so there is no markup in it to find.
            MessagePart::Text { text } if role == Role::Assistant => {
                for row in crate::views::markdown::render(text, body_width, &self.context.palette())
                {
                    out.push(self.ruled_spans(role, rule, row, width));
                }
            }
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
                // Present tense while the deltas are still arriving, past tense once they
                // stop. `thought for 2.5s` printed beside a block that is still growing is
                // a claim about a finished action that has not finished, and the duration
                // it quotes is the one the provider has not reported yet.
                let header = match (duration_secs, streaming) {
                    (_, true) => String::from("thinking…"),
                    (Some(secs), false) => format!("thought for {secs:.1}s"),
                    (None, false) => String::from("thought"),
                };
                // Inset one column past the prose, the same inset a tool call takes. Both are
                // things that happened *inside* the reply rather than being the reply, so
                // they share one indentation vocabulary: a reader learns the rule once and
                // then reads column 2 as "the answer" and column 3 as "the work". Flush with
                // the prose, a `thought for 12.0s` header sat exactly where the first
                // sentence of the answer sits and competed with it for the eye.
                let header = format!(" {} {header}", self.thinking.glyph());
                match self.thinking {
                    ThinkingDisplay::Collapsed => {
                        // One row, not two. Reasoning is secondary content that recurs on
                        // every step of every turn, so a collapsed block that spent two
                        // rows could out-measure the answer it precedes. The summary rides
                        // on the header instead of owning a row, and is dropped rather
                        // than wrapped when it does not fit: the glyph and the duration
                        // are what the row is for, and a summary continued onto a second
                        // row would spend exactly the row this form exists to save.
                        let row = match summary(text) {
                            Some(gist)
                                if display_width(&header) + 3 + display_width(&gist)
                                    <= usize::from(body_width) =>
                            {
                                format!("{header} · {gist}")
                            }
                            _ => header,
                        };
                        push(&row, self.context.thinking(), out);
                    }
                    ThinkingDisplay::Expanded => {
                        push(&header, self.context.thinking(), out);
                        // Aligned under the header's glyph, the same relationship a tool
                        // result has to its call row. Expanded reasoning is routinely longer
                        // than the answer, so its body has to be unmistakably a nested block
                        // — otherwise a long thought reads as the reply and the reply reads
                        // as a footnote to it.
                        // Italic on the body and not on the header: the body is the part
                        // that must read as subordinate to the answer below it, while the
                        // header is the affordance a user aims at and wants crisp. A
                        // terminal without italics loses nothing it had before, because
                        // the indent and the dimmed colour already carried the hierarchy.
                        let body = self.context.thinking().add_modifier(Modifier::ITALIC);
                        let inset = Self::RESULT_INSET;
                        let inset_columns = u16::try_from(display_width(inset)).unwrap_or(3);
                        for row in wrap(text, body_width.saturating_sub(inset_columns)) {
                            push(&format!("{inset}{row}"), body, out);
                        }
                    }
                }
            }
            MessagePart::Tool {
                name,
                arguments,
                title,
                status,
                output,
                diff,
                ..
            } => {
                let (icon, placeholder) = tool_affordance(name);
                // Only a *dispatched* call spins. `Pending` keeps the oracle's `~`
                // because the two states differ in a way a user acts on: pending means
                // the model is still writing the arguments, running means the tool is
                // executing, and collapsing both into one animation would hide which.
                let glyph = if *status == ToolStatus::Running {
                    self.transcript.spinner()
                } else {
                    status.glyph()
                };
                // `" {glyph} {icon} {name}"` — one leading space, so the tool block sits one
                // column inside the assistant's prose. That inset plus the two-column rule
                // is §7.5's three-column tool indent, and it is what makes a tool call read
                // as something the reply *did* rather than as another paragraph of it.
                let head = format!(" {glyph} {icon} {name}");
                // The tool's wire name plus the argument that matters, which is the whole of
                // §7.5. `title` is no longer preferred over the arguments: a completed
                // `read` reported `Read diff.rs`, which names the kind of work and drops the
                // path, so six reads in one turn produced six rows a reader could not tell
                // apart. The name stays beside the summary because the summary alone is
                // ambiguous — `crates/…/diff.rs` could be a read, a write or a patch — and
                // one icon does not carry that much resolution.
                //
                // `title` remains the fallback for a completed call whose arguments never
                // parsed, because a provider's own sentence beats a bare wire name.
                let row = match (crate::views::tool::summary(name, arguments), title, status) {
                    (Some(summary), _, _) => {
                        // Measured against what the head actually spent, not against a
                        // constant: the name's width runs from `read` to `update_goal`, and
                        // the summary has to be fitted to what is left after it. One more
                        // column is charged for the space that joins them.
                        let room = usize::from(body_width)
                            .saturating_sub(display_width(&head))
                            .saturating_sub(1);
                        format!("{head} {}", summary.fit(room))
                    }
                    (None, Some(title), _) => format!(" {glyph} {icon} {title}"),
                    (None, None, ToolStatus::Pending) => format!(" {glyph} {icon} {placeholder}"),
                    (None, None, _) => head,
                };
                push(
                    &row,
                    crate::views::tool::status_style(*status, &self.context),
                    out,
                );
                // A patch travels beside the output rather than inside it, so a result that
                // has one is rendered from it — and before this it was rendered from neither.
                // `tool_output_lines` only ever diff-sniffed `output`, and every mutating
                // tool's output is a *sentence* (`applied 1 change`), so the patch that
                // `TurnEvent::ToolDispatchCompleted` had faithfully carried all the way here
                // was dropped on the floor at the last step. The diff viewer could open it;
                // the transcript could not show it.
                let frame = RowFrame { role, rule, width };
                match (diff, output) {
                    (Some(patch), _) => {
                        self.tool_result_lines(frame, name, patch, *status, out);
                    }
                    (None, Some(output)) => {
                        self.tool_result_lines(frame, name, output, *status, out);
                    }
                    (None, None) => {}
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
                // `warning`, not `error`. A retry is a recovery under way and the turn
                // still usually succeeds, whereas `error` is what a failed tool call and a
                // failed turn render in — and a user scanning a transcript for red needs
                // those two answers to look different. Compact for a measured reason: the
                // old sentence `↻ Retrying provider request (attempt 2/3)` ran to 45
                // columns and was clipped at 40 after `attempt 2`, so the count it existed
                // to state was the first thing cut.
                push(
                    &format!("⟳ retry {attempt}/{max}"),
                    self.context.warning(),
                    out,
                );
            }
            MessagePart::Notice { text, level } => {
                let rows = wrap(text, body_width.saturating_sub(NOTICE_MARKER_COLS));
                let (shown, elided) = if rows.len() <= NOTICE_MAX_ROWS {
                    (rows.as_slice(), 0)
                } else {
                    let keep = NOTICE_MAX_ROWS - 1;
                    (&rows[..keep], rows.len() - keep)
                };
                let marker = level.glyph();
                let style = notice_style(*level, &self.context);
                for row in shown {
                    push(&format!("{marker} {row}"), style, out);
                }
                if elided > 0 {
                    push(
                        &format!("{marker} {ELIDED} {elided} more lines"),
                        self.context.muted(),
                        out,
                    );
                }
            }
            MessagePart::Diagnostics { report } => {
                let limit = match self.tool_output {
                    ToolDisplay::Collapsed => DIAGNOSTICS_PREVIEW_ROWS,
                    ToolDisplay::Expanded => DIAGNOSTICS_MAX_ROWS,
                };
                out.extend(report.lines(width, limit, &self.context));
            }
        }
    }

    /// The inset a tool result's rows are laid out at.
    ///
    /// Three columns past the rule, so a result row starts under its call row's icon
    /// rather than under the call row's own inset. Rule (2) plus this (3) is §7.5's
    /// five-column continuation indent, and the alignment under the icon is what makes a
    /// long result read as belonging to the call above it instead of floating between two.
    const RESULT_INSET: &'static str = "   ";

    /// A tool's result, as a diff when it is one and as budgeted prose otherwise.
    ///
    /// The diff branch is why an `edit` is worth reading in the transcript at all: an
    /// unstyled patch is a wall of text whose `+` and `-` a reader has to scan for, and the
    /// same patch with line numbers and the theme's eleven diff colours is a review
    /// surface. It reuses [`crate::views::diff::DiffView`] — the one diff renderer this
    /// crate has, paired delete/insert runs and all — rather than growing a second one that
    /// would be free to disagree with the viewer the same patch opens in.
    fn tool_result_lines(
        &self,
        frame: RowFrame,
        name: &str,
        result: &str,
        status: ToolStatus,
        out: &mut Vec<Line<'static>>,
    ) {
        let RowFrame { role, rule, width } = frame;
        let body_width = width.saturating_sub(frame.gutter(Self::RESULT_INSET));
        let marker = role.marker();
        let budget = crate::views::tool::output_budget(name, self.tool_output);
        // Trimmed before the wrap, not after. A single-line minified bundle is one row to
        // the row cap and a megabyte to the wrap, so a cap applied afterwards would already
        // have paid for the work it exists to avoid.
        let (result, capped) = match result.char_indices().nth(budget.chars) {
            Some((cut, _)) => (&result[..cut], true),
            None => (result, false),
        };
        if looks_like_diff(result) {
            let mut view = crate::views::diff::DiffView::new(self.context.clone(), result);
            let rows = view.lines(body_width);
            let total = rows.len();
            for row in rows.into_iter().take(budget.rows) {
                let mut spans = vec![Span::styled(
                    format!("{marker} {}", Self::RESULT_INSET),
                    rule,
                )];
                spans.extend(row.spans);
                out.push(Line::from(spans));
            }
            self.push_overflow(frame, total, budget, capped, out);
            return;
        }
        // A failed call's output *is* the failure, so it is painted as one. In `muted` it
        // read as ordinary output two shades quieter than the red row above it, which is
        // backwards: §7.5 asks for the error to hang below the tool row, and hanging it
        // there in the colour of success-adjacent noise is only half of that.
        let body = if status == ToolStatus::Error {
            self.context.error()
        } else {
            self.context.muted()
        };
        let rows = wrap(result, body_width);
        let total = rows.len();
        for row in rows.into_iter().take(budget.rows) {
            out.push(self.ruled(
                role,
                rule,
                &format!("{}{row}", Self::RESULT_INSET),
                body,
                width,
            ));
        }
        self.push_overflow(frame, total, budget, capped, out);
    }

    /// The row that says content was withheld, and which key returns it.
    ///
    /// Never silent. Two things can hide output — the row cap and the character cap — and
    /// a reader who cannot tell "that is all of it" from "that is the first three lines of
    /// it" will trust a truncated result as complete. The two are reported together rather
    /// than one winning, because they answer different questions: how much was left, and
    /// whether what *is* shown was itself cut short.
    fn push_overflow(
        &self,
        frame: RowFrame,
        total: usize,
        budget: crate::views::tool::OutputBudget,
        capped: bool,
        out: &mut Vec<Line<'static>>,
    ) {
        // The budget is passed in rather than re-derived. Deriving it here needed the tool's
        // name, and a version of this that reached for it without one silently quoted the
        // 4,000-character cap on a `read`, whose cap is 6,000 — a notice stating a limit
        // that was not the one applied.
        let shown = budget.rows;
        if total <= shown && !capped {
            return;
        }
        // `…` rather than the collapsed-reasoning triangle this row used to borrow. The
        // triangle is a *header* affordance — it opens the block it labels, and a collapsed
        // reasoning header two rows above is using it for exactly that — so a triangle here
        // read as a second nested section rather than as the tail of the one above it. An
        // ellipsis is the mark this codebase already uses for "content was cut here"
        // (`views::truncate`, `ambient::elide_left`), and the key that lifts the cap is
        // named on the same row, so the glyph does not have to carry that meaning too.
        let mut notice = format!("{}{ELIDED}", Self::RESULT_INSET);
        if total > shown {
            notice.push_str(&format!(" {} more lines", total - shown));
        }
        if capped {
            if total > shown {
                notice.push(',');
            }
            let chars = u64::try_from(budget.chars).unwrap_or(u64::MAX);
            notice.push_str(&format!(" cut at {} chars", thousands(chars)));
        }
        // Resolved through the *keymap* and not `key_label`: `tool_details` ships as `none`
        // and is bound by this build's own `SHIPPED_DEFAULTS`, so `key_label` reported no
        // key for a key that works. See [`crate::views::pressable_label`] — the notice read
        // `… 9 more lines` with nothing after it for exactly that reason.
        if let Some(key) = crate::views::pressable_label("tool_details", &self.context) {
            notice.push_str(&format!(" · {key}"));
        }
        out.push(self.ruled(
            frame.role,
            frame.rule,
            &notice,
            self.context.accent(),
            frame.width,
        ));
    }
}

/// Rows the per-message cache may hold at once, across every entry.
///
/// A **row** budget rather than an entry count, because rows are what cost memory and
/// entries are not comparable to each other: a one-line user prompt and an expanded
/// reasoning block both occupy one entry, and `MessagePart::Reasoning`'s expanded body is
/// wrapped with no row cap at all, so a single entry can be arbitrarily tall. §6.2's
/// reference point is a 2,048-entry FIFO; an entry bound here would have permitted an
/// unbounded number of bytes, which is the failure class
/// `.omo/plans/memory-perf-optimization.md` exists to remove rather than relocate.
///
/// **What it costs at the bound.** A prepared 931-message frame was measured at
/// 5,156,568 bytes over 13,023 rows — 396 bytes per row including each `Line`, its
/// `Span`s and their text (`crates/zuno-tui/tests/render_cost.rs`). So a full cache holds
/// about 12.98 MB, which is 1.08% of M1's 1,198,872 KiB tuned-jemalloc W-real median in
/// `docs/perf-methodology.md`. The figure is a ceiling and not a typical cost: the same
/// measured session fills 13,023 of these rows, 39.7% of the budget, so an ordinary long
/// session is cached whole and never evicts.
const MAX_CACHED_ROWS: usize = 32_768;

/// Everything besides the theme that decides a message's rows.
///
/// The theme is held separately because it is compared by `Arc` identity rather than by
/// value; see [`TranscriptView::cached_lines`] for why that comparison is both complete
/// and free of an ABA hazard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowKey {
    width: u16,
    thinking: ThinkingDisplay,
    tool_output: ToolDisplay,
    previous: Option<Role>,
    content: u64,
}

/// One message's recalled rows, beside the inputs that produced them.
struct CachedRows {
    key: RowKey,
    theme: Arc<crate::theme::Resolved>,
    rows: Vec<Line<'static>>,
}

/// The per-message row cache: one slot per message, bounded by total rows.
///
/// Indexed by message position rather than keyed in a map, because the transcript only
/// ever appends: [`Transcript::push`] and the fold's `append` add at the end, and nothing
/// removes or reorders. So a position is a stable identity for as long as it exists, and
/// the content fingerprint in [`RowKey`] catches the case a position's message was
/// mutated in place. An index also makes a lookup a bounds check rather than a hash of
/// the whole key.
#[derive(Default)]
struct RowCache {
    slots: Vec<Option<CachedRows>>,
    rows: usize,
    /// Hits and misses, so a test can assert that a frame was *recalled* and not merely
    /// that it was correct. Without it a cache that never hit would pass every
    /// correctness test in the file, which is the way a cache silently stops working.
    #[cfg(test)]
    hits: usize,
    #[cfg(test)]
    misses: usize,
}

impl RowCache {
    /// The rows stored for `index`, when every input still matches.
    fn get(
        &mut self,
        index: usize,
        key: &RowKey,
        theme: &Arc<crate::theme::Resolved>,
    ) -> Option<&[Line<'static>]> {
        let recalled = self.slots.get(index).is_some_and(|slot| {
            slot.as_ref()
                .is_some_and(|entry| entry.key == *key && Arc::ptr_eq(&entry.theme, theme))
        });
        #[cfg(test)]
        if recalled {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        if !recalled {
            return None;
        }
        Some(self.slots[index].as_ref()?.rows.as_slice())
    }

    /// Store `rows` for `index`, evicting the oldest entries to stay inside the bound.
    fn put(
        &mut self,
        index: usize,
        key: RowKey,
        theme: Arc<crate::theme::Resolved>,
        rows: Vec<Line<'static>>,
    ) {
        // A message taller than the whole budget is never stored. Storing it would evict
        // every other entry to make room for one that cannot be reused often enough to
        // pay for that, and the eviction loop below could not reach the budget anyway.
        if rows.len() > MAX_CACHED_ROWS {
            self.forget(index);
            return;
        }
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        self.forget(index);
        self.rows += rows.len();
        self.slots[index] = Some(CachedRows { key, theme, rows });
        // Oldest first, which is the top of the transcript. The viewport follows the
        // newest row, so the rows evicted are the ones least likely to be drawn next.
        let mut oldest = 0;
        while self.rows > MAX_CACHED_ROWS && oldest < self.slots.len() {
            if oldest != index {
                self.forget(oldest);
            }
            oldest += 1;
        }
    }

    /// Drop `index`'s entry, if it has one.
    fn forget(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index)
            && let Some(entry) = slot.take()
        {
            self.rows -= entry.rows.len();
        }
    }

    /// Drop every slot at or past `len`.
    ///
    /// The fold's `RetryRollback` clears a message's parts but never shortens the
    /// message list, so this is reached only by a transcript that was replaced wholesale.
    /// Without it those slots would keep their rows alive against the bound forever.
    fn truncate_to(&mut self, len: usize) {
        for index in len..self.slots.len() {
            self.forget(index);
        }
        self.slots.truncate(len);
    }

    #[cfg(test)]
    const fn stored_rows(&self) -> usize {
        self.rows
    }

    #[cfg(test)]
    fn stored_entries(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    #[cfg(test)]
    const fn counts(&self) -> (usize, usize) {
        (self.hits, self.misses)
    }
}

/// Whether `message`'s rows may be recalled on a later frame.
///
/// The one disqualifier is a [`ToolStatus::Running`] call, whose glyph is the transcript's
/// spinner and therefore a function of how many events have been folded rather than of the
/// message. Everything else a part renders is decided by the part itself, the width and
/// the palette, all of which are in [`RowKey`] or compared beside it.
fn is_recallable(message: &Message) -> bool {
    !message.parts.iter().any(|part| {
        matches!(
            part,
            MessagePart::Tool {
                status: ToolStatus::Running,
                ..
            }
        )
    })
}

/// A fingerprint of everything about `message` that reaches a rendered row.
///
/// The match over [`MessagePart`] is exhaustive with no wildcard arm, so a new variant
/// cannot compile without deciding what identifies it. That is the property this needs
/// most: a variant that silently fell through to a shared arm would make two different
/// messages fingerprint alike, and the cache would then serve one message's rows for
/// another. Each arm also writes a distinct tag before its fields, so a `Text` and a
/// `Notice` carrying the same string do not collide.
fn fingerprint(message: &Message) -> u64 {
    let mut hasher = DefaultHasher::new();
    match message.role {
        Role::User => 0_u8,
        Role::Assistant => 1,
        Role::System => 2,
    }
    .hash(&mut hasher);
    // The id is hashed even though no row prints it: two messages equal in every rendered
    // field are interchangeable, but a fingerprint that ignored the id would be a claim
    // this function cannot check, and the cost is one `Option<String>`.
    message.id.hash(&mut hasher);
    message.parts.len().hash(&mut hasher);
    for part in &message.parts {
        match part {
            MessagePart::Text { text } => {
                0_u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            MessagePart::Reasoning {
                text,
                duration_secs,
                streaming,
            } => {
                1_u8.hash(&mut hasher);
                text.hash(&mut hasher);
                // `f64` is not `Hash` because `NaN != NaN`. The bits are hashed instead,
                // which is the right comparison here regardless: two entries differ if the
                // rendered `{secs:.1}s` could differ, and identical bits cannot.
                duration_secs.map(f64::to_bits).hash(&mut hasher);
                streaming.hash(&mut hasher);
            }
            MessagePart::Tool {
                call_id,
                name,
                arguments,
                title,
                status,
                output,
                diff,
            } => {
                2_u8.hash(&mut hasher);
                call_id.hash(&mut hasher);
                name.hash(&mut hasher);
                arguments.hash(&mut hasher);
                title.hash(&mut hasher);
                match status {
                    ToolStatus::Pending => 0_u8,
                    ToolStatus::Running => 1,
                    ToolStatus::Completed => 2,
                    ToolStatus::Error => 3,
                }
                .hash(&mut hasher);
                output.hash(&mut hasher);
                diff.hash(&mut hasher);
            }
            MessagePart::Attachment { filename, mime } => {
                3_u8.hash(&mut hasher);
                filename.hash(&mut hasher);
                mime.hash(&mut hasher);
            }
            MessagePart::Retry { attempt, max } => {
                4_u8.hash(&mut hasher);
                attempt.hash(&mut hasher);
                max.hash(&mut hasher);
            }
            MessagePart::Notice { text, level } => {
                5_u8.hash(&mut hasher);
                text.hash(&mut hasher);
                // The level reaches a rendered row — it picks the glyph and the colour — so
                // omitting it here would let two notices with the same words and different
                // grades serve each other's cached rows.
                level.hash(&mut hasher);
            }
            MessagePart::Diagnostics { report } => {
                6_u8.hash(&mut hasher);
                report.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// The three values that jointly decide where a transcript row starts and ends.
///
/// One value rather than three parameters because they are never chosen independently:
/// the role picks the rule glyph, the rule carries its colour, and the width is what both
/// are measured against. Passing them separately grew the tool-result path to eight
/// arguments, and clippy was right to object — the length was the symptom, and the
/// missing name for "the frame a row is laid into" was the cause. Naming it also removes
/// the chance of handing one function a `Role::User` marker beside a `Role::Assistant`
/// rule, which the three loose parameters allowed.
///
/// `Copy`, because it is three machine words and every row in a message needs it.
#[derive(Debug, Clone, Copy)]
struct RowFrame {
    /// Whose message this row belongs to; picks the left rule's glyph.
    role: Role,
    /// The left rule's style.
    rule: Style,
    /// The full row width in terminal columns, rule included.
    width: u16,
}

impl RowFrame {
    /// Columns spent before the body starts: the rule, its trailing space, and `inset`.
    ///
    /// Measured with [`display_width`] rather than `len`, because the rule glyph differs
    /// per role (`▌`, `│`, `▲`) and a future one could be two columns wide — at which point
    /// a byte count would silently shift every body row by a column.
    fn gutter(self, inset: &str) -> u16 {
        u16::try_from(display_width(self.role.marker()) + 1 + display_width(inset))
            .unwrap_or(u16::MAX)
    }
}

impl Component for TranscriptView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        let lines = self.cached_lines(area.width);
        self.content_height = lines.len();
        self.viewport_height = usize::from(area.height);
        let max = self.content_height.saturating_sub(self.viewport_height);
        // Pinned to the newest row unless the reader has scrolled away from it, which is
        // the only thing that ever raised the offset. Without this half the transcript
        // rested at row 0 for a session's whole life: `following` was armed in `new` and
        // read by nothing, and the clamp below is one-directional by construction — it
        // lowers an offset that ran past the end and can never advance one the growing
        // content just left behind. So every row past `area.height` was below the fold,
        // and a reply that overflowed the pane looked cut off at whatever row the pane
        // happened to end on rather than scrolled.
        //
        // Conditional, not an unconditional pin: a reader who scrolled back mid-turn must
        // not be yanked to the bottom by the next delta. `set_offset` disarms `following`
        // on the way up and re-arms it on landing at the last row, so returning to the
        // bottom resumes following without another key press.
        if self.following || self.offset > max {
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
    /// The most recent language-server verdict.
    ///
    /// Kept separately from `detail` because `detail` is overwritten by the next
    /// transport message, and "your edit does not compile" must not be displaced by
    /// "connected". It is the same reasoning that put the shadowing warning in the
    /// transcript.
    diagnostics: Option<String>,
    /// Whether a permission prompt is asking the user to decide.
    ///
    /// The strip is the one row always on screen, so it is where a user looks when a
    /// turn seems stuck. Saying `working` there while the process is parked on an ask
    /// points them at the wrong thing to wait for.
    awaiting_permission: bool,
    /// The checkout's branch, as the host measured it.
    ///
    /// Not folded from an engine event, because no engine event carries it: the branch
    /// describes the working tree rather than the turn, which is also why
    /// [`Self::reset`] leaves it alone — the same reasoning that keeps `diagnostics`
    /// across a turn boundary.
    git_branch: Option<String>,
}

/// Token counts for the session, accumulated across every step of every turn.
///
/// Cumulative rather than per-step because the number a user is watching for is what
/// the session has cost so far; a per-step count resets to a small number at every
/// step boundary and reads as if usage went down.
/// The four buckets are **disjoint**, which is what makes [`Self::total`] a sum. Getting
/// there takes work at the fold, because providers disagree about whether their prompt
/// figure already contains their cache figure: `zuno_llm::event::PromptAccounting` states
/// which, and [`Transcript::observe`] normalises with it before adding. Adding the raw
/// numbers is what made the sidebar's total count OpenAI's cache reads twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt tokens that were neither read from nor written to the cache.
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

    /// Every token the session has been billed for, cache included and counted once.
    ///
    /// A plain sum only because the four buckets are disjoint — see the type's own note.
    /// Saturating rather than wrapping: a long session's total is large, and a figure
    /// that wrapped to a small number would read as usage going down.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
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

    /// What the strip says while a permission prompt is waiting on the user.
    ///
    /// It replaces [`Self::WORKING`] rather than sitting beside it: the two are opposite
    /// answers to "who is this turn blocked on", and a strip that printed both would make
    /// the user read the prompt as background noise.
    pub const AWAITING_PERMISSION: &'static str = "awaiting approval";

    /// What the exit hint says the key *does*, appended to whichever key
    /// [`Self::exit_hint`] resolved.
    ///
    /// Split out from [`Self::EXIT_HINT`] so that only the key varies: the verb is the
    /// strip's own wording and has nothing to do with the binding table.
    const EXIT_ACTION: &'static str = "cancel/exit";

    /// The key shown as the way out, and what it does while a turn is running.
    ///
    /// The strip is the one row always on screen, so it is where a binding a user
    /// cannot otherwise guess belongs. An application whose exit key is undiscoverable
    /// is only marginally better than one that has none.
    ///
    /// This is the **fallback**, not the rendered text: [`Self::exit_hint`] looks the
    /// key up. It is reached only when the user explicitly disabled `app_exit`, and it
    /// still names a key that works — see that method for why that is not a lie. A test
    /// pins it to the shipped table's own first spelling so it cannot drift from the
    /// derived form.
    pub const EXIT_HINT: &'static str = "ctrl+c cancel/exit";

    /// What marks the branch segment, borrowed from the ambient sidebar so the two
    /// surfaces name the same field the same way.
    ///
    /// Aliased to [`crate::views::ambient::BRANCH_GLYPH`] rather than spelled again here:
    /// "the two surfaces agree" was a claim held up by a copied literal, and a copied
    /// literal is a claim only until somebody edits one of the two.
    ///
    /// Written tight against the name — `⑂main`, not `⑂ main` — because the strip is one
    /// shared row and every segment on it is already compacted for the same reason
    /// [`TokenUsage::compact`] writes `↑3,000` rather than spelling the count out.
    pub const BRANCH_GLYPH: &'static str = crate::views::ambient::BRANCH_GLYPH;

    /// What separates two segments of the right-hand group.
    const TRAILER_GAP: &'static str = "  ";

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
            diagnostics: None,
            awaiting_permission: false,
            git_branch: None,
        }
    }

    /// Record the checkout's branch, as `zuno-cli/src/cmd/tui.rs` measured it.
    ///
    /// An empty string is treated as "not on a branch" rather than adopted, matching
    /// [`Self::describe`]: a blank segment on the strip is indistinguishable from a
    /// field that failed to resolve, and it would still cost the separator's columns.
    pub fn set_git_branch(&mut self, branch: impl Into<String>) {
        let branch = branch.into();
        self.git_branch = (!branch.is_empty()).then_some(branch);
    }

    /// Record whether a permission prompt is waiting on the user.
    ///
    /// Returns whether the answer changed, which is what a caller turns into a redraw.
    pub const fn set_awaiting_permission(&mut self, awaiting: bool) -> bool {
        let changed = self.awaiting_permission != awaiting;
        self.awaiting_permission = awaiting;
        changed
    }

    /// Record the latest language-server verdict.
    pub fn set_diagnostics(&mut self, summary: impl Into<String>) {
        self.diagnostics = Some(summary.into());
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
        // `diagnostics` deliberately survives: it describes the working tree, not the
        // turn, so clearing it at a turn boundary would hide a verdict that is still
        // true. The next report replaces it.
    }

    /// The exit hint, naming the key the *user's* keymap resolved for `app_exit`.
    ///
    /// Derived rather than written down because a hardcoded spelling goes stale the
    /// moment overrides become real: with `{"keybinds": {"app_exit": "ctrl+q"}}` the
    /// welcome grid and the command palette both said `ctrl+q` while this row still
    /// said `ctrl+c`, so one frame advertised two different ways out.
    ///
    /// [`key_label`] and not [`crate::keybind::Keymap::sequences`], for two reasons.
    /// The welcome grid is already built on [`key_label`]
    /// (`views/welcome.rs:351`), and agreeing with the surface this row contradicted
    /// is the entire fix — a second lookup could resolve the same override to a
    /// different spelling and reintroduce the disagreement one layer down. And
    /// `sequences` returns *every* binding: `app_exit` ships three
    /// (`ctrl+c,ctrl+d,<leader>q`), and comma-joining them would spend about
    /// twenty-four columns of a one-row strip advertising alternatives, evicting the
    /// token counts at widths where they fit today. One key is what a hint is for, and
    /// [`key_label`] takes the first because the table lists its preferred spelling
    /// first and a user's override lists theirs.
    ///
    /// When the user *disabled* `app_exit` there is no resolved spelling, and the row
    /// falls back to [`Self::EXIT_HINT`] rather than going quiet. That is still true,
    /// not a guess: [`crate::keybind::is_exit_chord`] reads the static table on
    /// purpose, so `ctrl+c` leaves the application even with no binding pointing at it
    /// (`keybind.rs:191`). Dropping the hint would make the one row that survives every
    /// width degradation silent about the only guaranteed way out.
    fn exit_hint(&self) -> String {
        key_label(APP_EXIT, &self.context).map_or_else(
            || Self::EXIT_HINT.to_owned(),
            |key| format!("{key} {}", Self::EXIT_ACTION),
        )
    }

    /// Every right-hand group the row will try, richest first.
    ///
    /// The fields are laid out in *ascending* priority left to right — branch, token
    /// counts, exit key — and each rung is built by dropping the leftmost, which is to
    /// say the lowest-ranked, field still present. Ordering them that way is what keeps
    /// the right edge from reflowing as the terminal narrows: a dropped field vanishes
    /// and nothing beside it moves.
    ///
    /// The ranking, lowest first, and why:
    ///
    /// * **branch** — the ambient sidebar already prints it (`views/ambient.rs:470`), so
    ///   it is the one field a narrow row loses nothing unique by dropping.
    /// * **token counts** — informational, and the sidebar carries the same accumulator.
    /// * **exit key** — the only way out, so it is last to go. It already outranked the
    ///   token counts before the branch existed; the branch joins below it rather than
    ///   displacing that order. Its spelling comes from [`Self::exit_hint`]; the rank
    ///   does not depend on how wide that spelling turns out to be.
    ///
    /// With no branch set this reduces to two rungs — counts plus the key, then the key
    /// alone — so the pre-existing degradation is unchanged.
    ///
    /// # Why there is no cost rung
    ///
    /// There was one, and it could never be populated. It had a `set_cost` setter whose
    /// comment named `zuno-cli/src/cmd/tui.rs` as the caller, and no such call existed:
    /// only tests ever set it, so the segment was empty in every real session while the
    /// comment asserted a contract nobody honoured.
    ///
    /// It was removed rather than wired because no authoritative figure is reachable to
    /// wire it *to*. `zuno_engine::stream::ProjectionContext::with_cost` is called only
    /// from `zuno-engine/tests/stream.rs`, so every persisted `"cost"` is `0.0` and the
    /// session's column always sums to zero — a wired segment would read `$0.00` forever,
    /// which a user reads as *free* rather than as *unknown*. Computing one here is worse:
    /// prices are per-million-token and `catalog::merge::cost_from_catalog` drops the
    /// `context_over_200k` band at resolve time, so a multiplication in this layer would
    /// be confidently wrong on exactly the long-context models where the number matters.
    /// A wrong price is worse than no price, and an empty segment claiming to be a price
    /// is worse than neither.
    fn trailers(&self) -> Vec<String> {
        let mut ranked = Vec::with_capacity(3);
        if let Some(branch) = &self.git_branch {
            ranked.push(format!("{}{branch}", Self::BRANCH_GLYPH));
        }
        if !self.usage.is_empty() {
            ranked.push(self.usage.compact());
        }
        ranked.push(self.exit_hint());
        (0..ranked.len())
            .map(|dropped| ranked[dropped..].join(Self::TRAILER_GAP))
            .collect()
    }

    /// The rendered row, with [`Self::exit_hint`] right-aligned when it fits.
    ///
    /// The hint is dropped rather than truncated on a narrow terminal: half a key
    /// name is worse than none, and the turn state it shares the row with is what a
    /// user needs more. The state itself is never dropped, only padded or truncated by
    /// [`padded`], which is why a field that must yield to the exit key belongs in
    /// [`Self::trailers`] and not in [`Self::state`].
    #[must_use]
    pub fn line(&self, width: u16) -> Line<'static> {
        let state = format!(" {}", self.state());
        let columns = usize::from(width);
        // Terminal columns, not `chars().count()`: the cache glyph `⚡` and a CJK model
        // name each occupy two cells, so counting characters here claims the row is
        // narrower than it is and over-fills it by one column per wide glyph.
        let state_columns = display_width(&state);
        for trailer in self.trailers() {
            let used = state_columns + display_width(&trailer) + 1;
            if used < columns {
                return Line::from(vec![
                    Span::styled(state, self.context.element()),
                    Span::styled(" ".repeat(columns - used), self.context.element()),
                    Span::styled(
                        trailer,
                        Style::new()
                            .fg(self.context.palette().text_muted.into())
                            .bg(self.context.palette().background_element.into()),
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
        if let Some(diagnostics) = &self.diagnostics {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(diagnostics);
        }
        if self.awaiting_permission {
            // Appended even beside a resolved agent and model, because those describe the
            // turn's configuration while this describes what it is stopped on. It also
            // displaces `working`/`idle` below: an outstanding ask outranks both.
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(Self::AWAITING_PERMISSION);
        } else if text.is_empty() {
            text.push_str(if self.running {
                Self::WORKING
            } else {
                Self::IDLE
            });
        } else if !self.running && self.agent.is_none() && self.model.is_none() && self.step == 0 {
            // Only when every field came from configuration. Saying `idle` beside a
            // resolved agent, model and step would contradict the state it sits next
            // to, which is the same defect as a strip that stays `idle` mid-turn.
            text.push_str(" · ");
            text.push_str(Self::IDLE);
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
            // Both the live field and the configured one. The live field is what the strip
            // shows during a turn and is cleared when the turn ends; the configured one
            // survives that reset, so a mid-session switch is still reported once the
            // turn it applies to has finished.
            TurnEvent::AgentResolved { agent, .. } => {
                self.agent = Some(agent.clone());
                self.configured_agent = Some(agent.clone());
                EventResult::REDRAW
            }
            TurnEvent::ModelResolved {
                provider_id,
                model_id,
                ..
            } => {
                let label = format!("{provider_id}/{model_id}");
                self.model = Some(label.clone());
                self.configured_model = Some(label);
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
                        accounting,
                    },
                ..
            } => {
                let input = input_tokens.unwrap_or(0);
                let cache_read = cache_read_input_tokens.unwrap_or(0);
                let cache_write = cache_write_input_tokens.unwrap_or(0);
                // Normalised the same way [`Transcript::observe`] does, and it has to be:
                // the strip and the sidebar show the same session, so two accumulators
                // folding the same event differently would put two totals on one screen.
                self.usage.add(
                    accounting.uncached_input(input, cache_read, cache_write),
                    output_tokens.unwrap_or(0),
                    cache_read,
                    cache_write,
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
            .fg(self.context.palette().border_subtle.into())
            .bg(self.context.palette().background.into());
        let thumb = Style::new()
            .fg(self.context.palette().border_active.into())
            .bg(self.context.palette().background.into());
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

/// Break `text` into rows no wider than `width` **columns**, on word boundaries where
/// possible.
///
/// ratatui can wrap for us, but not while also letting the transcript *count* the
/// rows it will occupy — which the scroll offset and the scrollbar both need. So
/// the wrap happens here and the produced lines are handed over already broken.
///
/// # Columns, not characters
///
/// Every measurement here is [`display_width`]. Counting characters instead was
/// measured producing rows twice as wide as the frame for Chinese prose: at 40 columns
/// the prompt `帮我把 diff viewer 接上文件树，顺便看一下 wrap 的宽字符行为` wrapped after
/// 38 *characters*, ratatui clipped the row at 38 *columns*, and the nineteen columns of
/// text past the cut — `一下 wrap` and everything after it — were silently discarded.
/// The row count the scroller trusts was wrong by the same factor. Wrapping a CJK
/// transcript is the single case this helper exists for that character counting cannot
/// get right, and the wider a word is the more of it disappears.
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
            // paths and URLs are common here and both are unbreakable on spaces. CJK
            // prose reaches here too, because it carries no spaces at all: the whole
            // paragraph arrives as one "word".
            while display_width(word) > width {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                let mut head = truncate(word, width);
                if head.is_empty() {
                    // One column cannot hold a two-column glyph, so `truncate` returns
                    // nothing and consuming zero bytes would spin here forever. Emit the
                    // glyph on a row of its own: one column of overflow is a rendering
                    // artefact a terminal absorbs, an infinite loop is a hung TUI.
                    head.extend(word.chars().next());
                }
                let consumed = head.len();
                rows.push(head);
                word = &word[consumed..];
            }
            if row.is_empty() {
                row.push_str(word);
            } else if display_width(&row) + 1 + display_width(word) <= width {
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
