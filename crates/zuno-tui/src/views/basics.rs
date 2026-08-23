//! The dialog base layer: a yes/no, an acknowledgement, and a text prompt.
//!
//! `§6.1` of the plan lists these three alongside [`crate::views::toast::Toast`] as the
//! generic shapes every specific surface is built out of. What existed before was the
//! specific set — pickers, the permission prompt, the question prompt, the MCP panel,
//! help, diff — each with its own hand-rolled contract. These three are the shapes with
//! no domain in them.
//!
//! # All three are `medium`
//!
//! `§11.4` puts Confirm, Alert and Prompt in the 60-column tier, and none of them
//! overrides [`Dialog::width`]. A confirmation is one question and two words; giving it
//! 116 columns would draw a sparse band across a wide terminal, which is the exact
//! failure the fixed tiers exist to prevent.
//!
//! # Left/right versus up/down
//!
//! `§6.1` describes the confirmation's buttons as switching left and right. The binding
//! table has no `left`/`right` dialog row, and it cannot grow one: `DEFINITIONS` is
//! asserted row-for-row against upstream's own fixture, so adding a row would make this
//! build claim upstream ships a key it does not. So the two buttons move on
//! `dialog.select.prev`/`dialog.select.next` — the same two actions the permission
//! prompt's "always allow" page already uses for its own Confirm/Cancel pair, which
//! means the whole TUI has one answer for "how do I move between two buttons" rather
//! than two. The footer advertises what the table actually resolves.
//!
//! # `Enter` and `Esc`, stated
//!
//! | form | `Enter` | `Esc` |
//! | --- | --- | --- |
//! | [`ConfirmDialog`] | the focused button: [`CONFIRM_VALUE`] when Confirm has focus (the default), otherwise cancel | cancel |
//! | [`AlertDialog`] | close | close |
//! | [`PromptDialog`] | submit the typed text, even when empty | cancel, discarding the text |
//!
//! Cancel is [`DialogOutcome::Cancelled`] in every case. An alert reports the same
//! outcome for both keys because it has one button and therefore nothing to report;
//! inventing an `Acknowledged` variant would give a caller a distinction it cannot act
//! on.
//!
//! A prompt submitting empty text is deliberate. Clearing a value is a legitimate answer
//! and the caller — not this dialog — knows whether an empty string means anything, the
//! same division [`crate::views::picker::SelectDialog`] keeps by resolving an opaque
//! value it does not interpret.

use crate::keybind::Definition;
use crate::views::dialog::{BodyAnchor, Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::{ViewContext, display_width, padded, truncate};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;

#[cfg(test)]
#[path = "basics_tests.rs"]
mod tests;

/// The value a [`ConfirmDialog`] resolves with when the user confirmed.
///
/// A constant rather than each caller spelling `"confirm"`: the producer and the
/// consumer are in different files, and a typo in either is a confirmation that
/// silently does nothing — the same class of dead branch as an action name that is not
/// in the binding table.
pub const CONFIRM_VALUE: &str = "confirm";

/// Wrap `text` into rows of at most `width` columns.
///
/// Columns, not characters, and it is the same reason `§11.5` gives: a row measured by
/// `chars().count()` overflows by one cell per wide glyph, and the overflow wraps and
/// shifts every row under it. [`truncate`] is what actually cuts, so a wide glyph is
/// never split in half.
///
/// # The one row that can exceed `width`
///
/// A glyph wider than the whole row — a CJK character in a one-column body, reachable on
/// a terminal about nine columns wide — gets a row of its own and that row is two columns
/// wide. Both alternatives are worse: dropping the glyph loses text silently, which is
/// the "truncation kept the wrong half" defect this project has already been bitten by,
/// and looping to make it fit does not terminate. Nothing overflows on screen, because
/// every caller passes each row through [`padded`], which clips to the frame.
fn wrap(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut rest = paragraph;
        loop {
            if display_width(rest) <= usize::from(width) {
                rows.push(rest.to_owned());
                break;
            }
            let head = truncate(rest, usize::from(width));
            // A zero-length head would loop forever, which is reachable when the width is
            // one column and the next glyph is two cells wide.
            let hard = if head.is_empty() {
                rest.chars().next().map_or(0, char::len_utf8)
            } else {
                head.len()
            };
            // Break at the last space inside the row rather than mid-word. Reading the
            // rendered frame is what earned this: a hard wrap produced `discar` / `ded`
            // and `os err` / `or 2`, which every width assertion passed because the row
            // was the right *length* — the same class as the truncation that kept a read
            // window and dropped the filename. The hard break is kept as the fallback,
            // since a single word longer than the row has to break somewhere.
            let taken = head
                .rfind(' ')
                .filter(|position| *position > 0)
                .map_or(hard, |position| position + 1);
            rows.push(rest[..taken].trim_end().to_owned());
            rest = rest[taken..].trim_start();
        }
    }
    rows
}

/// A yes/no question.
///
/// Confirm has focus on open, because every caller here is guarding an action the user
/// just asked for: they typed `/undo`, and the dialog exists to let them notice what it
/// costs, not to make them ask twice. Cancel is one key away either way.
pub struct ConfirmDialog {
    context: ViewContext,
    id: &'static str,
    heading: String,
    body: String,
    confirm_label: String,
    cancel_label: String,
    /// Whether Confirm has focus. `true` on open; see the type docs.
    confirm: bool,
}

impl ConfirmDialog {
    /// A confirmation identified by `id`, asking `body` under the title `heading`.
    ///
    /// `id` is `&'static str` because it is what
    /// [`crate::keybind::ActionComponent::apply_dialog_outcome`] routes on, and a routed
    /// identifier that could be built at runtime is one a caller can typo into a
    /// silently ignored outcome.
    #[must_use]
    pub fn new(
        context: ViewContext,
        id: &'static str,
        heading: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            context,
            id,
            heading: heading.into(),
            body: body.into(),
            confirm_label: String::from("Confirm"),
            cancel_label: String::from("Cancel"),
            confirm: true,
        }
    }

    /// Replace the two button labels.
    ///
    /// Naming the action beats `Confirm` when the action is destructive: `Restore` says
    /// what happens, and a user reading `Confirm` has to remember what they answered.
    #[must_use]
    pub fn with_labels(mut self, confirm: impl Into<String>, cancel: impl Into<String>) -> Self {
        self.confirm_label = confirm.into();
        self.cancel_label = cancel.into();
        self
    }

    fn decision(&self) -> DialogStep {
        if self.confirm {
            DialogStep::Resolved(DialogOutcome::Selected {
                dialog: self.id,
                value: String::from(CONFIRM_VALUE),
            })
        } else {
            DialogStep::Resolved(DialogOutcome::Cancelled)
        }
    }
}

impl Dialog for ConfirmDialog {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        self.heading.clone()
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::Medium
    }

    /// The buttons are the last row, and they are the row a confirmation cannot lose.
    fn anchor(&self) -> BodyAnchor {
        BodyAnchor::Tail
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        // One column of inner padding on each side, per `§11.5`.
        let text_width = width.saturating_sub(2);
        let mut lines = Vec::new();
        for row in wrap(&self.body, text_width) {
            lines.push(padded(&format!(" {row}"), width, self.context.text()));
        }
        lines.push(padded("", width, self.context.surface()));
        // The focused button is reversed — `primary` background over `background`
        // foreground — which is the one selection convention this TUI has, shared with
        // every list and the diff file tree (`§11.4`).
        let (confirm_style, cancel_style) = if self.confirm {
            (self.context.selected(), self.context.muted())
        } else {
            (self.context.muted(), self.context.selected())
        };
        let confirm = format!(" {} ", self.confirm_label);
        let cancel = format!(" {} ", self.cancel_label);
        let mut spans = vec![
            ratatui::text::Span::styled(String::from(" "), self.context.surface()),
            ratatui::text::Span::styled(confirm.clone(), confirm_style),
            ratatui::text::Span::styled(String::from(" "), self.context.surface()),
            ratatui::text::Span::styled(cancel.clone(), cancel_style),
        ];
        // Pad to the full width so the surface colour does not stop mid-row, the same
        // reason `padded` exists.
        let used = 1 + display_width(&confirm) + 1 + display_width(&cancel);
        if used < usize::from(width) {
            spans.push(ratatui::text::Span::styled(
                " ".repeat(usize::from(width) - used),
                self.context.surface(),
            ));
        }
        lines.push(Line::from(spans));
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "switch"), ("enter", "choose"), ("esc", "cancel")]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.select.prev" | "dialog.select.next" => {
                self.confirm = !self.confirm;
                DialogStep::Redraw
            }
            "dialog.select.submit" | "dialog.prompt.submit" => self.decision(),
            // `session_interrupt` is what the table binds escape to, and `app_exit` is
            // the chord a user in a raw-mode terminal reaches for. Both leave. See
            // `picker.rs`'s identical arm: without it the host absorbs the key and the
            // footer's `esc cancel` names a way out that does not exist.
            "app_exit" | "session_interrupt" => DialogStep::Resolved(DialogOutcome::Cancelled),
            _ => DialogStep::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: &MouseEvent, body: Rect) -> DialogStep {
        if event.kind != MouseEventKind::Up(MouseButton::Left)
            || event.column < body.left()
            || event.column >= body.right()
            || event.row < body.top()
            || event.row >= body.bottom()
            || event.row + 1 != body.bottom()
        {
            return DialogStep::Ignored;
        }

        let column = usize::from(event.column.saturating_sub(body.left()));
        let confirm_start = 1_usize;
        let confirm_end = confirm_start + display_width(&format!(" {} ", self.confirm_label));
        let cancel_start = confirm_end + 1;
        let cancel_end = cancel_start + display_width(&format!(" {} ", self.cancel_label));

        if (confirm_start..confirm_end).contains(&column) {
            self.confirm = true;
            self.decision()
        } else if (cancel_start..cancel_end).contains(&column) {
            self.confirm = false;
            self.decision()
        } else {
            DialogStep::Ignored
        }
    }
}

/// A message with one button.
///
/// For a fact the user must not miss and that a five-second corner notice cannot carry:
/// a multi-line error, or a path they will want to read twice. The dividing line against
/// [`crate::views::toast::Toast`] is length and consequence, not severity — a failure
/// short enough to read at a glance is still a toast.
pub struct AlertDialog {
    context: ViewContext,
    id: &'static str,
    heading: String,
    body: String,
}

impl AlertDialog {
    /// An alert identified by `id`.
    #[must_use]
    pub fn new(
        context: ViewContext,
        id: &'static str,
        heading: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            context,
            id,
            heading: heading.into(),
            body: body.into(),
        }
    }
}

impl Dialog for AlertDialog {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        self.heading.clone()
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::Medium
    }

    /// Same reason as the confirmation's: the button is the last row.
    fn anchor(&self) -> BodyAnchor {
        BodyAnchor::Tail
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let text_width = width.saturating_sub(2);
        let mut lines = Vec::new();
        for row in wrap(&self.body, text_width) {
            lines.push(padded(&format!(" {row}"), width, self.context.text()));
        }
        lines.push(padded("", width, self.context.surface()));
        lines.push(padded(" Dismiss ", width, self.context.selected()));
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("enter", "dismiss"), ("esc", "dismiss")]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.select.submit" | "dialog.prompt.submit" | "app_exit" | "session_interrupt" => {
                // One outcome, because there is one button. See the module docs.
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: &MouseEvent, body: Rect) -> DialogStep {
        if event.kind == MouseEventKind::Up(MouseButton::Left)
            && event.column >= body.left()
            && event.column < body.right()
            && event.row + 1 == body.bottom()
        {
            DialogStep::Resolved(DialogOutcome::Cancelled)
        } else {
            DialogStep::Ignored
        }
    }
}

/// The rows a [`PromptDialog`] gives its text area.
///
/// `§6.1`'s three. Independent of the transcript's `prompt_rows()`, which sizes the
/// *editor* against a live viewport; this is a fixed box inside a fixed-width dialog and
/// has no viewport to divide.
pub const PROMPT_ROWS: usize = 3;

/// A text prompt.
///
/// Characters arrive through [`Dialog::handle_typed`] rather than as actions, because a
/// printable key is the one thing the binding table deliberately does not claim — the
/// same seam the pickers' filter boxes use.
///
/// # No busy state
///
/// `§6.1` says a prompt blurs and refuses to submit while busy. That describes upstream's
/// rename prompt, which writes through a server call that can be in flight. This build's
/// prompt writes into the local editor buffer, which is never in flight, so a `busy` flag
/// here would be a field no caller could ever set — the `editor_open`/`tool_affordance`
/// failure this project has removed repeatedly. It is left out until a caller needs it.
pub struct PromptDialog {
    context: ViewContext,
    id: &'static str,
    heading: String,
    text: String,
}

impl PromptDialog {
    /// A prompt identified by `id`, pre-filled with `text`.
    ///
    /// Pre-filling rather than starting empty: every caller here is offering to edit
    /// something that already exists, and an empty box would ask the user to retype it.
    #[must_use]
    pub fn new(
        context: ViewContext,
        id: &'static str,
        heading: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            context,
            id,
            heading: heading.into(),
            text: text.into(),
        }
    }

    /// The text as it currently stands.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Dialog for PromptDialog {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        self.heading.clone()
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::Medium
    }

    /// The cursor is on the last row, so a short frame must not be what hides it.
    fn anchor(&self) -> BodyAnchor {
        BodyAnchor::Tail
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let text_width = width.saturating_sub(2);
        // The cursor is drawn as a glyph rather than moved with the terminal's own
        // cursor: `Component::render` has no way to place it, and every other surface in
        // this module marks position the same way.
        let mut rows = wrap(&format!("{}▏", self.text), text_width);
        // Keep the tail in view. A box that scrolled the *cursor* instead of the window
        // hides what the user is typing the moment they pass row three.
        if rows.len() > PROMPT_ROWS {
            rows.drain(..rows.len() - PROMPT_ROWS);
        }
        let mut lines = Vec::with_capacity(PROMPT_ROWS);
        for row in &rows {
            lines.push(padded(&format!(" {row}"), width, self.context.text()));
        }
        for _ in rows.len()..PROMPT_ROWS {
            lines.push(padded("", width, self.context.surface()));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("enter", "submit"), ("esc", "cancel")]
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.prompt.submit" | "dialog.select.submit" => {
                DialogStep::Resolved(DialogOutcome::Submitted {
                    dialog: self.id,
                    text: std::mem::take(&mut self.text),
                })
            }
            "app_exit" | "session_interrupt" => DialogStep::Resolved(DialogOutcome::Cancelled),
            "input_backspace" => {
                if self.text.pop().is_none() {
                    return DialogStep::Ignored;
                }
                DialogStep::Redraw
            }
            // An action this dialog has no arm for may still be a printable key the
            // binding table happened to claim, so it is offered to the text area before
            // being reported as unhandled — the same fall-through `SelectDialog` uses to
            // keep its filter typeable.
            _ => self.handle_typed(event),
        }
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        let Some(character) = crate::views::permission::typed_character(key) else {
            return DialogStep::Ignored;
        };
        self.text.push(character);
        DialogStep::Redraw
    }
}
