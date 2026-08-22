//! The delegated-task view: what this session handed to a subagent, and what came back.
//!
//! # What this can honestly show, and why it is not a job monitor
//!
//! The owner asked to *view running subagent tasks*. This build has no running ones to
//! view, and that is a runtime fact rather than a rendering gap:
//!
//! * `zuno_agent::continuation::JobBoard` models a running job — alias, agent, state,
//!   objective — and has **no production caller**. Nothing constructs it outside its own
//!   tests.
//! * `ChildSessionHost::dispatch` (`zuno-cli/src/cmd/child_turn.rs`) therefore *refuses*
//!   `background: true` outright, saying "nothing tracks a running subagent job or
//!   reports its completion, so a job id would name work you could never collect".
//! * A foreground delegation blocks the parent turn for its whole duration, and the
//!   child's own events are deliberately drained rather than forwarded, so no progress
//!   from inside a child is observable from here even while it runs.
//!
//! So a panel promising live job state would have nothing to put in it. What *does*
//! exist, completely, is the parent transcript's record of every delegation: a
//! [`crate::views::message::MessagePart::Tool`] named `task`, carrying the arguments the
//! model wrote, the dispatch status, and — on completion — the `<task …>` envelope
//! `zuno_tools::task::render` produced, whose `id` attribute is the child session's id.
//! This view is built over exactly that, so every row it shows is a delegation that
//! really happened.
//!
//! Its one honest limitation is stated on screen rather than hidden: the child's own
//! messages live in the child session's own transcript, which this view names by id so
//! the id can be opened, instead of pretending to contain it.
//!
//! # Why left and right move between tasks
//!
//! The binding table already ships `session_child_cycle` on `right` and
//! `session_child_cycle_reverse` on `left` (`keybind.ts` rows reproduced in
//! [`crate::keybind::DEFINITIONS`]), described as "Go to next/previous child session" —
//! which is what a delegation *is*. `session_child_first` on `<leader>down` — `ctrl+x`
//! then `down` — opens this view. Those three rows had no handler anywhere in the crate
//! before this view existed, so the keys the table advertised did nothing at all.
//!
//! [`crate::views::dialog::DialogHost`] already promotes the `session` scope while a
//! dialog is open, and `focused_scopes` are resolved *before* the static chain, so the
//! bare arrows reach this view only while it is open and go back to moving the prompt
//! cursor the moment it closes.

use crate::keybind::Definition;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::message::{Message, MessagePart, ToolStatus};
use crate::views::{ViewContext, truncate};
use crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

/// The dialog id [`DialogOutcome`] carries for this view.
pub const SUBAGENT_DIALOG_ID: &str = "session_child_first";

/// The tool whose calls are delegations.
///
/// The wire name, matching `zuno_tools::task::WIRE_ID`, because that is what the
/// transcript records. Spelled here rather than imported: `zuno-tui` does not depend on
/// `zuno-tools`, and a view that did would pull the whole tool runtime into the render
/// crate to learn one string. The pairing is asserted by this module's tests.
pub const TASK_TOOL: &str = "task";

/// Structured facts recovered from the task tool's stable output envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskEnvelope {
    /// Child session id, when the tool reached one.
    pub session_id: Option<String>,
    /// Child state reported by the envelope.
    pub state: Option<String>,
    /// The child report without the transport envelope.
    pub result: String,
}

/// What the view says when the session has delegated nothing.
///
/// A named answer rather than an empty body, for the reason
/// [`crate::views::diagnostics::EMPTY`] exists: a blank panel reads as one that failed to
/// load. This one also says what would fill it, because "no delegations" is a fact about
/// the conversation rather than a fault.
pub const EMPTY: &str = "no delegated tasks yet";

/// What the view says about where a child's own messages are.
pub const CHILD_TRANSCRIPT_NOTE: &str = "the subagent's own messages are in that session";

/// One delegation, projected from the parent transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    /// The provider's call id, which is this row's stable identity.
    pub call_id: String,
    /// The subagent asked for, when the model named one.
    pub agent: Option<String>,
    /// The delegation's description, or its prompt when it had no description.
    pub objective: Option<String>,
    /// How far the dispatch got.
    pub status: ToolStatus,
    /// The child session's id, parsed from the completed envelope.
    pub session_id: Option<String>,
    /// The state the envelope reported.
    pub state: Option<String>,
    /// Whether the model asked for this to run in the background.
    ///
    /// Kept because a refused background request is the one case where a row exists and
    /// no child session does, and the refusal is worth reading rather than looking like a
    /// delegation that vanished.
    pub background: bool,
}

impl Delegation {
    /// The row's headline, at most `width` columns.
    #[must_use]
    pub fn headline(&self, width: usize) -> String {
        let agent = self.agent.as_deref().unwrap_or("subagent");
        let objective = self.objective.as_deref().unwrap_or("(no description)");
        truncate(
            &format!("{} {agent}: {objective}", self.status.glyph()),
            width,
        )
    }
}

/// Every delegation in `messages`, in the order the model made them.
///
/// Reads the transcript already in memory rather than querying the session database.
/// Both would work and they would answer differently: the database's child sessions
/// include delegations from *earlier* runs of this session, while a view opened to see
/// "what did this conversation hand off" means the ones on screen. The transcript is
/// also the only source that has a row for a delegation that failed, because a failed
/// dispatch creates no child session to find.
#[must_use]
pub fn delegations(messages: &[Message]) -> Vec<Delegation> {
    let mut found = Vec::new();
    for message in messages {
        for part in &message.parts {
            let MessagePart::Tool {
                call_id,
                name,
                arguments,
                status,
                output,
                ..
            } = part
            else {
                continue;
            };
            if name != TASK_TOOL {
                continue;
            }
            let parsed = serde_json::from_str::<serde_json::Value>(arguments).ok();
            let field = |key: &str| -> Option<String> {
                parsed
                    .as_ref()?
                    .get(key)?
                    .as_str()
                    .map(str::to_owned)
                    .filter(|value| !value.is_empty())
            };
            let envelope = output.as_deref().and_then(task_envelope);
            found.push(Delegation {
                call_id: call_id.clone(),
                agent: field("subagent_type").or_else(|| field("category")),
                objective: field("description").or_else(|| field("prompt")),
                status: *status,
                session_id: envelope.as_ref().and_then(|found| found.session_id.clone()),
                state: envelope.and_then(|found| found.state),
                background: parsed
                    .as_ref()
                    .and_then(|value| value.get("background"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    found
}

/// Parse the stable `<task …>` envelope emitted by `zuno_tools::task`.
///
/// A scan for two quoted attributes rather than an XML parse: the envelope is produced by
/// one function (`zuno_tools::task::render`) whose shape is `<task id="…" state="…">`, and
/// pulling in a parser to read two attributes off a line this crate does not own would be
/// a larger commitment than the fact deserves. Anything unrecognised yields [`None`],
/// which renders as a row without a session id instead of a wrong one.
#[must_use]
pub fn task_envelope(output: &str) -> Option<TaskEnvelope> {
    let tag = output.lines().find(|line| line.starts_with("<task "))?;
    let result = output
        .split_once("<task_result>")
        .and_then(|(_, rest)| rest.split_once("</task_result>"))
        .map_or("", |(result, _)| result)
        .trim_matches('\n')
        .to_owned();
    Some(TaskEnvelope {
        session_id: attribute(tag, "id"),
        state: attribute(tag, "state"),
        result,
    })
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned())
}

/// The delegated-task view: a cursor over [`Delegation`]s with a detail body.
pub struct SubagentView {
    context: ViewContext,
    tasks: Vec<Delegation>,
    cursor: usize,
}

impl SubagentView {
    /// A view over the delegations the host projected from the transcript.
    #[must_use]
    pub fn new(context: ViewContext, tasks: Vec<Delegation>) -> Self {
        Self {
            context,
            tasks,
            cursor: 0,
        }
    }

    /// How many delegations the view is showing.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the session has delegated nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Which delegation the cursor is on.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The delegation under the cursor.
    #[must_use]
    pub fn selected(&self) -> Option<&Delegation> {
        self.tasks.get(self.cursor)
    }

    /// Move `step` places along the list, wrapping at both ends.
    ///
    /// `rem_euclid` over the signed sum, as the agent and effort cycles do, so one
    /// expression serves both directions including the wrap backwards off the first row.
    /// An empty list moves nowhere rather than panicking on a remainder by zero.
    fn step(&mut self, step: isize) -> DialogStep {
        if self.tasks.len() < 2 {
            // Still a redraw: with one task the keys are legitimately inert, and the
            // footer says `1/1`, which is the honest report. Returning `Ignored` would
            // let the arrow fall through to a scope below and move something else.
            return DialogStep::Redraw;
        }
        let length = isize::try_from(self.tasks.len()).unwrap_or(isize::MAX);
        let moved = isize::try_from(self.cursor)
            .unwrap_or(0)
            .saturating_add(step);
        self.cursor = usize::try_from(moved.rem_euclid(length)).unwrap_or(0);
        DialogStep::Redraw
    }

    fn detail(&self, width: usize) -> Vec<Line<'static>> {
        let Some(task) = self.selected() else {
            return vec![
                Line::from(Span::styled(EMPTY.to_owned(), self.context.muted())),
                Line::from(Span::styled(
                    format!("delegations appear here once the model uses `{TASK_TOOL}`"),
                    self.context.muted(),
                )),
            ];
        };
        let mut lines = vec![Line::from(Span::styled(
            task.headline(width),
            match task.status {
                ToolStatus::Completed => self.context.success(),
                ToolStatus::Error => self.context.error(),
                ToolStatus::Running | ToolStatus::Pending => self.context.warning(),
            },
        ))];
        let mut row = |label: &str, value: String| {
            lines.push(Line::from(Span::styled(
                truncate(&format!("  {label} {value}"), width),
                self.context.muted(),
            )));
        };
        if let Some(state) = &task.state {
            row("state", state.clone());
        }
        match &task.session_id {
            Some(id) => {
                row("session", id.clone());
                row("note", CHILD_TRANSCRIPT_NOTE.to_owned());
            }
            None if task.status == ToolStatus::Error => {
                // Named rather than left blank: a refused background delegation is the
                // common way to arrive here, and "no session" alone reads as data loss.
                row("session", String::from("none — the delegation did not run"));
            }
            None => row("session", String::from("not reported yet")),
        }
        if task.background {
            row(
                "background",
                String::from("requested; this build runs delegations in the foreground"),
            );
        }
        lines
    }
}

impl Dialog for SubagentView {
    fn id(&self) -> &'static str {
        SUBAGENT_DIALOG_ID
    }

    fn title(&self) -> String {
        if self.tasks.is_empty() {
            return String::from("Delegated tasks");
        }
        format!(
            "Delegated tasks  {}/{}",
            self.cursor.saturating_add(1),
            self.tasks.len()
        )
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        // `saturating_sub`, and never a `clamp` with a computed minimum: `u16::clamp`
        // panics when min exceeds max, which is exactly what a 20-column frame produces.
        let body = usize::from(width.saturating_sub(2)).max(1);
        let mut lines = Vec::new();
        for (index, task) in self.tasks.iter().enumerate() {
            let marker = if index == self.cursor { "›" } else { " " };
            lines.push(Line::from(Span::styled(
                truncate(&format!("{marker} {}", task.headline(body)), body),
                if index == self.cursor {
                    self.context.element()
                } else {
                    self.context.muted()
                },
            )));
        }
        if !self.tasks.is_empty() {
            lines.push(Line::from(Span::raw(String::new())));
        }
        lines.extend(self.detail(body));
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("←→", "task"), ("esc", "close")]
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::Large
    }

    /// The `session` scope, so the bare arrows reach this view while it is open.
    ///
    /// Named explicitly even though [`crate::views::dialog::DialogHost`] appends it for
    /// every dialog: this view is the only one whose *navigation* depends on it, and a
    /// later change to the host's list would otherwise silently take the arrows away.
    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["session"]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        match action.name {
            "session_child_cycle" | "dialog.select.next" => self.step(1),
            "session_child_cycle_reverse" | "dialog.select.prev" => self.step(-1),
            // `session_child_first` is the key that opened this view; pressing it again
            // returns to the first row rather than reopening or closing, which is what
            // "first child" means with the view already up.
            "session_child_first" | "dialog.select.home" => {
                self.cursor = 0;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.tasks.len().saturating_sub(1);
                DialogStep::Redraw
            }
            // Up is `session_parent`: from a child back to the parent conversation, which
            // from here is the transcript behind this view.
            "session_parent" | "session_interrupt" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
        }
    }
}
