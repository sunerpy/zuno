//! Dialogs, and why none of them blocks.
//!
//! # A modal owns the keyboard and pointer, but never the exit
//!
//! Swallowing every action a dialog does not understand is what stops `session_new`
//! from firing behind a permission prompt, and it must stay. Applied to the exit
//! chord it was a trap: the scope chain a permission prompt installs resolves
//! `ctrl+d` to `session_delete` and `ctrl+c` to `input_clear`, neither of which the
//! prompt understands, so both were absorbed and never reached the one component
//! that sends [`crate::app::TerminalEvent::Shutdown`]. Raw mode having already taken
//! `SIGINT` away, the application could not be left at all while a prompt was up.
//!
//! An ignored action whose chord the table binds to [`crate::keybind::APP_EXIT`] is
//! therefore forwarded to the base. A dialog that wants the chord for itself still
//! gets it first — the permission prompt resolves it to a rejection — and this path
//! runs only once the dialog has said it has no use for it.
//!
//! `session_interrupt` is the deliberate second exception, with different semantics:
//! the visible dialog handles it first and the base always sees it afterwards. During
//! a running turn that makes one physical `Esc` both close the prompt and arm turn
//! interruption; otherwise the advertised two-press contract becomes three presses
//! whenever a question or permission prompt is open.
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
//! - [`DialogHost`] forwards engine, wake, resize, and lifecycle events to the base
//!   component even while a dialog is open. Pointer events belong to the visible modal
//!   instead, so a click cannot activate covered content. This is the property the
//!   no-stall test asserts: background progress keeps moving without pointer
//!   click-through.
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
//!
//! # Width is the host's decision, not each dialog's
//!
//! `§11.4` fixes three widths — 60, 88 and 116 columns — and a dialog picks a tier
//! rather than a number ([`Dialog::width`]). The clamp, the centring and the
//! narrow-terminal fallback all happen once, here, for the reason the theme is shared
//! rather than copied: a per-dialog width computation is a rule that only *some* future
//! dialog remembers, and the failure is a stack whose two halves disagree about where
//! their left edge is.
//!
//! Fixed columns rather than a fraction of the terminal, again from `§11.4`: 60 columns
//! is about the floor for a readable list, and a percentage stretches that list into a
//! sparse band on a wide terminal. See [`dialog_columns`] for what happens when the
//! terminal is narrower than the tier — the case the plan does not name.
//!
//! # Toasts sit above the stack
//!
//! `§11.4` orders the layers backdrop, dialog, toast, and the host owns the toast slot
//! for that reason: a toast the base screen drew would be *under* an open modal, so a
//! copy made while a picker was up would be confirmed behind it. The base asks for one
//! through [`ActionComponent::drain_toasts`], the same seam it asks for a dialog
//! through.

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::{ActionComponent, Definition, PendingPrefix, is_exit_request};
use crate::views::autocomplete::WhichKeyView;
use crate::views::toast::ToastLayer;
use crate::views::{ViewContext, fill, hint, padded};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, symbols};
use std::time::Instant;

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
    /// The dialog produced an answer but remains mounted.
    Emitted(DialogOutcome),
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
    /// The session list requested an operation on its highlighted row.
    Session(crate::views::picker::SessionDialogAction),
    /// The durable input queue requested an edit or cancellation.
    QueuedInput(crate::views::picker::QueuedInputDialogAction),
    /// The MCP dialog requested an explicit lifecycle target.
    McpToggle(crate::views::picker::McpToggleRequest),
    /// The subagent dialog requested cancellation while remaining open.
    JobCancel {
        /// Durable job identifier.
        job_id: String,
    },
    /// The background-terminal dialog requested cancellation while remaining open.
    BackgroundCancel { execution_id: String },
    /// Approve one pending resident-memory candidate.
    MemoryApply { id: String },
    /// Reject one pending resident-memory candidate.
    MemoryReject { id: String },
    /// Undo one applied resident-memory candidate.
    MemoryUndo { id: String },
    /// Open an editor for a candidate, then approve the edited result.
    MemoryEditRequested { id: String, content: String },
    /// Remove one current resident-memory entry after in-dialog confirmation.
    MemoryRemove {
        scope: zuno_types::MemoryScope,
        content: String,
    },
}

/// One of `§11.4`'s three fixed dialog widths.
///
/// Three named tiers rather than a `u16` per dialog. A number lets a caller pick 62,
/// and then two dialogs in one stack have edges a column apart for no reason a reader
/// can recover; a tier makes "which of the three is this" the only question, and the
/// answer is reviewable against the plan's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogWidth {
    /// 60 columns: confirm, alert, prompt, rename, variant.
    Medium,
    /// 88 columns: model, agent, session, theme, skill, palette.
    Large,
    /// 116 columns: status, debug, inline diff, the permission prompt.
    XLarge,
}

/// Where a dialog belongs relative to the base surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogPlacement {
    /// A conventional overlay centred over the frame.
    Overlay,
    /// The base component's composer region, replacing the input surface.
    Composer,
}

impl DialogWidth {
    /// This tier's width in terminal columns.
    #[must_use]
    pub const fn columns(self) -> u16 {
        match self {
            Self::Medium => 60,
            Self::Large => 88,
            Self::XLarge => 116,
        }
    }
}

/// Columns left free around a dialog when the terminal cannot hold its tier.
///
/// `§11.4`'s `term_width - 4`: two columns of breathing room on each side once the
/// dialog is centred, so the surface behind it still reads as a surface rather than as
/// a border the dialog forgot to draw.
pub const DIALOG_GUTTER: u16 = 4;

/// How many columns a dialog at `tier` gets on a terminal `available` columns wide.
///
/// `§11.4` specifies `min(tier, available - 4)` and stops there. Two cases it does not
/// name, both reachable:
///
/// * **Below 60 columns** the tier is abandoned entirely and the dialog takes
///   `available - 4`. There is no lower tier to fall back to, and refusing to shrink
///   would draw a 60-column dialog into a 20-column frame — either clipped by the
///   backend or panicking on an out-of-bounds `Rect`, depending on which widget got
///   there first. The plan's acceptance case is exactly this: 20×10 must not panic.
/// * **At four columns or fewer** the gutter itself is abandoned and the dialog takes
///   the whole width. `available - 4` saturates to zero there, and a zero-width dialog
///   is not a small dialog — it is an *invisible modal that still owns the keyboard*,
///   which leaves the user pressing keys at something they cannot see. A cramped prompt
///   is recoverable; an invisible one is the trap the exit-chord forwarding above exists
///   to prevent, arrived at from the other direction.
#[must_use]
pub const fn dialog_columns(tier: DialogWidth, available: u16) -> u16 {
    let usable = available.saturating_sub(DIALOG_GUTTER);
    if usable == 0 {
        return available;
    }
    let tier = tier.columns();
    if tier < usable { tier } else { usable }
}

/// Which end of a body that does not fit is kept.
///
/// `§11.4` asks for internal scrolling on overflow, which the list dialogs implement
/// themselves by windowing. For the base forms the whole question is which row must not
/// be the one lost, and that is answerable without a scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyAnchor {
    /// Keep the first rows. What a list wants: the cursor is windowed into view already.
    Head,
    /// Keep the last rows.
    ///
    /// For a form whose final row is its buttons. Measured at 20×10: the confirmation's
    /// wrapped question filled the frame and pushed `Restore` / `Keep` off the bottom, so
    /// a destructive prompt was showing a question with no visible answer and no way to
    /// see which button `Enter` would press. Losing the head of a sentence whose title
    /// still names the action is the smaller loss by a wide margin.
    Tail,
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

    /// Which end of an over-long body survives when the frame cannot hold all of it.
    ///
    /// [`BodyAnchor::Head`] by default, which is what a list wants and what clipping did
    /// before this existed. A form whose decisive row is the *last* one says so — see
    /// [`BodyAnchor::Tail`] for the measured reason.
    fn anchor(&self) -> BodyAnchor {
        BodyAnchor::Head
    }

    /// Which of `§11.4`'s three tiers this dialog is laid out at.
    ///
    /// `Large` by default because that is the tier the majority of the shipped set sits
    /// in — every list surface — so the default is the answer a new list dialog wants
    /// without thinking about it. The two forms that want something else say so.
    fn width(&self) -> DialogWidth {
        DialogWidth::Large
    }

    /// Where this dialog is laid out.
    fn placement(&self) -> DialogPlacement {
        DialogPlacement::Overlay
    }

    /// Additional keybind scopes active only while this dialog owns the keyboard.
    fn focused_scopes(&self) -> Vec<&'static str> {
        Vec::new()
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

    /// Act on one pointer event inside the rendered body.
    ///
    /// The host owns modal geometry and translates nothing: `body` is the exact
    /// absolute rectangle used to draw [`Dialog::lines`]. The dialog owns the meaning
    /// of rows and buttons within it. Events outside this rectangle may still be
    /// reported so the dialog can deliberately support them, but the default ignores
    /// every pointer event.
    fn handle_mouse(&mut self, _event: &MouseEvent, _body: Rect) -> DialogStep {
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
    pending: PendingPrefix,
    which_key: WhichKeyView,
    toasts: ToastLayer,
    rendered_dialog: Option<RenderedDialog>,
}

/// Geometry from the last frame, paired with the dialog it belongs to.
///
/// Pointer events arrive after rendering, so keeping this in the host avoids every
/// dialog independently reconstructing the width tier, placement, clipping, and
/// composer-region rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderedDialog {
    id: &'static str,
    body: Rect,
}

impl DialogHost {
    /// A host over `base`.
    #[must_use]
    pub fn new(context: ViewContext, base: Box<dyn ActionComponent>) -> Self {
        Self {
            toasts: ToastLayer::new(context.clone()),
            which_key: WhichKeyView::new(context.clone()),
            context,
            base,
            stack: Vec::new(),
            outcomes: Vec::new(),
            pending: PendingPrefix::default(),
            rendered_dialog: None,
        }
    }

    /// Let this host's timed layers wake the loop when they expire.
    ///
    /// Without this they still expire, but only once something else brings the loop
    /// round — see [`crate::views::toast`] for why one deadline and one wake is the shape
    /// chosen over polling.
    ///
    /// Both layers are armed from one call on purpose. Two setters would let a host wire
    /// one and forget the other, which is how a surface ends up finished and untriggered.
    #[must_use]
    pub fn with_waker(
        mut self,
        waker: tokio::sync::mpsc::Sender<crate::app::TerminalEvent>,
    ) -> Self {
        self.toasts = ToastLayer::new(self.context.clone()).with_waker(waker.clone());
        self.which_key = WhichKeyView::new(self.context.clone()).with_waker(waker);
        self
    }

    /// The toast slot, for a host that wants to raise one itself.
    pub const fn toasts_mut(&mut self) -> &mut ToastLayer {
        &mut self.toasts
    }

    /// Move anything the base asked to show into the toast slot.
    ///
    /// Only the last survives, because the slot holds one. A base that raised two in a
    /// single action meant the second.
    fn take_toasts(&mut self) -> bool {
        let requested = self.base.drain_toasts();
        let raised = !requested.is_empty();
        if let Some(toast) = requested.into_iter().next_back() {
            self.toasts.show(toast);
        }
        raised
    }

    /// Push a dialog onto the stack.
    pub fn open(&mut self, dialog: Box<dyn Dialog>) {
        self.stack.push(dialog);
        self.rendered_dialog = None;
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
        let dismissed = self.stack.pop().is_some();
        if dismissed {
            self.rendered_dialog = None;
        }
        dismissed
    }

    /// Open whatever the base asked for, reporting whether anything opened.
    fn open_requested(&mut self) -> bool {
        let requested = self.base.drain_dialogs();
        let opened = !requested.is_empty();
        for dialog in requested {
            self.stack.push(dialog);
        }
        if opened {
            self.rendered_dialog = None;
        }
        opened
    }

    /// Apply one dialog step and keep every input path's lifecycle identical.
    ///
    /// Keys and pointer events both come through here. In particular, a resolved
    /// message-action menu must be able to open the confirmation requested by its
    /// base component regardless of whether the row was chosen with Enter or a click.
    fn settle_dialog_step(&mut self, id: &'static str, step: DialogStep) -> EventResult {
        match step {
            DialogStep::Ignored => EventResult {
                handled: true,
                redraw: false,
            },
            DialogStep::Redraw => EventResult::REDRAW,
            DialogStep::Emitted(outcome) => {
                self.base.apply_dialog_outcome(id, &outcome);
                self.outcomes.push((id, outcome));
                self.take_toasts();
                EventResult::REDRAW
            }
            DialogStep::Resolved(outcome) => {
                self.stack.pop();
                self.rendered_dialog = None;
                // The base is told before the outcome is queued, so a component that
                // asked for the dialog can act on the answer without owning the queue a
                // host also drains.
                self.base.apply_dialog_outcome(id, &outcome);
                self.outcomes.push((id, outcome));
                // Answering one dialog can request the next surface, as Revert does
                // when it escalates into a destructive-action confirmation.
                self.open_requested();
                self.take_toasts();
                EventResult::REDRAW
            }
        }
    }

    /// The pending leader sequence and its continuations.
    #[must_use]
    pub const fn pending(&self) -> &PendingPrefix {
        &self.pending
    }

    /// Draw the centred which-key overlay, if one is due.
    ///
    /// Above the dialog and below the toast. The view owns its content-derived frame so it
    /// can leave transcript context visible and cannot be clipped against the bottom edge.
    fn render_which_key(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // Render unconditionally: the view prunes its own deadline before deciding whether
        // it has a frame. This preserves the wake-driven timeout even when the prefix expired
        // between two otherwise idle frames.
        self.which_key.render(frame, area);
    }

    fn render_dialog(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(dialog) = self.stack.last() else {
            self.rendered_dialog = None;
            return;
        };
        let id = dialog.id();
        let placement = dialog.placement();
        let area = match placement {
            DialogPlacement::Overlay => area,
            DialogPlacement::Composer => self.base.dialog_region(id, area).unwrap_or(area),
        };
        if area.width == 0 || area.height == 0 {
            self.rendered_dialog = None;
            return;
        }
        let dialog = self
            .stack
            .last_mut()
            .expect("the dialog observed above remains on the stack");
        // `§11.4`'s tier, clamped once here rather than by each dialog. Everything below
        // measures from `columns`, so a dialog cannot disagree with its own frame.
        let columns = match placement {
            DialogPlacement::Overlay => dialog_columns(dialog.width(), area.width),
            DialogPlacement::Composer => area.width,
        };
        let inner_width = columns.saturating_sub(2);
        let title = dialog.title();
        let hints = dialog.hints();
        let anchor = dialog.anchor();
        let body = dialog.lines(inner_width);
        let content_rows = u16::try_from(body.len()).unwrap_or(u16::MAX);
        let desired = dialog.desired_height(content_rows, area.height);

        // `max(3)` after `min(area.height)` and not before: a frame with one or two rows
        // has to yield a one- or two-row dialog, and clamping up first would place a
        // three-row region outside the buffer.
        let height = desired.min(area.height).max(3).min(area.height);
        let region = Rect {
            x: area.x + (area.width.saturating_sub(columns)) / 2,
            y: match placement {
                DialogPlacement::Overlay => area.y + area.height.saturating_sub(height) / 2,
                DialogPlacement::Composer => area.y + area.height.saturating_sub(height),
            },
            width: columns,
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

        // Clipped here rather than left to `Paragraph`, which always drops the tail. Which
        // end is dropped is the dialog's call; see `BodyAnchor`.
        let rows = usize::from(body_area.height);
        let body = if body.len() > rows && anchor == BodyAnchor::Tail {
            body[body.len() - rows..].to_vec()
        } else {
            body
        };
        let rendered_body = Rect {
            x: body_area.x + 1,
            width: inner_width,
            ..body_area
        };
        self.rendered_dialog = Some(RenderedDialog {
            id,
            body: rendered_body,
        });
        Paragraph::new(body)
            .style(self.context.surface())
            .render(rendered_body, frame.buffer_mut());

        let footer = Rect {
            x: footer_area.x + 1,
            width: inner_width,
            ..footer_area
        };
        fill(frame.buffer_mut(), footer, self.context.element());
        // Whole pairs, and the last one that does not fit ends the row rather than being cut
        // through. `Paragraph` clips at the column, which spelled `esc cancel` as `es` — and a
        // key spelled halfway still reads as a key, so the footer was naming a chord that does
        // not exist. Dropping the pair says less; it does not say something false. Same
        // degradation order as `§7.1` and the same call `WhichKeyView` makes on its last cell.
        //
        // Reachable at two of the widths this crate is accepted at: the shipped four-hint
        // footer is 51 columns against the 34 a 40-column terminal leaves, and a fifth hint
        // puts it past the 54 a 60-column terminal leaves.
        let mut spans = Vec::new();
        let mut used = 0_usize;
        for (key, label) in hints {
            let pair = hint(key, label, &self.context);
            let cost = pair
                .iter()
                .map(|span| crate::views::display_width(&span.content))
                .sum::<usize>();
            // The pair's *trailing separator* is excluded from the fit test, because it is
            // spacing before the next pair rather than part of this one. The shipped
            // three-hint footer is 35 columns against the 34 a 40-column terminal leaves, and
            // the single overflowing column is that trailing space — so a test on the whole
            // cost would drop `esc cancel` from a row it has always fitted in.
            let separator = pair
                .last()
                .filter(|span| span.content.trim().is_empty())
                .map_or(0, |span| crate::views::display_width(&span.content));
            if used + cost - separator > usize::from(inner_width) {
                break;
            }
            used += cost;
            spans.extend(pair);
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
        //
        // Told first, so the base draws the frame knowing what is over it. See
        // `ActionComponent::observe_modal` for why this is derived here every frame
        // rather than pushed when the stack changes.
        self.base.observe_modal(self.active());
        self.base.render(frame, area);
        self.render_dialog(frame, area);
        self.render_which_key(frame, area);
        // Last, so it is on top of the modal. `§11.4`: backdrop, dialog, toast.
        self.toasts.render(frame.buffer_mut(), area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        if matches!(
            event,
            AppEvent::Terminal(crate::app::TerminalEvent::Input(_))
        ) {
            // A visible transient panel belongs to an interaction, not to the first
            // keystroke that happened to open it. Pointer movement, wheel input and keys
            // routed to its current surface all buy a fresh timeout. Completed or
            // abandoned key sequences clear the prefix before they reach this branch, so
            // `touch` cannot keep a panel alive after its action has resolved.
            self.which_key.touch(Instant::now());
        }
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
                    let step = dialog.handle_typed(key);
                    return self.settle_dialog_step(id, step);
                }
                self.base.handle_event(event)
            }
            AppEvent::Terminal(crate::app::TerminalEvent::Input(
                crossterm::event::Event::Mouse(mouse),
            )) if self.is_open() => {
                // A modal captures the pointer as well as the keyboard. If no frame has
                // painted this dialog yet, absorb the event rather than guessing at
                // geometry or allowing it to click through to covered content.
                let Some(rendered) = self.rendered_dialog else {
                    return EventResult {
                        handled: true,
                        redraw: false,
                    };
                };
                let Some(dialog) = self.stack.last_mut() else {
                    return EventResult {
                        handled: true,
                        redraw: false,
                    };
                };
                if dialog.id() != rendered.id {
                    return EventResult {
                        handled: true,
                        redraw: false,
                    };
                }
                let id = dialog.id();
                let step = dialog.handle_mouse(mouse, rendered.body);
                self.settle_dialog_step(id, step)
            }
            _ => {
                // Engine, wake, resize, and lifecycle events still reach the base while
                // a modal is open. See the module docs: this is what prevents a dialog
                // from stalling the agent loop.
                let mut result = self.base.handle_event(event);
                // Both seams are serviced here as well as after an action, because not
                // every request comes from a key. A wake is how the base learns its
                // external-editor worker answered, and a failed edit has to raise an alert
                // — which was requested and never opened while this branch only drained
                // for keys. The engine's own events reach the base the same way.
                //
                // The prunes are what make `TerminalEvent::Wake` remove an expired toast and
                // close an expired which-key panel. Both are needed *here*, not in `render`:
                // a wake only paints a frame if some component reports `redraw`, so pruning
                // during `render` would leave the stale panel on the physical terminal until
                // an unrelated event happened to redraw. Measured on a real terminal — the
                // panel sat there indefinitely past its 2000 ms deadline, while every
                // offscreen test passed because rendering a frame is what those tests do.
                let opened = self.open_requested();
                let now = std::time::Instant::now();
                let expired = self.toasts.prune(now) | self.which_key.prune(now);
                if opened || self.take_toasts() || expired {
                    result = EventResult {
                        handled: result.handled,
                        redraw: true,
                    };
                }
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
            let step = dialog.handle_action(action, event);
            let dialog_result = match step {
                DialogStep::Ignored if is_exit_request(event) => {
                    // The one forwarded class. See the module docs: absorbing this
                    // leaves a user with no way out of a raw-mode terminal.
                    return self.base.handle_action(action, event);
                }
                other => self.settle_dialog_step(id, other),
            };
            if action.name == "session_interrupt" {
                // One physical escape has two responsibilities while a running turn is
                // parked behind a modal: dismiss the visible prompt and arm cancellation
                // of the turn. Keeping only the first responsibility means two presses
                // close the prompt and merely arm the turn, so the advertised "press Esc
                // twice" contract needs a surprising third press.
                let base_result = self.base.handle_action(action, event);
                let opened = self.open_requested();
                let raised = self.take_toasts();
                return if opened || raised {
                    EventResult::REDRAW.merge(dialog_result).merge(base_result)
                } else {
                    dialog_result.merge(base_result)
                };
            }
            return dialog_result;
        }
        let result = self.base.handle_action(action, event);
        // Drained after the action, never before: the action is what asks.
        let opened = self.open_requested();
        let raised = self.take_toasts();
        if opened || raised {
            return EventResult::REDRAW;
        }
        result
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        if let Some(dialog) = self.stack.last() {
            let mut scopes = dialog.focused_scopes();
            scopes.extend([
                "permission.prompt",
                "dialog.select",
                "dialog.prompt",
                "session",
            ]);
            scopes
        } else {
            self.base.focused_scopes()
        }
    }

    fn dialog_region(&self, dialog: &'static str, area: Rect) -> Option<Rect> {
        self.base.dialog_region(dialog, area)
    }

    fn pending_changed(&mut self, pending: &PendingPrefix) -> EventResult {
        self.pending = pending.clone();
        let changed = self.which_key.observe(pending, Instant::now());
        // The base is told either way. It is the only component that can act on a
        // prefix, and gating that on this host's own redraw need would make the
        // notification arrive only sometimes.
        let below = self.base.pending_changed(pending);
        if changed || self.is_open() {
            return EventResult::REDRAW.merge(below);
        }
        below
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
            AppEvent::AnimationFrame => {}
        }
        self.inner.handle_event(event)
    }
}

impl<C: Component> ActionComponent for ObservedBase<C> {
    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> EventResult {
        EventResult::IGNORED
    }
}
