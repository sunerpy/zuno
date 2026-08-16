//! The question prompt: the surface behind the `question` tool.
//!
//! # The shape is todo 43's, deliberately duplicated rather than imported
//!
//! `zuno-tools`'s `QuestionRequest` — `question`, `header`, `options: [{label,
//! description}]`, `multiple`, `custom`
//! (`packages/schema/src/v1/question.ts:16-30`) — is the contract. This module
//! defines a field-identical type instead of depending on `zuno-tools`, because a
//! prompt renderer has no business pulling in the tool *implementations*
//! (`zuno-agent`, `zuno-catalog`, `zuno-memory`, `zuno-search` all arrive with that crate),
//! and because upstream's own boundary is a **serialized** request that travels over
//! the event bus to a client (`packages/core/src/question.ts`).
//!
//! The duplication is therefore checked rather than trusted:
//! [`QuestionRequest`] deserializes the oracle's JSON, and a test parses that exact
//! document. A field renamed on either side breaks it.
//!
//! # `custom` decides whether a typed answer exists
//!
//! The model-facing `Prompt` has no `custom` field, so a model-written question
//! always allows a typed answer; only an internal caller — `plan_exit` — can switch
//! it off. Absent means allowed, which is why the field is an `Option<bool>` read as
//! "the client's default, which is on" rather than a `bool` defaulting to `false`.
//!
//! # `multiple` changes the affordance, not the answer type
//!
//! An answer is a list of labels either way (`v1/question.ts:41`), so a
//! single-select question answers with a one-element list. That is what keeps
//! `answers[i]` positional and lets an empty list mean "unanswered" rather than
//! needing a separate sentinel.

use crate::keybind::Definition;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep};
use crate::views::{ViewContext, padded};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "question_tests.rs"]
mod tests;

/// The dialog id [`DialogOutcome`] carries for this prompt.
pub const DIALOG_ID: &str = "question";

/// What an unanswered question renders as (`question.ts:31`).
pub const UNANSWERED: &str = "Unanswered";

/// One selectable answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuestionOption {
    /// Display text.
    pub label: String,
    /// Why a user would pick it.
    pub description: String,
}

impl QuestionOption {
    /// An option.
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }
}

/// One question, in the shape the asking layer sends.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuestionRequest {
    /// The complete question.
    pub question: String,
    /// A very short label shown beside it.
    pub header: String,
    /// The offered choices.
    pub options: Vec<QuestionOption>,
    /// Whether more than one choice may be selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// Whether a typed answer is offered. Absent means yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

impl QuestionRequest {
    /// A single-select question that also allows a typed answer.
    #[must_use]
    pub fn new(
        question: impl Into<String>,
        header: impl Into<String>,
        options: Vec<QuestionOption>,
    ) -> Self {
        Self {
            question: question.into(),
            header: header.into(),
            options,
            multiple: None,
            custom: None,
        }
    }

    /// Whether more than one option may be chosen.
    #[must_use]
    pub fn is_multiple(&self) -> bool {
        self.multiple.unwrap_or(false)
    }

    /// Whether a typed answer is offered.
    #[must_use]
    pub fn allows_custom(&self) -> bool {
        self.custom.unwrap_or(true)
    }
}

/// The prompt for one or more questions, asked in order.
///
/// A list rather than one question per dialog, because the tool takes a list and
/// answers positionally: splitting them into separate dialogs would let a user
/// answer question three while question one is still open.
pub struct QuestionPrompt {
    context: ViewContext,
    questions: Vec<QuestionRequest>,
    /// Which question is showing.
    current: usize,
    /// Highlighted option within the current question. `options.len()` is the
    /// "type your own" row when the question allows one.
    cursor: usize,
    /// Selected labels per question.
    answers: Vec<Vec<String>>,
    /// Typed text for the current question.
    typed: String,
    /// Whether the typed row is being edited.
    editing: bool,
}

impl QuestionPrompt {
    /// A prompt over `questions`.
    #[must_use]
    pub fn new(context: ViewContext, questions: Vec<QuestionRequest>) -> Self {
        let answers = vec![Vec::new(); questions.len()];
        Self {
            context,
            questions,
            current: 0,
            cursor: 0,
            answers,
            typed: String::new(),
            editing: false,
        }
    }

    /// The answers collected so far, positional with the questions.
    #[must_use]
    pub fn answers(&self) -> &[Vec<String>] {
        &self.answers
    }

    /// Which question is showing.
    #[must_use]
    pub const fn current(&self) -> usize {
        self.current
    }

    /// The highlighted row.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the typed-answer row is being edited.
    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.editing
    }

    fn question(&self) -> &QuestionRequest {
        &self.questions[self.current]
    }

    fn rows(&self) -> usize {
        let question = self.question();
        question.options.len() + usize::from(question.allows_custom())
    }

    fn is_custom_row(&self) -> bool {
        self.question().allows_custom() && self.cursor == self.question().options.len()
    }

    fn toggle(&mut self) {
        let Some(option) = self.question().options.get(self.cursor) else {
            return;
        };
        let label = option.label.clone();
        let answers = &mut self.answers[self.current];
        if let Some(index) = answers.iter().position(|held| *held == label) {
            answers.remove(index);
        } else {
            answers.push(label);
        }
    }

    /// Record the current question's answer and move on, resolving after the last.
    fn advance(&mut self) -> DialogStep {
        if self.current + 1 < self.questions.len() {
            self.current += 1;
            self.cursor = 0;
            self.typed.clear();
            self.editing = false;
            return DialogStep::Redraw;
        }
        DialogStep::Resolved(DialogOutcome::Question(std::mem::take(&mut self.answers)))
    }

    fn submit(&mut self) -> DialogStep {
        if self.is_custom_row() {
            let typed = self.typed.trim();
            if !typed.is_empty() {
                self.answers[self.current] = vec![typed.to_owned()];
            }
            return self.advance();
        }
        if !self.question().is_multiple() {
            self.answers[self.current] = self
                .question()
                .options
                .get(self.cursor)
                .map(|option| vec![option.label.clone()])
                .unwrap_or_default();
        }
        self.advance()
    }
}

impl Dialog for QuestionPrompt {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        let question = self.question();
        if self.questions.len() > 1 {
            format!(
                "{} ({}/{})",
                question.header,
                self.current + 1,
                self.questions.len()
            )
        } else {
            question.header.clone()
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let question = &self.questions[self.current];
        let selected = &self.answers[self.current];
        let mut lines = Vec::new();
        for row in crate::views::message::wrap(&question.question, width.saturating_sub(2)) {
            lines.push(padded(&format!(" {row}"), width, self.context.text()));
        }
        lines.push(padded("", width, self.context.surface()));
        for (index, option) in question.options.iter().enumerate() {
            let style = if index == self.cursor {
                self.context.selected()
            } else {
                self.context.text()
            };
            // A checkbox only for a multi-select question: showing one for a
            // single-select question implies several answers are possible.
            let marker = if question.is_multiple() {
                if selected.contains(&option.label) {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else if index == self.cursor {
                "> "
            } else {
                "  "
            };
            lines.push(padded(&format!(" {marker}{}", option.label), width, style));
            if !option.description.is_empty() {
                lines.push(padded(
                    &format!("     {}", option.description),
                    width,
                    self.context.muted(),
                ));
            }
        }
        if question.allows_custom() {
            let style = if self.is_custom_row() {
                self.context.selected()
            } else {
                self.context.muted()
            };
            let body = if self.editing || !self.typed.is_empty() {
                format!(" > {}▏", self.typed)
            } else {
                String::from(" > type your own answer")
            };
            lines.push(padded(&body, width, style));
        }
        if selected.is_empty() && !self.editing {
            lines.push(Line::from(Span::styled(
                format!(" {UNANSWERED}"),
                self.context.muted(),
            )));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.question().is_multiple() {
            vec![
                ("↑↓", "move"),
                ("space", "toggle"),
                ("enter", "confirm"),
                ("esc", "skip"),
            ]
        } else {
            vec![("↑↓", "move"), ("enter", "confirm"), ("esc", "skip")]
        }
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        if !self.editing {
            return DialogStep::Ignored;
        }
        if let Some(character) = crate::views::permission::typed_character(key) {
            self.typed.push(character);
            return DialogStep::Redraw;
        }
        DialogStep::Ignored
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        if self.editing {
            match action.name {
                "dialog.prompt.submit" | "dialog.select.submit" => return self.submit(),
                "input_backspace" => {
                    self.typed.pop();
                    return DialogStep::Redraw;
                }
                "app_exit" => {
                    self.editing = false;
                    self.typed.clear();
                    return DialogStep::Redraw;
                }
                _ => return self.handle_typed(event),
            }
        }
        match action.name {
            "dialog.select.prev" => {
                let rows = self.rows();
                self.cursor = (self.cursor + rows - 1) % rows;
                DialogStep::Redraw
            }
            "dialog.select.next" => {
                self.cursor = (self.cursor + 1) % self.rows();
                DialogStep::Redraw
            }
            "dialog.select.home" => {
                self.cursor = 0;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.rows() - 1;
                DialogStep::Redraw
            }
            "dialog.mcp.toggle" => {
                // The table's only `space` binding. A multi-select question needs
                // one, and adding a second `space` row would be a keybind conflict
                // the table rejects at construction.
                if self.question().is_multiple() && !self.is_custom_row() {
                    self.toggle();
                    return DialogStep::Redraw;
                }
                DialogStep::Ignored
            }
            "dialog.select.submit" | "dialog.prompt.submit" => {
                if self.is_custom_row() && !self.editing && self.typed.is_empty() {
                    self.editing = true;
                    return DialogStep::Redraw;
                }
                self.submit()
            }
            // Skipping leaves the answer empty, which the tool renders as
            // `Unanswered` rather than treating as a rejection.
            "app_exit" => {
                DialogStep::Resolved(DialogOutcome::Question(std::mem::take(&mut self.answers)))
            }
            _ => {
                if let KeyCode::Char(' ') = event.code
                    && self.question().is_multiple()
                    && !self.is_custom_row()
                {
                    self.toggle();
                    return DialogStep::Redraw;
                }
                DialogStep::Ignored
            }
        }
    }
}

/// Render answers the way the tool reports them back to the model.
///
/// `zuno-tools`'s `format_answers` is the authority for the transcript; this is the
/// same rule for the *view's* own confirmation line, and it exists so an empty
/// answer is visibly `Unanswered` rather than a blank.
#[must_use]
pub fn render_answer(answer: &[String]) -> String {
    if answer.is_empty() {
        UNANSWERED.to_owned()
    } else {
        answer.join(", ")
    }
}
