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
use crate::views::dialog::{Dialog, DialogOutcome, DialogPlacement, DialogStep};
use crate::views::{ViewContext, padded};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
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
    /// Highlighted row per question. `options.len()` is the custom-answer row.
    cursors: Vec<usize>,
    /// Selected labels per question.
    answers: Vec<Vec<String>>,
    /// Typed custom-answer draft per question.
    drafts: Vec<String>,
    /// Whether each question's custom-answer row is being edited.
    editing: Vec<bool>,
}

impl QuestionPrompt {
    /// A prompt over `questions`.
    #[must_use]
    pub fn new(context: ViewContext, questions: Vec<QuestionRequest>) -> Self {
        let count = questions.len();
        Self {
            context,
            questions,
            current: 0,
            cursors: vec![0; count],
            answers: vec![Vec::new(); count],
            drafts: vec![String::new(); count],
            editing: vec![false; count],
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
    pub fn cursor(&self) -> usize {
        self.cursors.get(self.current).copied().unwrap_or_default()
    }

    /// Whether the typed-answer row is being edited.
    #[must_use]
    pub fn is_editing(&self) -> bool {
        self.editing.get(self.current).copied().unwrap_or(false)
    }

    fn question(&self) -> &QuestionRequest {
        &self.questions[self.current]
    }

    fn draft(&self) -> &str {
        self.drafts
            .get(self.current)
            .map(String::as_str)
            .unwrap_or_default()
    }

    fn set_cursor(&mut self, cursor: usize) {
        if let Some(current) = self.cursors.get_mut(self.current) {
            *current = cursor;
        }
    }

    fn rows(&self) -> usize {
        let question = self.question();
        question.options.len() + usize::from(question.allows_custom())
    }

    fn is_custom_row(&self) -> bool {
        self.question().allows_custom() && self.cursor() == self.question().options.len()
    }

    fn toggle(&mut self) {
        let Some(option) = self.question().options.get(self.cursor()) else {
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
            return DialogStep::Redraw;
        }
        DialogStep::Resolved(DialogOutcome::Question(std::mem::take(&mut self.answers)))
    }

    fn submit(&mut self) -> DialogStep {
        if self.is_custom_row() {
            let typed = self.draft().trim().to_owned();
            if !typed.is_empty() {
                self.answers[self.current] = vec![typed];
            }
            return self.advance();
        }
        if !self.question().is_multiple() {
            self.answers[self.current] = self
                .question()
                .options
                .get(self.cursor())
                .map(|option| vec![option.label.clone()])
                .unwrap_or_default();
        }
        self.advance()
    }

    fn move_cursor(&mut self, next: bool) -> DialogStep {
        let rows = self.rows();
        if rows == 0 {
            return DialogStep::Ignored;
        }
        let cursor = self.cursor();
        self.set_cursor(if next {
            (cursor + 1) % rows
        } else {
            (cursor + rows - 1) % rows
        });
        DialogStep::Redraw
    }

    fn move_question(&mut self, next: bool) -> DialogStep {
        let count = self.questions.len();
        if count <= 1 {
            return DialogStep::Ignored;
        }
        self.current = if next {
            (self.current + 1) % count
        } else {
            (self.current + count - 1) % count
        };
        DialogStep::Redraw
    }

    fn choose_digit(&mut self, character: char) -> DialogStep {
        let Some(digit) = character.to_digit(10) else {
            return DialogStep::Ignored;
        };
        if digit == 0 {
            return DialogStep::Ignored;
        }
        let choice = usize::try_from(digit - 1).unwrap_or_default();
        if choice >= self.rows() {
            return DialogStep::Ignored;
        }
        self.set_cursor(choice);
        if self.is_custom_row() {
            self.editing[self.current] = true;
            return DialogStep::Redraw;
        }
        if self.question().is_multiple() {
            self.toggle();
            return DialogStep::Redraw;
        }
        self.submit()
    }

    fn option_prefix(&self, index: usize) -> String {
        let marker = if index == self.cursor() { '›' } else { ' ' };
        format!(" {marker} {}. ", index + 1)
    }

    fn description_rows(text: &str, width: u16, indent: usize) -> Vec<String> {
        if text.is_empty() {
            return Vec::new();
        }
        crate::views::message::wrap(
            text,
            width
                .saturating_sub(u16::try_from(indent).unwrap_or(u16::MAX))
                .max(1),
        )
    }

    fn custom_input_rows(&self, width: u16) -> Vec<String> {
        if !self.is_editing() && self.draft().is_empty() {
            return Vec::new();
        }
        let mut rows = crate::views::message::wrap(self.draft(), width.saturating_sub(2).max(1));
        if rows.is_empty() {
            rows.push(String::new());
        }
        if self.is_editing()
            && let Some(last) = rows.last_mut()
        {
            last.push('▏');
        }
        rows
    }

    fn choice_at_line(&self, width: u16, target: usize) -> Option<usize> {
        let question = self.question();
        let mut line =
            crate::views::message::wrap(&question.question, width.saturating_sub(2)).len() + 1;
        for (index, option) in question.options.iter().enumerate() {
            let prefix = self.option_prefix(index);
            let description = Self::description_rows(
                &option.description,
                width,
                crate::views::display_width(&prefix),
            );
            let end = line.saturating_add(1 + description.len());
            if (line..end).contains(&target) {
                return Some(index);
            }
            line = end;
        }
        if question.allows_custom() {
            let index = question.options.len();
            let prefix = self.option_prefix(index);
            let description = Self::description_rows(
                "Type a custom answer",
                width,
                crate::views::display_width(&prefix),
            );
            let custom_rows = 1 + description.len() + self.custom_input_rows(width).len();
            if (line..line.saturating_add(custom_rows)).contains(&target) {
                return Some(question.options.len());
            }
        }
        None
    }
}

impl Dialog for QuestionPrompt {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        let question = self.question();
        let unanswered = self
            .answers
            .iter()
            .filter(|answer| answer.is_empty())
            .count();
        let progress = if unanswered == 0 {
            format!("Question {}/{}", self.current + 1, self.questions.len())
        } else {
            format!(
                "Question {}/{} ({unanswered} unanswered)",
                self.current + 1,
                self.questions.len()
            )
        };
        format!("{progress} · {}", question.header)
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let question = &self.questions[self.current];
        let selected = &self.answers[self.current];
        let mut lines = Vec::new();
        let question_style = if selected.is_empty() {
            self.context.accent()
        } else {
            self.context.text()
        };
        for row in crate::views::message::wrap(&question.question, width.saturating_sub(2)) {
            lines.push(padded(&format!(" {row}"), width, question_style));
        }
        lines.push(padded("", width, self.context.surface()));
        for (index, option) in question.options.iter().enumerate() {
            let prefix = self.option_prefix(index);
            let label = if question.is_multiple() {
                let check = if selected.contains(&option.label) {
                    "[x]"
                } else {
                    "[ ]"
                };
                format!("{check} {}", option.label)
            } else {
                option.label.clone()
            };
            let marker_style = if index == self.cursor() {
                self.context.accent()
            } else {
                self.context.muted()
            };
            let label_style = if index == self.cursor() {
                self.context.title()
            } else if selected.contains(&option.label) {
                self.context.accent()
            } else {
                self.context.text()
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.clone(), marker_style),
                Span::styled(label, label_style),
            ]));
            let indent = crate::views::display_width(&prefix);
            for row in Self::description_rows(&option.description, width, indent) {
                lines.push(padded(
                    &format!("{}{row}", " ".repeat(indent)),
                    width,
                    self.context.muted(),
                ));
            }
        }
        if question.allows_custom() {
            let index = question.options.len();
            let prefix = self.option_prefix(index);
            let marker_style = if self.is_custom_row() {
                self.context.accent()
            } else {
                self.context.muted()
            };
            let label_style = if self.is_custom_row() {
                self.context.title()
            } else {
                self.context.text()
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.clone(), marker_style),
                Span::styled("Other", label_style),
            ]));
            let indent = crate::views::display_width(&prefix);
            for row in Self::description_rows("Type a custom answer", width, indent) {
                lines.push(padded(
                    &format!("{}{row}", " ".repeat(indent)),
                    width,
                    self.context.muted(),
                ));
            }
            for row in self.custom_input_rows(width) {
                lines.push(padded(&format!(" {row}"), width, self.context.element()));
            }
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.is_editing() {
            return vec![
                ("shift+enter", "newline"),
                ("enter", "submit"),
                ("esc", "cancel"),
            ];
        }
        let mut hints = vec![("enter", "confirm"), ("1-9", "choose"), ("↑↓/jk", "move")];
        if self.question().is_multiple() {
            hints.insert(1, ("space", "toggle"));
        }
        if self.questions.len() > 1 {
            hints.push(("←→", "question"));
        }
        hints.push(("esc", "cancel"));
        hints
    }

    fn placement(&self) -> DialogPlacement {
        DialogPlacement::Composer
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["dialog.question"]
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        if !self.is_editing() {
            return match key.code {
                KeyCode::Char(character) => self.choose_digit(character),
                _ => DialogStep::Ignored,
            };
        }
        if let Some(character) = crate::views::permission::typed_character(key) {
            self.drafts[self.current].push(character);
            return DialogStep::Redraw;
        }
        DialogStep::Ignored
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        if self.is_editing() {
            match action.name {
                "dialog.prompt.submit" | "dialog.select.submit" => return self.submit(),
                "input_newline" => {
                    self.drafts[self.current].push('\n');
                    return DialogStep::Redraw;
                }
                "input_backspace" => {
                    self.drafts[self.current].pop();
                    return DialogStep::Redraw;
                }
                "app_exit" | "session_interrupt" => {
                    return DialogStep::Resolved(DialogOutcome::Cancelled);
                }
                _ => return self.handle_typed(event),
            }
        }
        match action.name {
            "dialog.select.prev" | "dialog.question.prev_option" => self.move_cursor(false),
            "dialog.select.next" | "dialog.question.next_option" => self.move_cursor(true),
            "dialog.question.prev_question" => self.move_question(false),
            "dialog.question.next_question" => self.move_question(true),
            "dialog.select.home" => {
                if self.rows() == 0 {
                    return DialogStep::Ignored;
                }
                self.set_cursor(0);
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                let rows = self.rows();
                if rows == 0 {
                    return DialogStep::Ignored;
                }
                self.set_cursor(rows - 1);
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
                if self.is_custom_row() && !self.is_editing() && self.draft().is_empty() {
                    self.editing[self.current] = true;
                    return DialogStep::Redraw;
                }
                self.submit()
            }
            "app_exit" | "session_interrupt" => DialogStep::Resolved(DialogOutcome::Cancelled),
            _ => {
                if let KeyCode::Char(' ') = event.code
                    && self.question().is_multiple()
                    && !self.is_custom_row()
                {
                    self.toggle();
                    return DialogStep::Redraw;
                }
                match event.code {
                    KeyCode::Char(character) => self.choose_digit(character),
                    _ => DialogStep::Ignored,
                }
            }
        }
    }

    fn handle_mouse(&mut self, event: &MouseEvent, body: Rect) -> DialogStep {
        if event.column < body.left()
            || event.column >= body.right()
            || event.row < body.top()
            || event.row >= body.bottom()
        {
            return DialogStep::Ignored;
        }
        match event.kind {
            MouseEventKind::ScrollUp if !self.is_editing() => {
                let rows = self.rows();
                if rows == 0 {
                    return DialogStep::Ignored;
                }
                self.set_cursor((self.cursor() + rows - 1) % rows);
                return DialogStep::Redraw;
            }
            MouseEventKind::ScrollDown if !self.is_editing() => {
                let rows = self.rows();
                if rows == 0 {
                    return DialogStep::Ignored;
                }
                self.set_cursor((self.cursor() + 1) % rows);
                return DialogStep::Redraw;
            }
            MouseEventKind::Up(MouseButton::Left) => {}
            _ => return DialogStep::Ignored,
        }
        let target = usize::from(event.row.saturating_sub(body.top()));
        let Some(choice) = self.choice_at_line(body.width, target) else {
            return DialogStep::Ignored;
        };
        self.set_cursor(choice);
        if self.is_custom_row() {
            self.editing[self.current] = true;
            return DialogStep::Redraw;
        }
        if self.question().is_multiple() {
            self.toggle();
            DialogStep::Redraw
        } else {
            self.submit()
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
