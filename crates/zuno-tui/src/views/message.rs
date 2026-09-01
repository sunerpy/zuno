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
use crate::views::selection::{TextPoint, TextSelection, slice_columns};
use crate::views::{ViewContext, display_width, fill, padded, truncate};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, symbols};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use zuno_engine::r#loop::{INTERRUPTED_TURN_NOTICE, TurnEvent};
use zuno_engine::session_command::SessionCommand;
use zuno_llm::event::StreamEvent;
pub use zuno_types::TokenUsage;
use zuno_types::UsageSnapshot;

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
    /// Refused before the requested effect ran.
    Blocked,
    /// Stopped because the turn received an explicit hard interruption.
    Cancelled,
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
            Self::Blocked => "!",
            Self::Cancelled => "×",
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
/// The first eight rows are verbatim from the oracle's `InlineTool` call sites: shell
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
        "shell" | "exec" | "exec_command" => ("$", "Writing command..."),
        "glob" => ("✱", "Finding files..."),
        "grep" => ("✱", "Searching content..."),
        "read" => ("→", "Reading file..."),
        "write" | "edit" => ("→", "Preparing write..."),
        "webfetch" | "web_fetch" => ("%", "Fetching from the web..."),
        "web_search" | "google_search" => ("◈", "Searching web..."),
        "task" => ("#", "Delegating..."),
        "job" => ("◷", "Checking job..."),
        "bg" => ("◉", "Inspecting background execution..."),
        // A patch is a write, so it shares the write arrow rather than inventing a glyph:
        // the two differ in how the change is expressed, not in what happens to the file.
        "apply_patch" => ("→", "Preparing patch..."),
        "plan_get" => ("≣", "Reading plan..."),
        "plan_update" => ("≣", "Updating plan..."),
        "todo_get" => ("☑", "Reading work items..."),
        "todo_update" => ("☑", "Updating work items..."),
        "history" => ("↶", "Reading session history..."),
        "notes" => ("≡", "Accessing working notes..."),
        // Tools that are about to block on the user share the one glyph that reads as a
        // question; their labels still distinguish general and goal-specific elicitation.
        "question" => ("?", "Asking..."),
        "goal_request_input" => ("?", "Clarifying the goal..."),
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
        "memory_propose" => ("≡", "Proposing memory..."),
        // One glyph for the non-interactive goal tools: they read, set and amend one
        // object, and separate glyphs would imply separate subjects.
        "goal_get" | "goal_propose" | "goal_update" => ("◎", "Reading the goal..."),
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

    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::Collapsed,
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

/// Whether routine activity is compacted into one row per contiguous group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityDisplay {
    #[default]
    Summary,
    Detailed,
}

/// A successfully loaded skill as identified by the model's tool arguments.
///
/// Unique names may omit `source`; ambiguous names can only complete after the
/// tool receives the exact source locator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoadedSkillIdentity {
    pub name: String,
    pub source: Option<String>,
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

    /// The other display state.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::Collapsed,
        }
    }

    /// The disclosure glyph drawn on a tool header.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Collapsed => "▸",
            Self::Expanded => "▾",
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

/// The glyphs that close the user's message box on the right and along its rules.
///
/// `▐` matches [`crate::views::session::COMPOSER_RIGHT_RULE`] rather than the `│` a border
/// set would give, because `│` is [`Role::Assistant`]'s own marker: a right edge drawn with
/// it would put the assistant's glyph on the user's rows, which is the one distinction this
/// box exists to sharpen.
pub(crate) const USER_BOX_RIGHT: &str = "▐";
pub(crate) const USER_BOX_RULE: &str = "─";

/// The fewest body columns the user's box keeps before the frame is dropped instead.
///
/// The same judgement [`PROMPT_MIN_CONTENT_COLS`](crate::views::session) records for the
/// composer's chrome: below this the frame costs more columns than the words can spare, and
/// a prompt wrapped every eight characters is less readable than an unframed one.
const USER_BOX_MIN_INNER_COLS: u16 = 12;

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
        ToastLevel::Success => context.text(),
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

/// Compact pulse frames for the live control row beneath the composer.
///
/// Three cells make motion visible without making the footer jump horizontally. The
/// transcript owns the clock because running tool glyphs already advance from it; a
/// second timer in the footer would eventually drift and redraw the same frame twice.
pub const WORK_PULSE: [&str; 6] = ["▰▱▱", "▰▰▱", "▱▰▰", "▱▱▰", "▱▰▰", "▰▰▱"];

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
        /// Stable client-facing identity resolved by the runtime.
        display_name: String,
        /// Durable client presentation intent supplied by the tool implementation.
        ui_intent: zuno_tool::ToolUiIntent,
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
    /// Durable message-level data retained for client projections.
    ///
    /// This part deliberately renders no transcript rows. It carries typed host metadata
    /// such as `message.data.taskReport` to views that need more than the visible text,
    /// without serialising that metadata into prose or teaching those views to guess from
    /// English report strings.
    ReplayData {
        /// The stored message `data` object.
        data: serde_json::Map<String, serde_json::Value>,
    },
}

fn compact_activity(part: &MessagePart) -> Option<zuno_types::ActivityKind> {
    use zuno_types::ActivityKind;
    match part {
        MessagePart::Tool {
            ui_intent: zuno_tool::ToolUiIntent::Subagent,
            status: ToolStatus::Completed,
            diff: None,
            ..
        } => Some(ActivityKind::Delegation),
        MessagePart::Tool {
            name,
            status: ToolStatus::Completed,
            diff: None,
            ..
        } => match name.as_str() {
            "shell" | "exec" | "exec_command" => Some(ActivityKind::Command),
            "read" | "glob" | "grep" | "list" | "ls" => Some(ActivityKind::Read),
            "web_search" | "google_search" | "webfetch" | "web_fetch" => Some(ActivityKind::Search),
            "view_image" | "image" | "imagegen" => Some(ActivityKind::Image),
            _ => None,
        },
        MessagePart::Attachment { .. } => Some(ActivityKind::Image),
        _ => None,
    }
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

    /// Retain one durable message `data` object for typed replay projections.
    ///
    /// Non-object values cannot be message data and are ignored. Storage adapters keep
    /// visible parts separate and call this once with the message-level object.
    pub fn attach_replay_data(&mut self, data: serde_json::Value) {
        let serde_json::Value::Object(data) = data else {
            return;
        };
        self.parts.push(MessagePart::ReplayData { data });
    }
}

/// Why a running turn is currently waiting on the user.
///
/// This is UI state rather than an engine event: a tool blocked in `ctx.ask` emits no
/// progress while the dialog is open, so only the mounted client surface can distinguish
/// an approval from an ordinary slow tool or a structured question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitingUser {
    /// A side effect needs permission.
    Approval,
    /// A structured question needs an answer.
    Answer,
}

impl AwaitingUser {
    pub(crate) const fn status_text(self) -> &'static str {
        match self {
            Self::Approval => "awaiting approval",
            Self::Answer => "awaiting answer",
        }
    }
}

/// The transcript: every message, folded from engine events.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    messages: Vec<Message>,
    /// The index of the assistant message currently receiving deltas.
    streaming: Option<usize>,
    /// Whether the turn is still running, for the fixed live footer.
    running: bool,
    /// Whether this live turn already emitted its session-owned interruption marker.
    ///
    /// Terminal delivery can be repeated across a client boundary. The marker describes
    /// the turn, not the number of times the boundary delivered its terminal event.
    interruption_noted: bool,
    /// Session-cumulative provider token accounting.
    ///
    /// The same [`TokenUsage`] the sidebar carries, folded from the same event.
    /// One type rather than two: a sidebar with its own accumulator is a second
    /// running total free to disagree with the transcript's, and two token figures on one
    /// screen that differ is worse than either alone.
    tokens: TokenUsage,
    /// Whether cumulative usage is known, unavailable, or not reported yet.
    usage_state: UsageState,
    /// Durable count of turns that ended in failure rather than completion or cancellation.
    failed_turns: u64,
    /// The whole prompt the most recent request sent.
    ///
    /// Separate from [`Self::tokens`], and replaced rather than accumulated, because the
    /// two answer different questions: `tokens` is what the session has been billed for
    /// so far, and this is what is currently in the window. Deriving the second from the
    /// first is the defect [`Transcript::context_used`] documents — a cumulative figure
    /// passes any window on the second turn.
    last_prompt_tokens: Option<u64>,
    /// Local estimate for the newest provider request until confirmed usage arrives.
    estimated_pending_prompt_tokens: Option<u64>,
    /// The model's context ceiling, when the catalog states one.
    context_limit: u64,
    /// How many animation-clock frames have elapsed.
    ///
    /// Provider and tool events deliberately do not advance this. A slow request can
    /// produce no event for seconds, which is exactly when an event-driven spinner
    /// freezes and falsely looks hung.
    ticks: usize,
    /// Why a mounted prompt is asking the user to decide right now.
    ///
    /// Not folded from an engine event, because a parked ask produces none: the
    /// dispatcher blocks inside `ctx.ask` and the engine's last word was
    /// `ToolDispatchStarted`, which is equally true of a shell command that is simply
    /// slow. The dialog stack is the only thing that knows the difference, so it is what
    /// sets this.
    awaiting_user: Option<AwaitingUser>,
    /// Skills loaded by the host before a provider request, including restored prompt
    /// blocks from an earlier process.
    loaded_skills: BTreeSet<LoadedSkillIdentity>,
    /// How many leading messages were read back from the database rather than lived through.
    ///
    /// A count rather than a flag per message, because [`Self::replay`] refuses a
    /// non-empty transcript: everything it installs is therefore a prefix and everything
    /// appended afterwards is live *by construction*, rather than by every push site
    /// remembering to set a flag.
    ///
    /// It exists because `SnapshotHistory` is rebuilt empty on every launch
    /// (`zuno-cli/src/cmd/tui.rs`), so the worktree checkpoint a replayed turn opened
    /// belongs to a process that has exited — see
    /// [`SessionScreen::message_actions`](crate::views::session::SessionScreen).
    replayed: usize,
}

impl Transcript {
    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore selected Skill identities from durable prompt receipts before the first
    /// frame. Tool-loaded identities are still derived from replayed tool calls.
    pub fn restore_loaded_skills(&mut self, skills: impl IntoIterator<Item = LoadedSkillIdentity>) {
        self.loaded_skills.extend(skills);
    }

    /// Record why a mounted prompt is asking the user to decide.
    ///
    /// Returns whether the answer changed, which is what a caller turns into a redraw.
    pub fn set_awaiting_user(&mut self, awaiting: Option<AwaitingUser>) -> bool {
        let changed = self.awaiting_user != awaiting;
        self.awaiting_user = awaiting;
        changed
    }

    /// Why a mounted prompt is currently asking the user to decide.
    #[must_use]
    pub const fn awaiting_user(&self) -> Option<AwaitingUser> {
        self.awaiting_user
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

    /// Mark an accepted submission live before the engine publishes `TurnStarted`.
    ///
    /// The footer appears immediately after Enter, so its animation clock must become
    /// active during the persistence gap as well as after the first engine event.
    pub const fn mark_running(&mut self) {
        self.running = true;
        self.interruption_noted = false;
    }

    /// The session's cumulative token accounting.
    #[must_use]
    pub const fn tokens(&self) -> TokenUsage {
        self.tokens
    }

    /// Whether the cumulative token projection is usable.
    #[must_use]
    pub const fn usage_state(&self) -> UsageState {
        self.usage_state
    }

    /// Restore durable usage before replaying an existing session.
    pub fn restore_usage(&mut self, snapshot: UsageSnapshot) {
        self.tokens = snapshot.confirmed;
        self.last_prompt_tokens = snapshot.last_prompt_tokens;
        self.estimated_pending_prompt_tokens = snapshot.estimated_pending_prompt_tokens;
        self.failed_turns = snapshot.failed_turns;
        if let Some(context_limit) = snapshot.context_limit {
            self.context_limit = context_limit;
        }
        self.usage_state = if snapshot.confirmed_known {
            UsageState::Known
        } else if snapshot.last_confirmed_at.is_some() || !snapshot.confirmed.is_empty() {
            UsageState::Unavailable
        } else {
            UsageState::NotReported
        };
    }

    /// Turns that ended in an error. User cancellations are tracked separately.
    #[must_use]
    pub const fn failed_turns(&self) -> u64 {
        self.failed_turns
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
    pub const fn last_prompt_tokens(&self) -> Option<u64> {
        self.last_prompt_tokens
    }

    /// The most recent prompt and its model-declared context ceiling.
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
    pub const fn context_window(&self) -> Option<ContextWindowUsage> {
        if self.context_limit == 0 {
            return None;
        }
        let (prompt_tokens, estimated) = match self.estimated_pending_prompt_tokens {
            Some(prompt_tokens) => (prompt_tokens, true),
            None => match self.last_prompt_tokens {
                Some(prompt_tokens) => (prompt_tokens, false),
                None => return None,
            },
        };
        Some(ContextWindowUsage {
            prompt_tokens,
            limit: self.context_limit,
            estimated,
        })
    }

    /// The spinner frame this transcript is on.
    #[must_use]
    pub const fn spinner(&self) -> &'static str {
        SPINNER[self.ticks % SPINNER.len()]
    }

    /// The pulse frame shown to the left of the running turn's interrupt hint.
    #[must_use]
    pub const fn work_pulse(&self) -> &'static str {
        WORK_PULSE[self.ticks % WORK_PULSE.len()]
    }

    /// Advance the liveness animation if it is currently visible.
    ///
    /// A turn parked on a permission or question dialog is waiting for the user, not
    /// working. Suppressing the frame there avoids both a contradictory spinner and a
    /// pointless whole-screen repaint.
    pub fn advance_animation(&mut self) -> bool {
        if !self.running || self.awaiting_user.is_some() {
            return false;
        }
        self.ticks = self.ticks.wrapping_add(1);
        true
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

    /// Install a session's persisted history, read back from the database on resume.
    ///
    /// This is what closes the gap between what the model is given and what the user can
    /// see. The next request rehydrates the whole session from the database
    /// (`zuno-engine/src/loop.rs`), so a resumed session used to reach the model with a
    /// conversation the screen had never heard of — the user read a welcome screen while
    /// the reply quoted turns that were nowhere on it.
    ///
    /// # Only on an empty transcript, and that is enforced
    ///
    /// A no-op on a transcript that already holds messages, because the only correct
    /// moment is before the first frame: it must precede the startup notices so a fresh
    /// session's welcome screen still works ([`Self::conversation_started`] excludes
    /// [`Role::System`] precisely so those notices do not claim a conversation began),
    /// and it must precede any prompt the command line supplied. Refusing here rather
    /// than trusting the caller is what makes [`Self::replayed`] a prefix rather than a
    /// claim.
    pub fn replay(&mut self, messages: Vec<Message>) {
        if !self.messages.is_empty() {
            return;
        }
        self.replayed = messages.len();
        self.messages = messages;
    }

    /// How many leading messages came from the database rather than from this session.
    ///
    /// Every index below this was replayed; every index at or above it was lived through.
    #[must_use]
    pub const fn replayed(&self) -> usize {
        self.replayed
    }

    /// Fold one engine event into the transcript.
    ///
    /// Returns whether anything visible changed, which is what the component turns
    /// into a redraw request.
    pub fn observe(&mut self, event: &TurnEvent) -> bool {
        match event {
            TurnEvent::SkillLoaded { name, source } => {
                self.loaded_skills.insert(LoadedSkillIdentity {
                    name: name.clone(),
                    source: Some(source.clone()),
                })
            }
            TurnEvent::TurnStarted { .. } => {
                self.mark_running();
                true
            }
            TurnEvent::SessionCommandStarted { .. } => {
                self.mark_running();
                true
            }
            TurnEvent::SessionCommandOutput { content, .. } => {
                self.messages.push(Message::noticed(
                    crate::views::toast::ToastLevel::Info,
                    content.clone(),
                ));
                true
            }
            TurnEvent::SessionCommandCompleted { command } => {
                self.running = false;
                self.streaming = None;
                self.close_reasoning();
                if let SessionCommand::Compact = command {
                    self.messages.push(Message::noticed(
                        crate::views::toast::ToastLevel::Success,
                        "context compacted; older history was summarized",
                    ));
                }
                true
            }
            TurnEvent::SessionCommandFailed { .. } => {
                // The owning surface reports the returned command error as the
                // terminal turn failure. Stop the command activity here without
                // rendering the same error twice.
                self.running = false;
                self.streaming = None;
                self.close_reasoning();
                true
            }
            TurnEvent::ProviderRequestStarted {
                estimated_prompt_tokens,
                ..
            } => {
                self.estimated_pending_prompt_tokens = Some(*estimated_prompt_tokens);
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
            TurnEvent::ToolCallStarted {
                call_id,
                display_name,
                name,
                ui_intent,
                ..
            } => {
                self.update_tool(call_id, |part| {
                    if let MessagePart::Tool {
                        display_name: rendered_name,
                        ui_intent: intent,
                        ..
                    } = part
                    {
                        *rendered_name = display_name.clone();
                        *intent = *ui_intent;
                    }
                }) || {
                    self.append(MessagePart::Tool {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        display_name: display_name.clone(),
                        ui_intent: *ui_intent,
                        arguments: String::new(),
                        title: None,
                        status: ToolStatus::Pending,
                        output: None,
                        diff: None,
                    });
                    true
                }
            }
            TurnEvent::ToolDispatchStarted {
                call_id,
                display_name,
                name,
                ui_intent,
                ..
            } => {
                self.update_tool(call_id, |part| {
                    if let MessagePart::Tool {
                        status,
                        display_name: rendered_name,
                        ui_intent: intent,
                        ..
                    } = part
                    {
                        *status = ToolStatus::Running;
                        *rendered_name = display_name.clone();
                        *intent = *ui_intent;
                    }
                }) || {
                    // A dispatch with no `ToolUseStart` seen — the provider stream
                    // was not observed, e.g. after a reconnect. Materialise the
                    // call rather than dropping it from the transcript.
                    self.append(MessagePart::Tool {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        display_name: display_name.clone(),
                        ui_intent: *ui_intent,
                        arguments: String::new(),
                        title: None,
                        status: ToolStatus::Running,
                        output: None,
                        diff: None,
                    });
                    true
                }
            }
            TurnEvent::ToolDispatchBlocked { call_id, .. } => self.update_tool(call_id, |part| {
                if let MessagePart::Tool { status, .. } = part {
                    *status = ToolStatus::Blocked;
                }
            }),
            TurnEvent::ToolDispatchInterrupted {
                call_id,
                display_name,
                title,
                output,
                ..
            } => self.update_tool(call_id, |part| {
                if let MessagePart::Tool {
                    status,
                    display_name: rendered_name,
                    title: slot,
                    output: body,
                    ..
                } = part
                {
                    *status = ToolStatus::Cancelled;
                    *rendered_name = display_name.clone();
                    *slot = Some(title.clone());
                    *body = Some(output.clone());
                }
            }),
            TurnEvent::ToolDispatchCompleted {
                call_id,
                display_name,
                title,
                output,
                diff,
                is_error,
                ..
            } => self.update_tool(call_id, |part| {
                if let MessagePart::Tool {
                    status,
                    display_name: rendered_name,
                    title: slot,
                    output: body,
                    diff: patch,
                    ..
                } = part
                {
                    *status = if *is_error && *status != ToolStatus::Blocked {
                        ToolStatus::Error
                    } else if *is_error {
                        ToolStatus::Blocked
                    } else {
                        ToolStatus::Completed
                    };
                    *rendered_name = display_name.clone();
                    *slot = Some(title.clone());
                    *body = Some(output.clone());
                    *patch = diff
                        .as_ref()
                        .and_then(|diff| diff.unified())
                        .map(str::to_owned);
                }
            }),
            TurnEvent::TurnCompleted { .. } => {
                self.running = false;
                self.streaming = None;
                self.close_reasoning();
                true
            }
            TurnEvent::TurnWaitingForHuman { request_id, .. } => {
                self.running = false;
                self.streaming = None;
                self.close_reasoning();
                self.messages.push(Message::noticed(
                    crate::views::toast::ToastLevel::Info,
                    format!("waiting for human input: {request_id}"),
                ));
                true
            }
            TurnEvent::TurnInterrupted { .. } => {
                self.running = false;
                self.streaming = None;
                self.close_reasoning();
                if !self.interruption_noted {
                    self.messages.push(Message::noticed(
                        crate::views::toast::ToastLevel::Info,
                        INTERRUPTED_TURN_NOTICE,
                    ));
                    self.interruption_noted = true;
                }
                true
            }
            TurnEvent::TurnFailed { message, .. } => {
                self.running = false;
                self.failed_turns = self.failed_turns.saturating_add(1);
                self.streaming = None;
                self.close_reasoning();
                for transcript_message in &mut self.messages {
                    for part in &mut transcript_message.parts {
                        if let MessagePart::Tool { status, .. } = part
                            && matches!(status, ToolStatus::Pending | ToolStatus::Running)
                        {
                            *status = ToolStatus::Error;
                        }
                    }
                }
                self.messages.push(Message::noticed(
                    crate::views::toast::ToastLevel::Error,
                    format!("this turn ended early: {message}"),
                ));
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
                if self.update_tool(id, |_| {}) {
                    return false;
                }
                self.append(MessagePart::Tool {
                    call_id: id.clone(),
                    name: name.clone(),
                    display_name: name.clone(),
                    ui_intent: zuno_tool::ToolUiIntent::Generic,
                    arguments: String::new(),
                    title: None,
                    status: ToolStatus::Pending,
                    output: None,
                    diff: None,
                });
                true
            }
            // Providers may interleave arguments for several calls. The event's id keeps
            // each fragment attached to the row opened by its own ToolUseStart.
            StreamEvent::ToolInputDelta { id, delta } => self.update_tool(id, |part| {
                if let MessagePart::Tool { arguments, .. } = part {
                    arguments.push_str(delta);
                }
            }),
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
                self.usage_state = UsageState::Known;
                self.estimated_pending_prompt_tokens = None;
                let input = input_tokens.unwrap_or_default();
                let cache_read = cache_read_input_tokens.unwrap_or_default();
                let cache_write = cache_write_input_tokens.unwrap_or_default();
                // Replaced, not added: this is the window's current occupancy.
                self.last_prompt_tokens =
                    Some(accounting.prompt_total(input, cache_read, cache_write));
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
            // Warnings go in the transcript rather than a transient footer: a footer
            // holds one detail and the next one overwrites it, so a suppressed tool
            // would appear for a moment and then be gone.
            StreamEvent::StatusDetail { detail } if detail.starts_with("warning: ") => {
                self.messages.push(Message::notice(detail.clone()));
                self.streaming = None;
                true
            }
            StreamEvent::Error { message, .. } => {
                self.messages.push(Message::noticed(
                    crate::views::toast::ToastLevel::Error,
                    message.clone(),
                ));
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

/// One message's rows and the tool-header rows inside it.
#[derive(Clone)]
struct MessageRows {
    lines: Vec<Line<'static>>,
    tools: Vec<(usize, String)>,
    reasoning: Vec<(usize, ReasoningKey)>,
    copy: Vec<Option<CopyRow>>,
}

/// One visual row's semantic clipboard projection.
///
/// `text` is a slice of the durable source, not the padded terminal row. Consecutive
/// rows therefore concatenate back to the exact source: visual wrapping contributes
/// no newline, while explicit source newlines remain embedded in `text`.
#[derive(Debug, Clone)]
struct CopyRow {
    content_start: u16,
    text: String,
}

/// Stable identity of one reasoning part in the append-only transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReasoningKey {
    message: usize,
    part: usize,
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
    activity: ActivityDisplay,
    /// Per-block overrides applied on top of [`Self::thinking`].
    reasoning_overrides: BTreeMap<ReasoningKey, ThinkingDisplay>,
    /// Cache invalidation for one-block disclosure changes.
    reasoning_revision: u64,
    /// Per-call overrides applied on top of [`Self::tool_output`].
    tool_overrides: BTreeMap<String, ToolDisplay>,
    /// Cache invalidation for per-call affordance changes.
    tool_revision: u64,
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
    /// Which message produced each row of the last measured line list, `None` for chrome.
    ///
    /// Built beside the rows in [`Self::cached_lines`] rather than reconstructed afterwards,
    /// which is the only way it can be right: the number of rows a message occupies is
    /// decided by the reasoning affordance, the global and per-call tool affordances, the wrap
    /// width and whether the message opens with a header — every one of them already an input
    /// to the cache key. Anything that recomputed the mapping would be a second implementation
    /// of `message_rows`, and the copy that drifted would attribute a click to the neighbouring
    /// message.
    line_owners: Vec<Option<usize>>,
    /// Which tool header produced each row of the last measured line list.
    line_tools: Vec<Option<String>>,
    /// Which reasoning header produced each row of the last measured line list.
    line_reasoning: Vec<Option<ReasoningKey>>,
    /// Where each message was drawn in the frame that **was drawn**.
    ///
    /// Absolute screen rows, recorded by [`Component::render`] from the same slice it paints,
    /// for the reason [`crate::views::ambient::SidebarView`] records its section headings the
    /// same way: this view has no `Rect` outside `render`, and a map kept anywhere else would
    /// need re-deriving on every resize, scroll and affordance toggle — and the update that is
    /// forgotten is a click landing on a message that has moved.
    hits: Vec<(Rect, usize)>,
    /// Tool-header targets from the frame that was drawn.
    tool_hits: Vec<(Rect, String)>,
    /// Reasoning-header targets from the frame that was drawn.
    reasoning_hits: Vec<(Rect, ReasoningKey)>,
    /// The transcript rectangle from the frame that was drawn.
    area: Option<Rect>,
    /// The user's application-owned selection, in content coordinates.
    selection: Option<TextSelection>,
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
            activity: ActivityDisplay::Detailed,
            reasoning_overrides: BTreeMap::new(),
            reasoning_revision: 0,
            tool_overrides: BTreeMap::new(),
            tool_revision: 0,
            offset: 0,
            content_height: 0,
            viewport_height: 0,
            following: true,
            cache: RowCache::default(),
            line_owners: Vec::new(),
            line_tools: Vec::new(),
            line_reasoning: Vec::new(),
            hits: Vec::new(),
            tool_hits: Vec::new(),
            reasoning_hits: Vec::new(),
            area: None,
            selection: None,
        }
    }

    /// Which message occupies `(column, row)` in the frame that was drawn, if any.
    ///
    /// Absolute frame coordinates, because that is what a [`crossterm::event::MouseEvent`]
    /// carries; translating at the boundary is what keeps the caller from having to know this
    /// view's geometry. `None` for chrome rows, for a row below the fold, and for every
    /// coordinate while mouse reporting is off — see [`Component::render`].
    ///
    /// The whole row is the target, rule column included. A transcript row holds nothing but
    /// one message's own text, so there is no neighbouring control a generous target could
    /// steal from, and the same argument [`crate::views::ambient::SidebarView::click`] makes
    /// about its heading rows applies unchanged.
    #[must_use]
    pub fn message_at(&self, column: u16, row: u16) -> Option<usize> {
        self.hits
            .iter()
            .find(|(area, _)| {
                column >= area.left()
                    && column < area.right()
                    && row >= area.top()
                    && row < area.bottom()
            })
            .map(|(_, index)| *index)
    }

    /// Which tool header occupies `(column, row)` in the frame that was drawn.
    #[must_use]
    pub fn tool_at(&self, column: u16, row: u16) -> Option<&str> {
        self.tool_hits
            .iter()
            .find(|(area, _)| {
                column >= area.left()
                    && column < area.right()
                    && row >= area.top()
                    && row < area.bottom()
            })
            .map(|(_, call_id)| call_id.as_str())
    }

    /// Toggle the reasoning header occupying `(column, row)`.
    ///
    /// The key stays private to the transcript so callers cannot retain an identity after
    /// replacing the transcript. Returning a boolean lets the session screen route a click
    /// without learning the append-only message/part coordinate scheme.
    pub fn toggle_reasoning_at(&mut self, column: u16, row: u16) -> bool {
        let Some(key) = self
            .reasoning_hits
            .iter()
            .find(|(area, _)| {
                column >= area.left()
                    && column < area.right()
                    && row >= area.top()
                    && row < area.bottom()
            })
            .map(|(_, key)| *key)
        else {
            return false;
        };
        let next = self.reasoning_display(key).toggled();
        self.reasoning_overrides.insert(key, next);
        self.reasoning_revision = self.reasoning_revision.wrapping_add(1);
        true
    }

    /// Discard the recorded message positions.
    ///
    /// Called by the owner on any frame that draws the welcome surface instead of this view,
    /// for the reason [`crate::views::ambient::SidebarView::forget_hit_targets`] exists: the
    /// last drawn geometry would otherwise keep answering clicks aimed at whatever now
    /// occupies those rows.
    pub fn forget_hit_targets(&mut self) {
        self.hits.clear();
        self.tool_hits.clear();
        self.reasoning_hits.clear();
        self.area = None;
        self.selection = None;
    }

    /// Flip the tool-output affordance, the `tool_details` action.
    pub fn toggle_tool_output(&mut self) {
        self.tool_output = self.tool_output.toggled();
        self.tool_overrides.clear();
        self.tool_revision = self.tool_revision.wrapping_add(1);
    }

    /// Flip one tool call without changing its neighbours.
    pub fn toggle_tool(&mut self, call_id: &str) {
        let next = self.tool_display(call_id).toggled();
        self.tool_overrides.insert(call_id.to_owned(), next);
        self.tool_revision = self.tool_revision.wrapping_add(1);
    }

    /// The current tool-output affordance.
    #[must_use]
    pub const fn tool_output(&self) -> ToolDisplay {
        self.tool_output
    }

    /// Choose the main-timeline summary or the complete activity transcript.
    pub fn set_activity_display(&mut self, display: ActivityDisplay) {
        if self.activity != display {
            self.activity = display;
            self.cache = RowCache::default();
        }
    }

    #[must_use]
    pub const fn activity_display(&self) -> ActivityDisplay {
        self.activity
    }

    fn tool_display(&self, call_id: &str) -> ToolDisplay {
        self.tool_overrides
            .get(call_id)
            .copied()
            .unwrap_or(self.tool_output)
    }

    fn reasoning_display(&self, key: ReasoningKey) -> ThinkingDisplay {
        self.reasoning_overrides
            .get(&key)
            .copied()
            .unwrap_or(self.thinking)
    }

    /// Skill identities loaded either by the host before the first request or by a
    /// successful `skill load` tool call.
    #[must_use]
    pub fn loaded_skills(&self) -> BTreeSet<LoadedSkillIdentity> {
        let mut loaded = self.transcript.loaded_skills.clone();
        loaded.extend(
            self.transcript
                .messages
                .iter()
                .flat_map(|message| &message.parts)
                .filter_map(|part| {
                    let MessagePart::Tool {
                        name,
                        arguments,
                        status: ToolStatus::Completed,
                        ..
                    } = part
                    else {
                        return None;
                    };
                    if name != "skill" {
                        return None;
                    }
                    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
                    if value.get("action").and_then(serde_json::Value::as_str) != Some("load") {
                        return None;
                    }
                    let name = value
                        .get("name")
                        .and_then(serde_json::Value::as_str)?
                        .to_owned();
                    let source = value
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    Some(LoadedSkillIdentity { name, source })
                }),
        );
        loaded
    }

    /// Begin a transcript selection at a screen coordinate.
    ///
    /// A press outside the last rendered transcript rectangle is rejected rather than
    /// clamped. Only an already-started drag is clamped, which is what lets a pointer move
    /// into the sidebar without letting a press in the sidebar create a transcript
    /// selection.
    pub fn begin_selection(&mut self, column: u16, row: u16) -> bool {
        let Some(point) = self.point_at(column, row, false) else {
            return false;
        };
        self.selection = Some(TextSelection {
            anchor: point,
            head: point,
        });
        true
    }

    /// Extend the active selection, clamped to the transcript rectangle.
    pub fn update_selection(&mut self, column: u16, row: u16) -> bool {
        let Some(point) = self.point_at(column, row, true) else {
            return false;
        };
        let Some(selection) = &mut self.selection else {
            return false;
        };
        selection.head = point;
        true
    }

    /// Clear any transcript selection.
    pub const fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Whether a transcript selection currently exists.
    #[must_use]
    pub const fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Text covered by the active transcript selection.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let area = self.area?;
        if area.width == 0 {
            return None;
        }
        let rows = self.selection_rows(area.width);
        let (start, end) = selection.ordered();
        if start.row >= rows.len() {
            return None;
        }
        let mut selected = String::new();
        let mut boundary = false;
        let last = end.row.min(rows.len().saturating_sub(1));
        for (row, copy) in rows
            .iter()
            .enumerate()
            .take(last.saturating_add(1))
            .skip(start.row)
        {
            let Some((left, right)) = selection.columns(row, area.width) else {
                continue;
            };
            let Some(copy) = copy else {
                if !selected.is_empty() {
                    boundary = true;
                }
                continue;
            };
            let content_width =
                u16::try_from(semantic_display_width(&copy.text)).unwrap_or(u16::MAX);
            let content_end = copy.content_start.saturating_add(content_width);
            let selected_left = left.max(copy.content_start);
            let selected_right = right.min(content_end);
            let slice = if copy.text == "\n" && left <= copy.content_start {
                String::from("\n")
            } else if selected_left < selected_right {
                slice_semantic_columns(
                    &copy.text,
                    selected_left.saturating_sub(copy.content_start),
                    selected_right.saturating_sub(copy.content_start),
                )
            } else {
                String::new()
            };
            if slice.is_empty() {
                continue;
            }
            if boundary && !selected.ends_with('\n') {
                selected.push('\n');
            }
            boundary = false;
            selected.push_str(&slice);
        }
        (!selected.is_empty()).then_some(selected)
    }

    fn point_at(&self, column: u16, row: u16, clamp: bool) -> Option<TextPoint> {
        let area = self.area?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if !clamp
            && (column < area.left()
                || column >= area.right()
                || row < area.top()
                || row >= area.bottom())
        {
            return None;
        }
        let column = column.clamp(area.left(), area.right().saturating_sub(1)) - area.left();
        let visible_row = row.clamp(area.top(), area.bottom().saturating_sub(1)) - area.top();
        Some(TextPoint {
            row: self.offset.saturating_add(usize::from(visible_row)),
            column,
        })
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
    pub fn toggle_thinking(&mut self) {
        self.thinking = match self.thinking {
            ThinkingDisplay::Collapsed => ThinkingDisplay::Expanded,
            ThinkingDisplay::Expanded => ThinkingDisplay::Collapsed,
        };
        self.reasoning_overrides.clear();
        self.reasoning_revision = self.reasoning_revision.wrapping_add(1);
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

    /// Measure the complete transcript at `width` before assigning its viewport.
    ///
    /// The session layout uses this to place the identity row immediately after short
    /// content and to make it sticky only once the transcript fills the available pane.
    /// It deliberately calls the cached renderer: the following `render` sees the same
    /// rows and reuses them instead of formatting the whole history twice.
    pub fn measure_content_height(&mut self, width: u16) -> usize {
        self.cached_lines(width).len()
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
        for (index, message) in self.transcript.messages.iter().enumerate() {
            lines.append(&mut self.message_rows(index, message, previous, width).lines);
            previous = Some(message.role);
        }
        self.push_margin(&mut lines, previous.is_some(), width);
        lines
    }

    fn selection_rows(&self, width: u16) -> Vec<Option<CopyRow>> {
        let mut rows = Vec::new();
        let mut previous: Option<Role> = None;
        for (index, message) in self.transcript.messages.iter().enumerate() {
            let message_rows = self.message_rows(index, message, previous, width);
            rows.extend(message_rows.copy);
            previous = Some(message.role);
        }
        if previous.is_some() {
            rows.push(None);
        }
        rows
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
        message_index: usize,
        message: &Message,
        previous: Option<Role>,
        width: u16,
    ) -> MessageRows {
        let mut lines = Vec::new();
        let mut tools = Vec::new();
        let mut reasoning = Vec::new();
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
        } else if previous.is_some() {
            // A change of speaker is the one boundary a reader scans for, so it gets
            // the stronger of the two separators: a row with no rule at all. Two
            // grades of gap is what lets the eye tell "the other party is talking now"
            // from "this reply took another step" without reading either row.
            lines.push(padded("", width, self.context.surface()));
        }
        if message.role == Role::User {
            self.push_boxed(
                message_index,
                message,
                rule,
                width,
                &mut lines,
                &mut reasoning,
            );
            let copy = self.copy_rows(message, previous, width, &lines);
            return MessageRows {
                lines,
                tools,
                reasoning,
                copy,
            };
        }
        if previous != Some(message.role)
            && let Some(label) = self.role_label(message.role)
        {
            lines.push(self.role_header(message.role, rule, label, width));
        }
        let mut parts = message.parts.iter().enumerate().peekable();
        while let Some((part_index, part)) = parts.next() {
            if self.activity == ActivityDisplay::Summary && compact_activity(part).is_some() {
                let mut compacted = vec![part];
                while let Some((_, next)) = parts.peek()
                    && compact_activity(next).is_some()
                {
                    compacted.push(parts.next().expect("peeked part exists").1);
                }
                if compacted.len() == 1 {
                    if let MessagePart::Tool { call_id, .. } = part {
                        tools.push((lines.len(), call_id.clone()));
                    }
                    let key = ReasoningKey {
                        message: message_index,
                        part: part_index,
                    };
                    if matches!(part, MessagePart::Reasoning { .. }) {
                        reasoning.push((lines.len(), key));
                    }
                    self.part_lines(
                        message.role,
                        rule,
                        part,
                        self.reasoning_display(key),
                        width,
                        &mut lines,
                    );
                } else {
                    self.activity_lines(message.role, rule, &compacted, width, &mut lines);
                }
                continue;
            }
            if let MessagePart::Tool { call_id, .. } = part {
                tools.push((lines.len(), call_id.clone()));
            }
            let key = ReasoningKey {
                message: message_index,
                part: part_index,
            };
            if matches!(part, MessagePart::Reasoning { .. }) {
                reasoning.push((lines.len(), key));
            }
            self.part_lines(
                message.role,
                rule,
                part,
                self.reasoning_display(key),
                width,
                &mut lines,
            );
        }
        let copy = self.copy_rows(message, previous, width, &lines);
        MessageRows {
            lines,
            tools,
            reasoning,
            copy,
        }
    }

    fn copy_rows(
        &self,
        message: &Message,
        previous: Option<Role>,
        width: u16,
        lines: &[Line<'static>],
    ) -> Vec<Option<CopyRow>> {
        let separator_rows = usize::from(previous.is_some());
        let gutter = u16::try_from(display_width(message.role.marker()) + 1).unwrap_or(2);
        let mut first_content = separator_rows;
        let mut last_content = lines.len();
        if message.role == Role::User {
            let edge = u16::try_from(display_width(USER_BOX_RIGHT)).unwrap_or(1);
            let inner = width.saturating_sub(gutter.saturating_add(edge));
            if inner >= USER_BOX_MIN_INNER_COLS {
                first_content = first_content.saturating_add(1);
                last_content = last_content.saturating_sub(1);
            } else if self.role_label(Role::User).is_some() {
                first_content = first_content.saturating_add(1);
            }
        } else if previous != Some(message.role) && self.role_label(message.role).is_some() {
            first_content = first_content.saturating_add(1);
        }
        first_content = first_content.min(lines.len());
        last_content = last_content.max(first_content).min(lines.len());

        let mut copy = vec![None; lines.len()];
        let content_indices = (first_content..last_content).collect::<Vec<_>>();
        if content_indices.is_empty() {
            return copy;
        }
        let rendered = content_indices
            .iter()
            .map(|index| {
                let text = lines[*index]
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let right = width.saturating_sub(u16::from(message.role == Role::User));
                slice_columns(&text, gutter, right).trim_end().to_owned()
            })
            .collect::<Vec<_>>();

        let source = message
            .parts
            .iter()
            .map(|part| match part {
                MessagePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join("\n"));
        let semantic = source.map_or_else(
            || rendered.clone(),
            |source| partition_semantic_source(&source, &rendered),
        );
        for ((index, text), rendered) in content_indices.into_iter().zip(semantic).zip(rendered) {
            let text = if text.is_empty() && rendered.is_empty() {
                String::from("\n")
            } else {
                text
            };
            copy[index] = Some(CopyRow {
                content_start: gutter,
                text,
            });
        }
        copy
    }

    fn activity_lines(
        &self,
        role: Role,
        rule: Style,
        parts: &[&MessagePart],
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let mut activity = zuno_types::ActivityProjection::default();
        for part in parts {
            let kind = compact_activity(part).expect("activity group contains compactable parts");
            activity.record(kind);
        }
        let mut labels = Vec::new();
        let mut push = |count: usize, singular: &str, plural: &str| {
            if count > 0 {
                labels.push(format!(
                    "{count} {}",
                    if count == 1 { singular } else { plural }
                ));
            }
        };
        push(activity.commands, "command", "commands");
        push(activity.reads, "read", "reads");
        push(activity.searches, "search", "searches");
        push(activity.delegations, "delegation", "delegations");
        push(activity.images, "image", "images");
        push(activity.tools, "tool", "tools");
        let row = format!(" • {} · Ctrl+T details", labels.join(" · "));
        out.push(self.ruled(role, rule, &row, self.context.muted(), width));

        let gutter = u16::try_from(display_width(role.marker()) + 1).unwrap_or(2);
        let body_width = width.saturating_sub(gutter);
        for part in parts {
            if let Some(row) = Self::activity_item_line(part, body_width) {
                out.push(self.ruled(role, rule, &row, self.context.secondary(), width));
            }
        }
    }

    fn activity_item_line(part: &MessagePart, body_width: u16) -> Option<String> {
        match part {
            MessagePart::Tool {
                name,
                arguments,
                title,
                ..
            } => {
                let (icon, _) = tool_affordance(name);
                let activity = compact_activity(part)?;
                let (label, separator) = match activity {
                    zuno_types::ActivityKind::Command => (None, " "),
                    zuno_types::ActivityKind::Read => (Some("read"), " · "),
                    zuno_types::ActivityKind::Search => (Some("search"), " · "),
                    zuno_types::ActivityKind::Delegation => (Some("task"), " · "),
                    zuno_types::ActivityKind::Image => (Some("image"), " · "),
                    zuno_types::ActivityKind::Tool => (Some(name.as_str()), " · "),
                };
                let prefix = label.map_or_else(
                    || format!("   {icon}"),
                    |label| format!("   {icon} {label}"),
                );
                let detail_room = usize::from(body_width)
                    .saturating_sub(display_width(&prefix))
                    .saturating_sub(display_width(separator));
                let detail = crate::views::tool::summary(name, arguments)
                    .map(|summary| summary.fit(detail_room))
                    .or_else(|| {
                        title
                            .as_deref()
                            .filter(|title| !title.is_empty())
                            .map(|title| truncate(title, detail_room))
                    })
                    .filter(|detail| !detail.is_empty());
                let row = detail.map_or(prefix.clone(), |detail| {
                    format!("{prefix}{separator}{detail}")
                });
                Some(truncate(&row, usize::from(body_width)))
            }
            MessagePart::Attachment { filename, .. } => {
                let prefix = "   ⎘ attachment · ";
                let room = usize::from(body_width).saturating_sub(display_width(prefix));
                let filename = crate::views::ambient::elide_left(filename, room);
                Some(truncate(
                    &format!("{prefix}{filename}"),
                    usize::from(body_width),
                ))
            }
            _ => None,
        }
    }

    /// The user's message as a closed box: a titled top rule, the body, a closing rule.
    ///
    /// # Why the user's turn is framed and the assistant's is not
    ///
    /// Both sides used to be a label over flowing text, told apart only by one glyph in
    /// column zero — `▌` against `│`. Reported: a long conversation was hard to scan,
    /// because "what I asked" and "what it answered" look the same from a metre away and
    /// the glyph that distinguishes them is one cell wide. Upstream frames the same side
    /// and leaves the other unframed (`routes/session/index.tsx:1395-1420`), and a real
    /// `opencode 1.18.18` pane confirms it: the user's turn carries a rule down its left
    /// edge while the reply is bare prose.
    ///
    /// So only one side is framed, and it is the user's, for two reasons that point the
    /// same way. The prompt is short and finite, so a frame costs it two rows out of
    /// three or four rather than two out of forty; and prose the model wrote is the thing
    /// being *read*, which a box interrupts. Framing both would restore the symmetry this
    /// exists to break.
    ///
    /// # The box's edges are the crate's existing vocabulary, not new glyphs
    ///
    /// The left edge is [`Role::marker`]'s own `▌`, unchanged, so every positional
    /// assertion that reads a user row at column zero still reads one and the accent
    /// colour still runs down the turn. The right edge is
    /// [`crate::views::session::COMPOSER_RIGHT_RULE`]'s `▐`, which is already what this
    /// crate uses to close a region on the right. Nothing here invents a symbol, which is
    /// why the box reads as part of the same surface family as the composer.
    ///
    /// # The heading rides the top rule instead of owning a row
    ///
    /// A separate `You` row plus a top rule would spend two rows on chrome before the
    /// first word. The label sits *in* the rule — `▌ You ─────▐` — so the box costs the
    /// same two rows the old header-plus-nothing arrangement did, and the top row still
    /// contains the literal `▌ You` every existing assertion looks for.
    ///
    /// The rule is dropped rather than the label when the pane cannot hold both: a
    /// truncated `Yo` names nobody, while a label with no dashes beside it is still a
    /// heading.
    fn push_boxed(
        &self,
        message_index: usize,
        message: &Message,
        rule: Style,
        width: u16,
        out: &mut Vec<Line<'static>>,
        reasoning: &mut Vec<(usize, ReasoningKey)>,
    ) {
        let marker = Role::User.marker();
        // The columns the frame itself spends: the marker, the space after it, and the
        // right edge. Everything below is measured against what is left, so a pane too
        // narrow to hold a body degrades to the unframed rows rather than to a box with
        // negative width.
        let gutter = u16::try_from(display_width(marker) + 1).unwrap_or(2);
        let edge = u16::try_from(display_width(USER_BOX_RIGHT)).unwrap_or(1);
        let inner = width.saturating_sub(gutter.saturating_add(edge));
        if inner < USER_BOX_MIN_INNER_COLS {
            // No frame at all rather than a broken one, the same degradation the composer
            // rules and the ambient panel make. The header is restored here because the
            // top rule that would have carried it is what has just been dropped.
            if let Some(label) = self.role_label(Role::User) {
                out.push(self.ruled(Role::User, rule, label, self.context.title(), width));
            }
            for (part_index, part) in message.parts.iter().enumerate() {
                let key = ReasoningKey {
                    message: message_index,
                    part: part_index,
                };
                if matches!(part, MessagePart::Reasoning { .. }) {
                    reasoning.push((out.len(), key));
                }
                self.part_lines(
                    Role::User,
                    rule,
                    part,
                    self.reasoning_display(key),
                    width,
                    out,
                );
            }
            return;
        }
        out.push(self.boxed_edge(rule, self.role_label(Role::User), inner));
        let mut body = Vec::new();
        for (part_index, part) in message.parts.iter().enumerate() {
            let key = ReasoningKey {
                message: message_index,
                part: part_index,
            };
            if matches!(part, MessagePart::Reasoning { .. }) {
                reasoning.push((out.len() + body.len(), key));
            }
            self.part_lines(
                Role::User,
                rule,
                part,
                self.reasoning_display(key),
                gutter + inner,
                &mut body,
            );
        }
        for mut line in body {
            line.spans
                .push(Span::styled(USER_BOX_RIGHT.to_owned(), rule));
            out.push(line);
        }
        out.push(self.boxed_edge(rule, None, inner));
    }

    /// One horizontal rule of the user's box, carrying `label` when it has one.
    fn boxed_edge(&self, rule: Style, label: Option<&'static str>, inner: u16) -> Line<'static> {
        let marker = Role::User.marker();
        let mut spans = vec![Span::styled(format!("{marker} "), rule)];
        let mut spent = 0usize;
        if let Some(label) = label {
            let text = format!("{label} ");
            spent = display_width(&text);
            spans.push(Span::styled(text, self.context.title()));
        }
        let dashes = usize::from(inner).saturating_sub(spent);
        spans.push(Span::styled(USER_BOX_RULE.repeat(dashes), rule));
        spans.push(Span::styled(USER_BOX_RIGHT.to_owned(), rule));
        Line::from(spans)
    }

    /// One bottom margin separating the reply from the sticky identity row.
    ///
    /// Transient liveness no longer belongs to the transcript. It is rendered in the
    /// composer's live control row, where it cannot be persisted, selected as message
    /// content, or duplicated beside a running tool spinner.
    fn push_margin(&self, lines: &mut Vec<Line<'static>>, any_message: bool, width: u16) {
        if any_message {
            lines.push(padded("", width, self.context.surface()));
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
    /// * **the display affordances**, `thinking`, `tool_output`, and `tool_revision`, which
    ///   decide how many rows reasoning blocks and globally or individually expanded tool
    ///   results produce.
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
        // Cleared here rather than in `render`, because this is the function that knows how
        // many rows each message produced; a `render` that cleared it would be describing a
        // list it did not build.
        self.line_owners.clear();
        self.line_tools.clear();
        self.line_reasoning.clear();
        let mut previous: Option<Role> = None;
        for index in 0..self.transcript.messages.len() {
            let message = &self.transcript.messages[index];
            let key = RowKey {
                width,
                thinking: self.thinking,
                reasoning_revision: self.reasoning_revision,
                tool_output: self.tool_output,
                activity: self.activity,
                tool_revision: self.tool_revision,
                previous,
                content: fingerprint(message),
            };
            previous = Some(message.role);
            if let Some(rows) = self.cache.get(index, &key, &theme) {
                self.line_owners
                    .extend(std::iter::repeat_n(Some(index), rows.lines.len()));
                let mut tools = vec![None; rows.lines.len()];
                for (row, call_id) in &rows.tools {
                    if let Some(slot) = tools.get_mut(*row) {
                        *slot = Some(call_id.clone());
                    }
                }
                self.line_tools.extend(tools);
                let mut reasoning = vec![None; rows.lines.len()];
                for (row, key) in &rows.reasoning {
                    if let Some(slot) = reasoning.get_mut(*row) {
                        *slot = Some(*key);
                    }
                }
                self.line_reasoning.extend(reasoning);
                lines.extend(rows.lines.iter().cloned());
                continue;
            }
            let rows =
                self.message_rows(index, &self.transcript.messages[index], key.previous, width);
            self.line_owners
                .extend(std::iter::repeat_n(Some(index), rows.lines.len()));
            let mut tools = vec![None; rows.lines.len()];
            for (row, call_id) in &rows.tools {
                if let Some(slot) = tools.get_mut(*row) {
                    *slot = Some(call_id.clone());
                }
            }
            self.line_tools.extend(tools);
            let mut reasoning = vec![None; rows.lines.len()];
            for (row, key) in &rows.reasoning {
                if let Some(slot) = reasoning.get_mut(*row) {
                    *slot = Some(*key);
                }
            }
            self.line_reasoning.extend(reasoning);
            lines.extend(rows.lines.iter().cloned());
            if is_recallable(&self.transcript.messages[index]) {
                self.cache.put(index, key, Arc::clone(&theme), rows);
            } else {
                self.cache.forget(index);
            }
        }
        self.cache.truncate_to(self.transcript.messages.len());
        self.push_margin(&mut lines, previous.is_some(), width);
        // The margin belongs to no message, so a click on it falls through. Padding to the
        // full length rather than leaving the vector short keeps `render` from having to
        // know which rows are chrome.
        self.line_owners.resize(lines.len(), None);
        self.line_tools.resize(lines.len(), None);
        self.line_reasoning.resize(lines.len(), None);
        lines
    }

    /// The heading a change of speaker prints, when the speaker is a party to the
    /// conversation.
    ///
    /// `None` for [`Role::System`], and that absence is the whole change. The session is
    /// not a speaker: everything it says is already marked as its own by the `▲` rule at
    /// column zero and by a level glyph — `✓`, `!`, `✗` — that no party's text carries. A
    /// `Session` heading therefore restated what the row beneath it already said, and it
    /// restated it *at the top of the frame*: a one-line `model set to … for the next
    /// turn` opened a fresh conversation with a blank row, a `Session` heading and the
    /// notice, three rows of which one was content. That was reported, in the owner's
    /// words, as a session hint that need not be shown at all on a first conversation.
    ///
    /// The blank separator above it is deliberately kept — see [`Self::message_rows`] —
    /// because it is what stops a notice from reading as the tail of the reply above it.
    /// Only the heading goes.
    const fn role_label(&self, role: Role) -> Option<&'static str> {
        match role {
            Role::User => Some("You"),
            Role::Assistant => Some("Assistant"),
            Role::System => None,
        }
    }

    /// A speaker title followed by a weak rule, so a new answer is visible without boxing
    /// the prose or painting a full-width accent stripe.
    fn role_header(&self, role: Role, rule: Style, label: &str, width: u16) -> Line<'static> {
        let room = usize::from(width).saturating_sub(display_width(role.marker()) + 1);
        self.ruled_spans(role, rule, self.title_rule_spans("", label, room), width)
    }

    /// A neutral title plus a weak trailing rule within `room` terminal columns.
    fn title_rule_spans(&self, inset: &str, label: &str, room: usize) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if !inset.is_empty() {
            spans.push(Span::styled(inset.to_owned(), self.context.surface()));
        }
        spans.push(Span::styled(label.to_owned(), self.context.title()));
        let used = display_width(inset).saturating_add(display_width(label));
        if used < room {
            spans.push(Span::styled(String::from(" "), self.context.surface()));
            let rule = room.saturating_sub(used).saturating_sub(1);
            if rule > 0 {
                spans.push(Span::styled(
                    USER_BOX_RULE.repeat(rule),
                    self.context.muted(),
                ));
            }
        }
        spans
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

    /// Rebase Markdown chrome onto neutral semantic roles while retaining the parser's
    /// emphasis, heading, code, link, and syntax-highlight modifiers.
    fn neutral_markdown_row(&self, mut row: Vec<Span<'static>>) -> Vec<Span<'static>> {
        let heading = {
            let palette = self.context.palette();
            ratatui::style::Color::from(palette.markdown_heading)
        };
        let title = self.context.title();
        let muted = self.context.muted();
        for span in &mut row {
            let text = span.content.as_ref();
            let ordered = text
                .strip_suffix(' ')
                .and_then(|text| text.strip_suffix('.'))
                .is_some_and(|number| {
                    !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
                });
            let chrome = ordered
                || matches!(text, "• " | "◦ " | "▪ " | "│ " | "[ ] " | "[x] " | "[X] ")
                || (!text.is_empty() && text.chars().all(|character| character == '─'));
            if span.style.fg == Some(heading) {
                span.style.fg = title.fg;
                span.style.bg = title.bg;
            } else if chrome {
                span.style.fg = muted.fg;
                span.style.bg = muted.bg;
            }
        }
        row
    }

    fn part_lines(
        &self,
        role: Role,
        rule: Style,
        part: &MessagePart,
        thinking: ThinkingDisplay,
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let gutter = u16::try_from(display_width(role.marker()) + 1).unwrap_or(2);
        let body_width = width.saturating_sub(gutter);
        let push = |body: &str, style: Style, out: &mut Vec<Line<'static>>| {
            out.push(self.ruled(role, rule, body, style, width));
        };
        match part {
            // Both parties author CommonMark on this surface. Rendering the user's source
            // with the same parser is what keeps pasted tables, constraint lists and code
            // fences legible instead of showing their punctuation as an unaligned wall.
            // The durable part still retains the exact source for export and replay.
            MessagePart::Text { text } if role != Role::System => {
                for row in crate::views::markdown::render(text, body_width, &self.context.palette())
                {
                    out.push(self.ruled_spans(role, rule, self.neutral_markdown_row(row), width));
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
                let state = match (duration_secs, streaming) {
                    (_, true) => String::from("thinking…"),
                    (Some(secs), false) => format!("{secs:.1}s"),
                    (None, false) => String::from("complete"),
                };
                // Inset one column past the prose, the same inset a tool call takes. Both are
                // things that happened *inside* the reply rather than being the reply, so
                // they share one indentation vocabulary: a reader learns the rule once and
                // then reads column 2 as "the answer" and column 3 as "the work". Flush with
                // the prose, a `thought for 12.0s` header sat exactly where the first
                // sentence of the answer sits and competed with it for the eye.
                let action = if self.context.config.mouse {
                    "click"
                } else {
                    "/thinking"
                };
                let header = format!(" {} ◇ Thought · {state}", thinking.glyph());
                match thinking {
                    ThinkingDisplay::Collapsed => {
                        // One row, not two. Reasoning is secondary content that recurs on
                        // every step of every turn, so a collapsed block that spent two
                        // rows could out-measure the answer it precedes. The summary rides
                        // on the header instead of owning a row, and is dropped rather
                        // than wrapped when it does not fit: the glyph and the duration
                        // are what the row is for, and a summary continued onto a second
                        // row would spend exactly the row this form exists to save.
                        let row = summary(text).map_or_else(
                            || format!("{header} · {action}"),
                            |gist| {
                                let with_both = format!("{header} · {gist} · {action}");
                                if display_width(&with_both) <= usize::from(body_width) {
                                    return with_both;
                                }
                                let with_summary = format!("{header} · {gist}");
                                if display_width(&with_summary) <= usize::from(body_width) {
                                    with_summary
                                } else {
                                    format!("{header} · {action}")
                                }
                            },
                        );
                        push(&row, self.context.thinking(), out);
                    }
                    ThinkingDisplay::Expanded => {
                        push(
                            &format!("{header} · {action} to collapse"),
                            self.context.thinking(),
                            out,
                        );
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
                call_id,
                name,
                display_name,
                arguments,
                title,
                status,
                output,
                diff,
                ui_intent,
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
                let display = self.tool_display(call_id);
                // The argument that matters is the whole of §7.5. `title` is no longer
                // preferred over the arguments: a completed `read` reported `Read
                // diff.rs`, which names the kind of work and drops the path, so six reads
                // in one turn produced six rows a reader could not tell apart. Non-command
                // tools retain their display identity beside the summary because a path
                // alone is ambiguous. Commands are different: the summary is the exact
                // submitted command, so prepending the interpreter would fabricate a
                // different command when the row is copied.
                //
                // `title` remains the fallback for a completed call whose arguments never
                // parsed, because a provider's own sentence beats a bare wire name.
                let summary = crate::views::tool::summary(name, arguments);
                let has_summary = summary.is_some();
                let activity = compact_activity(part);
                let command = activity == Some(zuno_types::ActivityKind::Command);
                let prefix = format!(" {} {glyph} Tool · {icon} ", display.glyph());
                let identity = if command {
                    summary
                        .as_ref()
                        .map(|summary| {
                            summary
                                .fit(usize::from(body_width).saturating_sub(display_width(&prefix)))
                        })
                        .filter(|summary| !summary.is_empty())
                        .or_else(|| title.clone().filter(|title| !title.is_empty()))
                        .unwrap_or_else(|| display_name.clone())
                } else if has_summary {
                    display_name.clone()
                } else if let Some(title) = title.as_deref() {
                    title.to_owned()
                } else if *status == ToolStatus::Pending {
                    placeholder.to_owned()
                } else {
                    name.clone()
                };
                // Measured against what the header actually spent, not against a
                // constant. Commands already use their submitted text as the identity,
                // so they must not receive a second, interpreter-prefixed detail.
                let room = usize::from(body_width)
                    .saturating_sub(display_width(&prefix))
                    .saturating_sub(display_width(&identity))
                    .saturating_sub(1);
                let detail = (!command)
                    .then_some(summary.as_ref())
                    .flatten()
                    .map(|summary| summary.fit(room))
                    .filter(|summary| !summary.is_empty());
                let styles = crate::views::tool::header_styles(*status, *ui_intent, &self.context);
                let mut spans = vec![
                    Span::styled(String::from(" "), self.context.surface()),
                    Span::styled(display.glyph().to_owned(), styles.chrome),
                    Span::styled(String::from(" "), self.context.surface()),
                    Span::styled(glyph.to_owned(), styles.status),
                    Span::styled(String::from(" "), self.context.surface()),
                    Span::styled(String::from("Tool"), styles.title),
                    Span::styled(String::from(" · "), styles.chrome),
                    Span::styled(icon.to_owned(), styles.chrome),
                    Span::styled(String::from(" "), self.context.surface()),
                    Span::styled(identity, styles.title),
                ];
                if let Some(detail) = detail {
                    spans.push(Span::styled(String::from(" "), self.context.surface()));
                    spans.push(Span::styled(detail, styles.detail));
                }
                out.push(self.ruled_spans(role, rule, spans, width));
                let frame = RowFrame { role, rule, width };
                if display == ToolDisplay::Expanded {
                    self.tool_argument_lines(frame, arguments, out);
                    if diff.is_some() || output.is_some() {
                        self.tool_section_label(frame, "Result", out);
                    }
                }
                // A patch travels beside the output rather than inside it, so a result that
                // has one is rendered from it — and before this it was rendered from neither.
                // `tool_output_lines` only ever diff-sniffed `output`, and every mutating
                // tool's output is a *sentence* (`applied 1 change`), so the patch that
                // `TurnEvent::ToolDispatchCompleted` had faithfully carried all the way here
                // was dropped on the floor at the last step. The diff viewer could open it;
                // the transcript could not show it.
                match (diff, output) {
                    (Some(patch), _) => {
                        self.tool_result_lines(frame, name, patch, *status, display, out);
                    }
                    (None, Some(output)) => {
                        if *ui_intent == zuno_tool::ToolUiIntent::Subagent
                            && let Some(envelope) = crate::views::subagent::output_envelope(output)
                        {
                            let detail = format!("{}{}", Self::RESULT_INSET, envelope.detail);
                            push(&detail, self.context.secondary(), out);
                            if !envelope.result.is_empty() {
                                self.tool_result_lines(
                                    frame,
                                    name,
                                    &envelope.result,
                                    *status,
                                    display,
                                    out,
                                );
                            }
                        } else {
                            self.tool_result_lines(frame, name, output, *status, display, out);
                        }
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
            MessagePart::ReplayData { .. } => {}
        }
    }

    /// The inset a tool result's rows are laid out at.
    ///
    /// Three columns past the rule, so a result row starts under its call row's icon
    /// rather than under the call row's own inset. Rule (2) plus this (3) is §7.5's
    /// five-column continuation indent, and the alignment under the icon is what makes a
    /// long result read as belonging to the call above it instead of floating between two.
    const RESULT_INSET: &'static str = "   ";

    /// The inset for structured tool-call details beneath a section label.
    const DETAIL_INSET: &'static str = "     ";

    /// Expanded input stays inspectable without allowing one malformed call to allocate an
    /// unbounded frame. A visible overflow row makes either limit explicit.
    const TOOL_ARGUMENT_ROWS: usize = 120;
    const TOOL_ARGUMENT_CHARS: usize = 16_000;

    fn tool_section_label(&self, frame: RowFrame, label: &str, out: &mut Vec<Line<'static>>) {
        let room = usize::from(frame.width)
            .saturating_sub(display_width(frame.role.marker()).saturating_add(1));
        out.push(self.ruled_spans(
            frame.role,
            frame.rule,
            self.title_rule_spans(Self::RESULT_INSET, label, room),
            frame.width,
        ));
    }

    fn tool_argument_lines(&self, frame: RowFrame, arguments: &str, out: &mut Vec<Line<'static>>) {
        if arguments.trim().is_empty() {
            return;
        }
        self.tool_section_label(frame, "Arguments", out);
        let formatted = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| arguments.to_owned());
        let (formatted, capped) = match formatted.char_indices().nth(Self::TOOL_ARGUMENT_CHARS) {
            Some((cut, _)) => (&formatted[..cut], true),
            None => (formatted.as_str(), false),
        };
        let body_width = frame.width.saturating_sub(frame.gutter(Self::DETAIL_INSET));
        let rows = Self::indented_detail_rows(formatted, body_width);
        let total = rows.len();
        for row in rows.iter().take(Self::TOOL_ARGUMENT_ROWS) {
            out.push(self.ruled(
                frame.role,
                frame.rule,
                &format!("{}{row}", Self::DETAIL_INSET),
                self.context.tool_output(),
                frame.width,
            ));
        }
        if total > Self::TOOL_ARGUMENT_ROWS || capped {
            let mut notice = format!("{}{ELIDED}", Self::DETAIL_INSET);
            if total > Self::TOOL_ARGUMENT_ROWS {
                notice.push_str(&format!(
                    " {} more argument lines",
                    total - Self::TOOL_ARGUMENT_ROWS
                ));
            }
            if capped {
                if total > Self::TOOL_ARGUMENT_ROWS {
                    notice.push(',');
                }
                notice.push_str(&format!(" cut at {} chars", Self::TOOL_ARGUMENT_CHARS));
            }
            out.push(self.ruled(
                frame.role,
                frame.rule,
                &notice,
                self.context.warning(),
                frame.width,
            ));
        }
    }

    fn indented_detail_rows(text: &str, width: u16) -> Vec<String> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for line in text.split('\n') {
            let content = line.trim_start_matches(char::is_whitespace);
            let leading = &line[..line.len() - content.len()];
            let indent = truncate(leading, usize::from(width.saturating_sub(1)));
            let indent_width = u16::try_from(display_width(&indent)).unwrap_or(u16::MAX);
            let content_width = width.saturating_sub(indent_width).max(1);
            for row in wrap(content, content_width) {
                rows.push(format!("{indent}{row}"));
            }
        }
        rows
    }

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
        display: ToolDisplay,
        out: &mut Vec<Line<'static>>,
    ) {
        let RowFrame { role, rule, width } = frame;
        let body_width = width.saturating_sub(frame.gutter(Self::RESULT_INSET));
        let marker = role.marker();
        let budget = crate::views::tool::output_budget(name, display);
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
        // The status remains on the header. Result prose itself stays readable instead of
        // tinting an entire multi-line block red or low-contrast grey; only blocked guidance
        // keeps warning colour because it asks the user to act.
        let body = match status {
            ToolStatus::Error | ToolStatus::Completed => self.context.tool_output(),
            ToolStatus::Blocked | ToolStatus::Cancelled => self.context.warning(),
            ToolStatus::Pending | ToolStatus::Running => self.context.secondary(),
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
            self.context.muted(),
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
/// the perf plan exists to remove rather than relocate.
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
    reasoning_revision: u64,
    tool_output: ToolDisplay,
    activity: ActivityDisplay,
    tool_revision: u64,
    previous: Option<Role>,
    content: u64,
}

/// One message's recalled rows, beside the inputs that produced them.
struct CachedRows {
    key: RowKey,
    theme: Arc<crate::theme::Resolved>,
    rows: MessageRows,
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
    ) -> Option<&MessageRows> {
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
        Some(&self.slots[index].as_ref()?.rows)
    }

    /// Store `rows` for `index`, evicting the oldest entries to stay inside the bound.
    fn put(
        &mut self,
        index: usize,
        key: RowKey,
        theme: Arc<crate::theme::Resolved>,
        rows: MessageRows,
    ) {
        // A message taller than the whole budget is never stored. Storing it would evict
        // every other entry to make room for one that cannot be reused often enough to
        // pay for that, and the eviction loop below could not reach the budget anyway.
        if rows.lines.len() > MAX_CACHED_ROWS {
            self.forget(index);
            return;
        }
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        self.forget(index);
        self.rows += rows.lines.len();
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
            self.rows -= entry.rows.lines.len();
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
                display_name,
                arguments,
                title,
                status,
                output,
                diff,
                ..
            } => {
                2_u8.hash(&mut hasher);
                call_id.hash(&mut hasher);
                name.hash(&mut hasher);
                display_name.hash(&mut hasher);
                arguments.hash(&mut hasher);
                title.hash(&mut hasher);
                match status {
                    ToolStatus::Pending => 0_u8,
                    ToolStatus::Running => 1,
                    ToolStatus::Completed => 2,
                    ToolStatus::Blocked => 3,
                    ToolStatus::Cancelled => 4,
                    ToolStatus::Error => 5,
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
            MessagePart::ReplayData { .. } => {
                // Replay data is intentionally not rendered. The variant tag and part count
                // are enough to distinguish it from visible content while allowing messages
                // whose hidden metadata differs to share identical transcript rows.
                7_u8.hash(&mut hasher);
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
        if self
            .area
            .is_some_and(|previous| previous.width != area.width)
        {
            // Content coordinates are row/column positions after wrapping. A width change
            // changes both, so retaining the old points would highlight unrelated text.
            self.selection = None;
        }
        self.area = Some(area);
        // Cleared before anything can return early, so a frame that draws nothing leaves no
        // target behind — the same discipline `SidebarView::render` keeps, and for the same
        // reason: stale geometry answers clicks aimed at whatever now occupies those rows.
        self.hits.clear();
        self.tool_hits.clear();
        self.reasoning_hits.clear();
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
        // Recorded from the same `skip`/`take` window the rows below are drawn through, so a
        // scrolled transcript's targets move with it and a row below the fold has none. Gated
        // on the mouse setting for the reason the sidebar gates its own: with reporting off no
        // press ever arrives, and a map maintained for nobody is a map that can be wrong
        // unnoticed.
        if self.context.config.mouse {
            self.hits = self
                .line_owners
                .iter()
                .skip(self.offset)
                .take(self.viewport_height)
                .enumerate()
                .filter_map(|(row, owner)| {
                    let owner = (*owner)?;
                    let y = area.y.checked_add(u16::try_from(row).ok()?)?;
                    Some((
                        Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: 1,
                        },
                        owner,
                    ))
                })
                .collect();
            self.tool_hits = self
                .line_tools
                .iter()
                .skip(self.offset)
                .take(self.viewport_height)
                .enumerate()
                .filter_map(|(row, call_id)| {
                    let call_id = call_id.as_ref()?.clone();
                    let y = area.y.checked_add(u16::try_from(row).ok()?)?;
                    Some((
                        Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: 1,
                        },
                        call_id,
                    ))
                })
                .collect();
            self.reasoning_hits = self
                .line_reasoning
                .iter()
                .skip(self.offset)
                .take(self.viewport_height)
                .enumerate()
                .filter_map(|(row, key)| {
                    let key = (*key)?;
                    let y = area.y.checked_add(u16::try_from(row).ok()?)?;
                    Some((
                        Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: 1,
                        },
                        key,
                    ))
                })
                .collect();
        }
        let visible = lines
            .into_iter()
            .skip(self.offset)
            .take(self.viewport_height)
            .collect::<Vec<_>>();
        Paragraph::new(visible)
            .style(self.context.surface())
            .render(area, frame.buffer_mut());
        if let Some(selection) = self.selection {
            let selected = self.context.selected();
            for visible_row in 0..area.height {
                let content_row = self.offset.saturating_add(usize::from(visible_row));
                let Some((left, right)) = selection.columns(content_row, area.width) else {
                    continue;
                };
                for column in left..right {
                    frame.buffer_mut()[(area.x + column, area.y + visible_row)].set_style(selected);
                }
            }
        }
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
            AppEvent::AnimationFrame => {
                if self.transcript.advance_animation() {
                    EventResult::REDRAW
                } else {
                    EventResult::IGNORED
                }
            }
            AppEvent::Terminal(_) => EventResult::IGNORED,
        }
    }
}

/// The identity attached to the current reply.
///
/// This view owns only the durable-looking identity a reader associates with the
/// response: agent, model, and reasoning level. Transient liveness, context occupancy,
/// interrupt controls, branch, and command discovery belong to the composer's live
/// footer instead. Keeping those surfaces separate lets a short reply carry its identity
/// immediately below the content while the controls remain fixed and discoverable.
pub struct StatusView {
    context: ViewContext,
    running: bool,
    agent: Option<String>,
    model: Option<String>,
    /// What the session was configured with, shown before the first turn resolves.
    ///
    /// Separate from `agent`/`model`, which are what the *engine* resolved: carrying
    /// the configured pair in the same fields would make the identity claim a turn had
    /// resolved a model before one had run, and clearing them at the end of a turn
    /// would then blank a row that is still true.
    configured_agent: Option<String>,
    configured_model: Option<String>,
    /// Display names keyed by the exact `provider/model` value used on the wire.
    ///
    /// The picker already receives these names from the catalog. Reusing the same map
    /// keeps the reply identity readable (`Claude Opus 5`) without teaching the view to
    /// guess names from provider-specific ids.
    model_names: BTreeMap<String, String>,
    /// Why a mounted prompt is asking the user to decide.
    ///
    /// The session footer is the one row always on screen, so the screen reads this
    /// state when deciding whether to show a pulse or an explicit user-input request.
    awaiting_user: Option<AwaitingUser>,
    /// The reasoning level the session asked the model for, when it asked for one.
    ///
    /// Beside `configured_model` rather than among the resolved fields, and surviving
    /// [`Self::reset`] for the same reason: it is what the *composer is set to*, not
    /// something a turn reports. No engine event carries it.
    ///
    /// `None` renders nothing at all. That is the honest answer for a model with no
    /// reasoning support, where a level would name a control the request does not send.
    effort: Option<zuno_llm::effort::ReasoningEffort>,
}

/// Occupancy of the model window for the most recent provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextWindowUsage {
    /// Tokens in the complete most recent prompt, using provider accounting semantics.
    pub prompt_tokens: u64,
    /// Model-declared maximum prompt window.
    pub limit: u64,
    /// True until provider usage confirms this locally estimated request.
    pub estimated: bool,
}

impl ContextWindowUsage {
    /// Percentage with one decimal place, without discarding the denominator.
    #[must_use]
    pub fn percent(self) -> f64 {
        if self.limit == 0 {
            return 0.0;
        }
        self.prompt_tokens as f64 * 100.0 / self.limit as f64
    }
}

/// Availability of the durable session usage projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UsageState {
    /// No provider report has arrived in a new session.
    #[default]
    NotReported,
    /// Every stored assistant snapshot can be normalized.
    Known,
    /// Historical snapshots lack enough accounting information to be reliable.
    Unavailable,
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
    /// A reply identity over `context`.
    #[must_use]
    pub fn new(context: ViewContext) -> Self {
        Self {
            context,
            running: false,
            agent: None,
            model: None,
            configured_agent: None,
            configured_model: None,
            model_names: BTreeMap::new(),
            awaiting_user: None,
            effort: None,
        }
    }

    /// Record why a mounted prompt is waiting on the user.
    ///
    /// Returns whether the answer changed, which is what a caller turns into a redraw.
    pub fn set_awaiting_user(&mut self, awaiting: Option<AwaitingUser>) -> bool {
        let changed = self.awaiting_user != awaiting;
        self.awaiting_user = awaiting;
        changed
    }

    /// Why a mounted prompt is waiting on the user.
    #[must_use]
    pub const fn awaiting_user(&self) -> Option<AwaitingUser> {
        self.awaiting_user
    }

    /// Install catalog display names keyed by exact model id.
    pub fn set_model_names(&mut self, names: impl IntoIterator<Item = (String, String)>) {
        self.model_names = names.into_iter().collect();
    }

    /// Adopt the configured identity before the first turn resolves.
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

    /// State the reasoning level, or `None` to show none at all.
    pub const fn set_effort(&mut self, effort: Option<zuno_llm::effort::ReasoningEffort>) {
        self.effort = effort;
    }

    /// The reasoning level the reply identity is showing.
    #[must_use]
    pub const fn effort(&self) -> Option<zuno_llm::effort::ReasoningEffort> {
        self.effort
    }

    /// Whether a turn is in flight, for the session's live footer.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// Whether there is an identity worth assigning a row.
    #[must_use]
    pub fn has_identity(&self) -> bool {
        self.agent
            .as_ref()
            .or(self.configured_agent.as_ref())
            .is_some()
            || self
                .model
                .as_ref()
                .or(self.configured_model.as_ref())
                .is_some()
    }

    /// Replace the palette and settings, for a live theme change.
    pub fn set_context(&mut self, context: ViewContext) {
        self.context = context;
    }

    /// Report a turn as running before the engine's first event arrives.
    pub fn mark_running(&mut self) {
        self.reset(true);
    }

    /// Discard the live resolution and retain the configured identity.
    fn reset(&mut self, running: bool) {
        self.running = running;
        self.agent = None;
        self.model = None;
        self.awaiting_user = None;
    }

    fn display_model<'a>(&'a self, model: &'a str) -> &'a str {
        self.model_names
            .get(model)
            .map(String::as_str)
            .unwrap_or_else(|| model.split_once('/').map_or(model, |(_, id)| id))
    }

    /// Compact current identity for a prompt-adjacent footer.
    ///
    /// The marker is intentionally neutral: routine identity is persistent metadata, not a
    /// warning or an active accent. The agent remains bold so Tab changes are immediately
    /// visible without relying on purple or green foregrounds.
    #[must_use]
    pub fn compact_spans(&self) -> Vec<Span<'static>> {
        let agent = self.agent.as_ref().or(self.configured_agent.as_ref());
        let model = self.model.as_ref().or(self.configured_model.as_ref());
        if agent.is_none() && model.is_none() {
            return Vec::new();
        }
        let mut spans = vec![Span::styled("▣ ".to_owned(), self.context.muted())];
        if let Some(agent) = agent {
            spans.push(Span::styled(agent.clone(), self.context.title()));
        }
        if let Some(model) = model {
            if agent.is_some() {
                spans.push(Span::styled(" · ".to_owned(), self.context.muted()));
            }
            spans.push(Span::styled(
                self.display_model(model).to_owned(),
                self.context.text(),
            ));
        }
        if let Some(effort) = self.effort {
            spans.push(Span::styled(
                format!(" ({})", effort.as_str()),
                self.context.muted(),
            ));
        }
        spans
    }

    /// The reply identity, styled as one compact OpenCode-like line.
    #[must_use]
    pub fn line(&self, width: u16) -> Line<'static> {
        let identity = self.compact_spans();
        if identity.is_empty() {
            return padded("", width, self.context.surface());
        }
        let mut spans = vec![Span::styled(" ".to_owned(), self.context.surface())];
        spans.extend(identity);
        let columns = usize::from(width);
        let mut spans = crate::views::markdown::truncate_row(spans, columns);
        let used = crate::views::markdown::row_width(&spans);
        if used < columns {
            spans.push(Span::styled(
                " ".repeat(columns - used),
                self.context.surface(),
            ));
        }
        Line::from(spans)
    }
}

impl Component for StatusView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        Paragraph::new(vec![self.line(area.width)])
            .style(self.context.surface())
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
            // Both the live field and configured fallback. The reply keeps identifying
            // the model and agent after completion without retaining transient run state.
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
            TurnEvent::TurnCompleted { .. }
            | TurnEvent::TurnWaitingForHuman { .. }
            | TurnEvent::TurnInterrupted { .. }
            | TurnEvent::TurnFailed { .. } => {
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

    fn thumb(&self, height: usize) -> Option<(usize, usize, usize, usize)> {
        if height == 0 || self.total <= self.viewport || self.total == 0 {
            return None;
        }
        let span = (self.viewport * height / self.total).max(1).min(height);
        let travel = height.saturating_sub(span);
        let scrollable = self.total.saturating_sub(self.viewport).max(1);
        let start = self.offset.min(scrollable) * travel / scrollable;
        Some((start, span, travel, scrollable))
    }

    /// Where inside the thumb a drag started.
    #[must_use]
    pub fn drag_anchor(&self, row: u16, area: Rect) -> usize {
        let local = usize::from(
            row.saturating_sub(area.top())
                .min(area.height.saturating_sub(1)),
        );
        let Some((start, span, _, _)) = self.thumb(usize::from(area.height)) else {
            return 0;
        };
        if local >= start && local < start.saturating_add(span) {
            local - start
        } else {
            span / 2
        }
    }

    /// Map a pointer row to a content offset while preserving `anchor` inside the thumb.
    #[must_use]
    pub fn offset_at(&self, row: u16, area: Rect, anchor: usize) -> usize {
        let local = usize::from(
            row.saturating_sub(area.top())
                .min(area.height.saturating_sub(1)),
        );
        let Some((_, _, travel, scrollable)) = self.thumb(usize::from(area.height)) else {
            return 0;
        };
        if travel == 0 {
            return 0;
        }
        local.saturating_sub(anchor).min(travel) * scrollable / travel
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
        let Some((start, span, _, _)) = self.thumb(height) else {
            return;
        };
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

/// Partition durable source across visual content rows without inventing separators.
///
/// The widths come from the rendered rows, but the returned chunks are byte-for-byte
/// slices of `source`. A soft terminal wrap therefore rejoins to the original space,
/// while a source newline remains a newline. Newlines count as one layout column here
/// because CommonMark renders an ordinary line ending as a space; they still remain the
/// original `\n` in the clipboard chunk.
fn partition_semantic_source(source: &str, rendered: &[String]) -> Vec<String> {
    if rendered.is_empty() {
        return Vec::new();
    }
    if rendered.len() == 1 {
        return vec![source.to_owned()];
    }
    let mut chunks = Vec::with_capacity(rendered.len());
    let mut offset = 0usize;
    for (index, row) in rendered.iter().enumerate() {
        if index + 1 == rendered.len() {
            chunks.push(source[offset..].to_owned());
            break;
        }
        let target = display_width(row);
        let mut used = 0usize;
        let mut end = offset;
        for (relative, character) in source[offset..].char_indices() {
            let cost = if character == '\n' {
                1
            } else {
                unicode_width::UnicodeWidthChar::width(character).unwrap_or(0)
            };
            if used >= target && cost > 0 {
                break;
            }
            if used.saturating_add(cost) > target && used > 0 {
                break;
            }
            used = used.saturating_add(cost);
            end = offset
                .saturating_add(relative)
                .saturating_add(character.len_utf8());
        }
        if end == offset && offset < source.len() {
            let character = source[offset..]
                .chars()
                .next()
                .expect("non-empty source tail has a first character");
            end = offset.saturating_add(character.len_utf8());
        }
        chunks.push(source[offset..end].to_owned());
        offset = end;
    }
    chunks.resize(rendered.len(), String::new());
    chunks
}

fn semantic_display_width(text: &str) -> usize {
    text.chars()
        .map(|character| {
            if character == '\n' {
                1
            } else {
                unicode_width::UnicodeWidthChar::width(character).unwrap_or(0)
            }
        })
        .sum()
}

fn slice_semantic_columns(text: &str, left: u16, right: u16) -> String {
    let left = usize::from(left);
    let right = usize::from(right);
    let mut column = 0usize;
    let mut out = String::new();
    let mut selected_previous = false;
    for character in text.chars() {
        let width = if character == '\n' {
            1
        } else {
            unicode_width::UnicodeWidthChar::width(character).unwrap_or(0)
        };
        if width == 0 {
            if selected_previous {
                out.push(character);
            }
            continue;
        }
        let end = column.saturating_add(width);
        selected_previous = column < right && end > left;
        if selected_previous {
            out.push(character);
        }
        column = end;
        if column >= right {
            break;
        }
    }
    out
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
