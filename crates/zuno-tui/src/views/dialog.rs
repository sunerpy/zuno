//! Dialogs, and why none of them blocks.
//!
//! # A modal owns the keyboard, but never the exit
//!
//! Swallowing every action a dialog does not understand is what stops `session_new`
//! from firing behind a permission prompt, and it must stay. Applied to the exit
//! chord it was a trap: the scope chain a permission prompt installs resolves
//! `ctrl+d` to `session_delete` and `ctrl+c` to `input_clear`, neither of which the
//! prompt understands, so both were absorbed and never reached the one component
//! that sends [`crate::app::TerminalEvent::Shutdown`]. Raw mode having already taken
//! `SIGINT` away, the application could not be left at all while a prompt was up.
//!
//! So exactly one class of ignored action is forwarded: one whose chord the table
//! binds to [`crate::keybind::APP_EXIT`]. A dialog that wants the chord for itself
//! still gets it first — the permission prompt resolves it to a rejection — and this
//! only runs once the dialog has said it has no use for it.
//!
//! # A dialog is state in the tree, not a call that waits
//!
//! The tempting shape for a modal is a function that renders, reads input, and
//! returns the answer. It is also a deadlock. The TUI's event loop
//! ([`crate::app::App::run`]) is the single consumer of terminal input, engine
//! [`zuno_engine::r#loop::TurnEvent`]s, **and** the terminal-lease wake
//! notification. A dialog that awaits its answer inside `handle_event` stops all
//! three: engine progress stops being drawn, and — worse — the lease the plugin
//! host takes to run an OAuth prompt can neither be granted nor reclaimed, because
//! the reclaim path needs the render lock this frame is holding.
//!
//! So the contract is:
//!
//! - [`Dialog::handle_action`] returns immediately, always. It reports
//!   [`DialogStep::Resolved`] when the user has decided, and the host records the
//!   outcome in a queue for whoever asked.
//! - [`DialogHost`] forwards **every non-key event to the base component even while
//!   a dialog is open**. A dialog captures keys, not the world. This is the property
//!   the no-stall test asserts, and it fails if the host is made modal.
//!
//! # Keys reach a dialog as actions
//!
//! The `dialog.select.*` and `dialog.prompt.submit` rows of the binding table
//! (`packages/tui/src/config/keybind.ts:202-221`) exist precisely so a dialog does
//! not name a key. [`DialogHost`] implements
//! [`crate::keybind::ActionComponent`], so it is wrapped in a
//! [`crate::keybind::KeyDispatcher`] and receives resolved
//! [`crate::keybind::Definition`]s.
//!
//! # Stacking
//!
//! Dialogs stack, because upstream's do: a permission prompt escalates into an
//! "always allow" confirmation, and a session picker can open a rename prompt. The
//! top of the stack has the keys; everything below it renders nothing.

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::{ActionComponent, Chord, Definition, is_exit_request};
use crate::views::{ViewContext, fill, hint, padded};
use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, symbols};

#[cfg(test)]
#[path = "dialog_tests.rs"]
mod tests;

/// What a dialog did with one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogStep {
    /// The action was not for this dialog.
    Ignored,
    /// The dialog changed state and needs a frame.
    Redraw,
    /// The dialog is finished; `outcome` is its answer.
    Resolved(DialogOutcome),
}

/// A finished dialog's answer.
///
/// One enum rather than a per-dialog associated type, because the host owns a
/// heterogeneous stack and the consumer drains one queue. The payload is a string
/// or a structured reply, which is enough for every dialog in this module and keeps
/// the host free of generics it would have to erase anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    /// The user dismissed the dialog without choosing.
    Cancelled,
    /// The user chose the item with this value.
    Selected {
        /// Which dialog answered, so a consumer can route the value.
        dialog: &'static str,
        /// The chosen item's opaque value.
        value: String,
    },
    /// The user typed an answer.
    Submitted {
        /// Which dialog answered.
        dialog: &'static str,
        /// The typed text.
        text: String,
    },
    /// A permission request was replied to.
    Permission(crate::views::permission::PermissionDecision),
    /// A question was answered, one label list per question.
    Question(Vec<Vec<String>>),
}

/// A modal surface.
///
/// Deliberately not `Component`: a dialog is rendered by its host inside a computed
/// region, and its input arrives as an action rather than an [`AppEvent`]. Making it
/// a `Component` would let it be mounted directly into the tree, where nothing would
/// enforce the non-blocking contract above.
pub trait Dialog: Send {
    /// A stable identifier, used in [`DialogOutcome`] and in tests.
    fn id(&self) -> &'static str;

    /// The line shown in the dialog's title bar.
    fn title(&self) -> String;

    /// The dialog's body rows, for `width` columns.
    fn lines(&mut self, width: u16) -> Vec<Line<'static>>;

    /// The footer hints, as `(key label, description)` pairs.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "move"), ("enter", "select"), ("esc", "cancel")]
    }

    /// Act on one resolved binding.
    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep;

    /// Act on a key no binding claimed, which is how a filter box is typed into.
    ///
    /// Separate from [`Dialog::handle_action`] because the two arrive by different routes:
    /// an action comes from [`crate::keybind::KeyDispatcher`], while an unclaimed key
    /// reaches the host as an ordinary event. A dialog that only implemented the former
    /// could be navigated but not typed into.
    fn handle_typed(&mut self, _key: &KeyEvent) -> DialogStep {
        DialogStep::Ignored
    }

    /// Rows the dialog wants, given the rows its body produced and the rows the
    /// frame has.
    ///
    /// The default fits the content plus the title and footer rows. A dialog that
    /// wants otherwise — the permission prompt caps itself so the transcript behind
    /// it stays readable — overrides this. `content_rows` is passed in rather than
    /// re-derived because [`Dialog::lines`] takes `&mut self` and a size query must
    /// not be able to mutate the dialog.
    fn desired_height(&self, content_rows: u16, available: u16) -> u16 {
        content_rows.saturating_add(2).min(available)
    }
}

/// The dialog stack, mounted above a base component.
///
/// `base` keeps receiving engine events, resizes, and shutdown while a dialog is
/// open. That is the whole point; see the module docs.
pub struct DialogHost {
    context: ViewContext,
    base: Box<dyn ActionComponent>,
    stack: Vec<Box<dyn Dialog>>,
    outcomes: Vec<(&'static str, DialogOutcome)>,
    pending: Vec<Chord>,
}

impl DialogHost {
    /// A host over `base`.
    #[must_use]
    pub fn new(context: ViewContext, base: Box<dyn ActionComponent>) -> Self {
        Self {
            context,
            base,
            stack: Vec::new(),
            outcomes: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Push a dialog onto the stack.
    pub fn open(&mut self, dialog: Box<dyn Dialog>) {
        self.stack.push(dialog);
    }

    /// Whether a dialog is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.stack.is_empty()
    }

    /// How deep the stack is.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// The id of the dialog with the keys, if any.
    #[must_use]
    pub fn active(&self) -> Option<&'static str> {
        self.stack.last().map(|dialog| dialog.id())
    }

    /// Take every outcome produced since the last drain.
    ///
    /// A queue rather than a callback: a callback would run inside `handle_event`,
    /// which is exactly the place a reply must not be awaited from.
    pub fn drain_outcomes(&mut self) -> Vec<(&'static str, DialogOutcome)> {
        std::mem::take(&mut self.outcomes)
    }

    /// Close the top dialog without an outcome.
    pub fn dismiss(&mut self) -> bool {
        self.stack.pop().is_some()
    }

    /// Open whatever the base asked for, reporting whether anything opened.
    fn open_requested(&mut self) -> bool {
        let requested = self.base.drain_dialogs();
        let opened = !requested.is_empty();
        for dialog in requested {
            self.stack.push(dialog);
        }
        opened
    }

    /// The pending leader chords, for a which-key surface.
    #[must_use]
    pub fn pending(&self) -> &[Chord] {
        &self.pending
    }

    fn render_dialog(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(dialog) = self.stack.last_mut() else {
            return;
        };
        let inner_width = area.width.saturating_sub(2);
        let title = dialog.title();
        let hints = dialog.hints();
        let body = dialog.lines(inner_width);
        let content_rows = u16::try_from(body.len()).unwrap_or(u16::MAX);
        let desired = dialog.desired_height(content_rows, area.height);

        let height = desired.min(area.height).max(3);
        let region = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(height),
            width: area.width,
            height,
        };
        fill(frame.buffer_mut(), region, self.context.surface());

        let [title_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(region);

        // The left rule is the oracle's split border (`ui/border.ts`): a full box
        // would cost two of the few rows a prompt has.
        for y in region.top()..region.bottom() {
            let cell = &mut frame.buffer_mut()[(region.left(), y)];
            cell.set_style(self.context.accent());
            cell.set_symbol(symbols::line::VERTICAL);
        }
        let text_area = Rect {
            x: region.left() + 1,
            y: region.top(),
            width: inner_width,
            height: region.height,
        };
        let _ = text_area;

        Paragraph::new(vec![padded(
            &format!(" {title}"),
            inner_width,
            self.context.title(),
        )])
        .render(
            Rect {
                x: title_area.x + 1,
                width: inner_width,
                ..title_area
            },
            frame.buffer_mut(),
        );

        Paragraph::new(body).style(self.context.surface()).render(
            Rect {
                x: body_area.x + 1,
                width: inner_width,
                ..body_area
            },
            frame.buffer_mut(),
        );

        let footer = Rect {
            x: footer_area.x + 1,
            width: inner_width,
            ..footer_area
        };
        fill(frame.buffer_mut(), footer, self.context.element());
        let mut spans = Vec::new();
        for (key, label) in hints {
            spans.extend(hint(key, label, &self.context));
        }
        Paragraph::new(vec![Line::from(spans)])
            .style(self.context.element())
            .render(footer, frame.buffer_mut());
    }
}

impl Component for DialogHost {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // The base always renders. A dialog is an overlay on a live application,
        // not a replacement for it, so a streaming answer stays visible behind a
        // permission prompt — which is the whole reason a user can judge the
        // prompt.
        self.base.render(frame, area);
        self.render_dialog(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        // Non-key events reach the base unconditionally. See the module docs: this
        // single line is what keeps an open dialog from stalling the loop.
        match event {
            AppEvent::Terminal(crate::app::TerminalEvent::Input(crossterm::event::Event::Key(
                key,
            ))) => {
                // Key routing belongs to `KeyDispatcher`, which calls `handle_action`
                // below. Reaching here means no binding matched — an ordinary printable
                // character, in practice — so it belongs to whoever has the keyboard.
                //
                // While a dialog is open, that is the dialog and never the base. Sending
                // it to the base instead is what put a picker's filter text into the
                // prompt behind it: every keystroke both failed to filter and appended
                // to a message the user was not writing. A modal owns the keyboard; only
                // the exit chord is forwarded, and that is handled in `handle_action`.
                if let Some(dialog) = self.stack.last_mut() {
                    let id = dialog.id();
                    return match dialog.handle_typed(key) {
                        DialogStep::Resolved(outcome) => {
                            self.stack.pop();
                            self.base.apply_dialog_outcome(id, &outcome);
                            self.outcomes.push((id, outcome));
                            EventResult::REDRAW
                        }
                        DialogStep::Redraw => EventResult::REDRAW,
                        DialogStep::Ignored => EventResult {
                            handled: true,
                            redraw: false,
                        },
                    };
                }
                self.base.handle_event(event)
            }
            _ => {
                let result = self.base.handle_event(event);
                if self.is_open() {
                    // A dialog is drawn over the base, so a base repaint has to
                    // repaint the dialog too.
                    EventResult {
                        handled: result.handled,
                        redraw: result.redraw,
                    }
                } else {
                    result
                }
            }
        }
    }
}

impl ActionComponent for DialogHost {
    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult {
        if let Some(dialog) = self.stack.last_mut() {
            let id = dialog.id();
            return match dialog.handle_action(action, event) {
                DialogStep::Ignored if is_exit_request(event) => {
                    // The one forwarded class. See the module docs: absorbing this
                    // leaves a user with no way out of a raw-mode terminal.
                    self.base.handle_action(action, event)
                }
                DialogStep::Ignored => {
                    // An action a dialog does not understand is *not* forwarded to
                    // the base. A modal owns the keyboard; forwarding would let
                    // `session_new` fire while a permission prompt is up.
                    EventResult {
                        handled: true,
                        redraw: false,
                    }
                }
                DialogStep::Redraw => EventResult::REDRAW,
                DialogStep::Resolved(outcome) => {
                    self.stack.pop();
                    // The base is told before the outcome is queued, so a component that
                    // asked for the dialog can act on the answer without owning the
                    // queue a host also drains.
                    self.base.apply_dialog_outcome(id, &outcome);
                    self.outcomes.push((id, outcome));
                    self.open_requested();
                    EventResult::REDRAW
                }
            };
        }
        let result = self.base.handle_action(action, event);
        // Drained after the action, never before: the action is what asks.
        if self.open_requested() {
            return EventResult::REDRAW;
        }
        result
    }

    fn pending_changed(&mut self, pending: &[Chord]) -> EventResult {
        self.pending = pending.to_vec();
        if self.is_open() {
            return EventResult::REDRAW;
        }
        self.base.pending_changed(pending)
    }
}

/// A component that counts what it observed, and the base of the no-stall test.
///
/// Not test-only: it is also how a host embeds a view that has no actions of its
/// own, and the counting is what makes "the loop kept running" observable.
pub struct ObservedBase<C: Component> {
    inner: C,
    engine_events: usize,
    terminal_events: usize,
}

impl<C: Component> ObservedBase<C> {
    /// Wrap `inner`.
    #[must_use]
    pub const fn new(inner: C) -> Self {
        Self {
            inner,
            engine_events: 0,
            terminal_events: 0,
        }
    }

    /// Engine events observed so far.
    #[must_use]
    pub const fn engine_events(&self) -> usize {
        self.engine_events
    }

    /// Terminal events observed so far.
    #[must_use]
    pub const fn terminal_events(&self) -> usize {
        self.terminal_events
    }

    /// The wrapped component.
    #[must_use]
    pub const fn inner(&self) -> &C {
        &self.inner
    }
}

impl<C: Component> Component for ObservedBase<C> {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inner.render(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        match event {
            AppEvent::Engine(_) => self.engine_events += 1,
            AppEvent::Terminal(_) => self.terminal_events += 1,
        }
        self.inner.handle_event(event)
    }
}

impl<C: Component> ActionComponent for ObservedBase<C> {
    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> EventResult {
        EventResult::IGNORED
    }
}
