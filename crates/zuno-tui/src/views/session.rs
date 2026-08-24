//! The composed session screen: transcript, reply identity, composer, and live footer.
//!
//! Todo 76 built every view and left this composition to whoever booted the TUI,
//! and nothing did — so the views were reachable only from their own tests. This
//! is the one type a host needs to construct in order to have a screen, and it
//! lives here rather than in the CLI so that rendering stays inside this crate and
//! the host only wires channels.
//!
//! # A submitted prompt leaves through a channel, and the turn comes back as events
//!
//! [`SessionScreen::with_prompt_sink`] is the only outward edge this screen has
//! besides shutdown, and it is deliberately as thin as one: a typed submission out, and
//! [`zuno_engine::r#loop::TurnEvent`]s back in through
//! [`crate::app::AppEvent::Engine`]. The screen therefore knows nothing about
//! sessions, providers, databases or tools — a turn driver is not a collaborator it
//! holds, it is a reader on the far side of a bounded channel. That is what keeps
//! this crate above the turn loop even though a keystroke here now starts one.
//!
//! # Shutdown travels back through the terminal channel
//!
//! [`crate::app::App`] ends its loop on [`crate::app::TerminalEvent::Shutdown`] and
//! on nothing else, so a screen that resolves the `app_exit` action has to *send*
//! that event rather than return a flag. The alternative — teaching the input
//! producer which key means exit — would put a key spelling back above the keymap,
//! which is the one thing the view layer's discipline forbids. The sender is
//! therefore a collaborator of the screen, and `try_send` is deliberate: a full
//! terminal channel already has 64 events queued, and blocking here would stall the
//! render loop that has to drain them.
//!
//! # Escape confirms interruption; an exit chord remains the emergency way out
//!
//! A single accidental escape must not discard a running turn. The session interrupt
//! action therefore arms a short confirmation window; a second press inside it asks
//! the driver to cancel. The live footer changes from `esc interrupt` to
//! `esc again to interrupt` without inserting a row or moving the composer. When a
//! modal or autocomplete surface owns the first press, it closes that surface and arms
//! the same running turn, so the user's second physical press still confirms
//! cancellation. The arm is transient UI state and is cleared at turn boundaries.
//!
//! The application exit chord keeps the older two-stage emergency behavior: first it
//! cancels, then it leaves unconditionally. Reading
//! "has a turn been cancelled already" off the reply identity's running state looks
//! equivalent and is not: a turn parked on a permission ask never reaches the
//! engine's interrupt check, so it stays running after an abort and the projection never
//! clears. A screen that re-derived its answer from that projection would cancel forever
//! and never leave — the same trap in a politer form. One press is therefore
//! remembered explicitly, and cleared when a new turn starts.
//!
//! For the same reason cancellation never gets to swallow the key: with no sink
//! attached, or a sink that refuses, the chord falls straight through to shutdown. A
//! user must always have a way out, so a broken collaborator costs a cancelled turn,
//! never the ability to leave.

use crate::app::{AppEvent, Component, EventResult, TerminalEvent};
use crate::keybind::{APP_EXIT, ActionComponent, Definition, is_exit_request};
use crate::views::ViewContext;
use crate::views::autocomplete::{AutocompleteStep, AutocompleteView, SlashSource};
use crate::views::editor::{EditorSignal, InputEditor};
use crate::views::external::{Clipboard, EditorRequest, ExternalError, SystemClipboard};
use crate::views::message::{Message, ScrollbarView, StatusView, TranscriptView};
use crate::views::permission::typed_character;
use crate::views::scroll::Scroller;
use crate::views::slash::{CatalogCommand, HostCommand, SlashRouter, SlashSubmission};
use crate::views::toast::{Toast, ToastLevel};
use crossterm::event::{
    Event as CrosstermEvent, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// The dialog id the skill browser reports under.
pub const SKILL_DIALOG_ID: &str = "prompt_skills";

/// The dialog id the session rename prompt reports under.
pub const SESSION_RENAME_DIALOG_ID: &str = "session.rename";

/// The dialog id used to edit one durable queued input.
pub const QUEUED_INPUT_EDIT_DIALOG_ID: &str = "queued_input.edit";

/// The id the `/undo` confirmation reports under.
///
/// `/undo` restores the worktree to the boundary before the last completed turn, so it
/// overwrites files on disk and discards anything edited since — the only action this
/// screen routes that can destroy work the model never saw. It reached the driver with
/// nothing asked, which made a mistyped `/undo` unrecoverable by any means the TUI has.
///
/// `/redo` is deliberately *not* confirmed. It reapplies the boundary the user just left
/// by confirming an undo, so it restores a state they were shown and agreed to moments
/// earlier; a second prompt for the same round trip only teaches people to press through
/// both of them.
pub const UNDO_CONFIRM_DIALOG_ID: &str = "confirm.undo";

/// Confirmation shown by `/plan` before changing the session's collaboration mode.
pub const PLAN_START_CONFIRM_DIALOG_ID: &str = "confirm.plan.start";

/// Confirmation shown before the durable plan is handed to the orchestrator.
pub const WORK_START_CONFIRM_DIALOG_ID: &str = "confirm.work.start";

/// The id the per-message action menu reports under.
pub const MESSAGE_ACTIONS_DIALOG_ID: &str = "message.actions";

/// The values that menu's rows report.
///
/// Spelled as constants rather than matched as literals at both ends because the producer and
/// the consumer are two hundred lines apart, and a typo in either would silently reduce the
/// menu to a row that closes the dialog and does nothing — which is precisely the dead
/// affordance this whole change is about.
const MESSAGE_ACTION_COPY: &str = "copy";
const MESSAGE_ACTION_REVERT: &str = "revert";

/// The id the in-dialog prompt editor reports under.
pub const EDITOR_FALLBACK_DIALOG_ID: &str = "prompt.editor.fallback";

/// The id the external-editor failure alert reports under.
pub const EDITOR_ALERT_DIALOG_ID: &str = "alert.editor";

/// Rows reserved for the reply identity when the pane can also retain transcript content.
const IDENTITY_ROWS: u16 = 1;

/// The info row's height on a `height`-row frame: [`INFO_ROWS`], or none when it costs the
/// prompt its survival floor.
///
/// Measured, not defensive. On a three-row pane the bands demand `body 1 + prompt 2 + info 1 =
/// 4`, and ratatui resolves the overflow by shrinking the prompt — so adding this row
/// unconditionally leaves a one-row composer whose only row is the spacer, which is nowhere to
/// type. [`PROMPT_MIN_ROWS`] calls two rows the least that can be typed into, and that floor
/// outranks knowing which directory you are in. The reply identity is not part of this minimum:
/// the conversation layout admits it only when two rows remain above the prompt, preserving one
/// row of transcript first.
///
/// The threshold is the sum of those minimums rather than a chosen number, so it cannot drift
/// away from the bands it protects: raise [`PROMPT_MIN_ROWS`] and this moves with it. Above it
/// the prompt is capped at a third of the frame ([`PROMPT_MAX_SHARE`]), so `1 + height/3 + 2`
/// never exceeds `height` and the row is always affordable.
///
/// A function rather than a constant because two places need the same answer — the vertical
/// split and [`welcome_tail_rows`]'s subtraction — and a frame whose tail was computed against
/// a row the split then dropped is off centre by one.
pub(crate) const fn info_rows(height: u16) -> u16 {
    if height >= 1 + PROMPT_MIN_ROWS + INFO_ROWS {
        INFO_ROWS
    } else {
        0
    }
}

/// Rows reserved for the ambient or live footer.
///
/// One, and always drawn — on the welcome screen as well as mid-conversation. Both halves of
/// that are taken from a real `opencode 1.18.18` pane rather than chosen: measured at 120x32,
/// its welcome frame carries the working directory on the frame's **last** row while the
/// composer sits centred above it, and its conversation frame carries the same row with the
/// command hints moved onto its right end. While a turn runs, the same row becomes the pulse,
/// interrupt control, context occupancy, and command hint. So the row is not a property of
/// having a conversation, and a row that appeared with the first message would move the
/// composer up by one the instant a reply landed — the objection
/// [`PROMPT_PREFERRED_ROWS`] records about the prompt's height, applied to the frame's floor.
const INFO_ROWS: u16 = 1;

/// The prompt's survival floor, its preferred floor, and the share it may grow to.
///
/// [`PROMPT_MIN_ROWS`] is the absolute floor and stays at two: one row of text plus the
/// spacer is the least that can be typed into, and it is what a pane too short to afford
/// anything else gets.
///
/// [`PROMPT_PREFERRED_ROWS`] is what the prompt asks for when the pane can pay. Four rows
/// is three rows of text plus the spacer — the same *band* height the empty prompt occupied
/// before was two, so a single-line buffer read as a one-line field rather than as a place
/// to compose a paragraph, which is what was reported twice: once for the empty welcome
/// screen and once mid-conversation.
///
/// **One value for both states, not two.** The welcome screen has no transcript competing
/// for the region, so a taller floor there costs only hint rows, and a taller floor during
/// a conversation costs transcript rows — which argues for diverging. It is still one
/// number, because the prompt would then *shrink by two rows the instant the first message
/// lands*, while the user is watching the reply they just asked for. That is the same
/// objection [`WELCOME_TAIL_MAX_ROWS`] records about the prompt's position, applied to its
/// height: a composer that changes size between an empty and a used session reads as two
/// different applications.
///
/// **Four rather than five, and the arithmetic is the reason.** On a 24-row pane — the
/// shortest common one — the fixed footer takes one row and a populated conversation may
/// reserve one reply-identity row. A four-row composer still leaves eighteen rows for the
/// transcript, while a five-row composer spends one more row without improving a
/// single-line edit. Four is the cheaper stable floor.
///
/// The cap is a third because the prompt is only ever half of a conversation: a pasted diff
/// allowed to take the whole height would evict the transcript it is about to be sent
/// against, and a prompt the user has to scroll is a smaller loss than a reply they cannot
/// see at all. The preferred floor is granted *through* that cap rather than over it — see
/// [`prompt_rows`], where that ordering is what keeps the clamp from aborting.
const PROMPT_MIN_ROWS: u16 = 2;
const PROMPT_PREFERRED_ROWS: u16 = 4;
const PROMPT_MAX_SHARE: u16 = 3;

/// The prompt's own chrome: a marker gutter, a right inset and a bottom spacer.
///
/// No border, and that is a decision rather than an omission. None of the three reference
/// composers draws one, `codex` — a ratatui composer, so the closest analogue — least of
/// all: it calls `Block::default().style(..)` with no `.borders(..)` and takes its
/// containment from insets plus a `›` glyph in a two-column gutter. A border costs two rows
/// of a band whose floor is two, so on the 20x10 pane [`prompt_rows`] exists to survive it
/// would leave zero rows for the text it frames.
///
/// Taken from `codex`: the gutter and its marker, a column of air on the right, and a blank
/// row under the text so the caret never sits on the terminal's last line. Not taken:
/// `codex`'s *top* inset, because the reply identity already separates the transcript from
/// the composer when present, while a short reply intentionally leaves its unused viewport
/// below that identity. A second fixed inset would spend a transcript row in both cases.
///
/// # The band's own surface is the third element of that containment, and it was missing
///
/// With no border, the only thing that can say "this four-row region is one box" is its
/// background. The band was filled with [`crate::views::ViewContext::text`], whose background
/// is `background_panel` — **the same colour the transcript and the welcome screen are filled
/// with**. So the three rows below the caret were painted, and painted invisible: measured on a
/// real 120x32 pane the composer read as a single row of text with nine dead rows beneath it,
/// and was reported that way twice. The rows were allocated exactly as
/// [`prompt_rows`] intends and bought nothing, because a region indistinguishable from the
/// surface above it is not a region.
///
/// It is filled with [`crate::views::ViewContext::element`] instead, the role documented as the
/// fill behind an inset element. The live status remains on the final frame row rather than
/// sharing the composer's fill: that keeps the pulse and interrupt affordance fixed while the
/// reply identity follows the response above the composer.
///
/// # The fill alone is not a box, and at full width it never was
///
/// A shared surface says "these rows are one object"; it cannot say "this object is the thing
/// you type into", because a fill that runs the frame's whole width is a *band* rather than a
/// box — it has no left or right edge for the eye to close. That is what was reported as an
/// input box indistinguishable from its surroundings, and no colour choice fixes it while the
/// region is edge to edge. [`WELCOME_COMPOSER_MAX_COLS`] keeps the empty-session
/// composer readable on a very wide terminal, while an active conversation still
/// uses its whole content column. [`SessionScreen::composer_rules`] paints the two
/// edges into the space around either form.
///
/// A fill rather than the border this constant's note rejects, because the objection to the
/// border was that it costs two rows of a two-row floor. A background costs none.
const PROMPT_GUTTER_COLS: u16 = 2;
const PROMPT_RIGHT_INSET: u16 = 1;
const PROMPT_SPACER_ROWS: u16 = 1;

/// Columns the text must keep before the chrome is dropped instead of the text.
///
/// Chrome that squeezes the buffer below this is chrome that costs more than it gives: at
/// 20 columns the gutter and inset leave 17, which still holds a phrase, while a pane
/// narrow enough to fall under this floor needs every column for the words.
const PROMPT_MIN_CONTENT_COLS: u16 = 12;

/// The marker drawn in the prompt's gutter.
const PROMPT_MARKER: &str = "›";

/// What the empty prompt says about itself.
///
/// One sentence rather than a list of keys: the band is a single row at its floor, and a
/// hint long enough to be truncated there teaches less than a short one that fits.
const PROMPT_PLACEHOLDER: &str = "ask anything, or / for commands";

/// How long the first session-interrupt action arms the second.
const INTERRUPT_CONFIRM_WINDOW_MS: u64 = 1_500;

/// What separates the info row's right-hand facts from each other.
///
/// The transcript's own `·` rather than two spaces, so the row reads as one list; the status
/// footer joins its state fields with the same glyph.
const INFO_SEPARATOR: &str = " · ";

/// The fewest columns the info row keeps for the directory before dropping a right-hand fact.
///
/// A path elided to four columns is `…uno`, which names no directory — so on a narrow pane the
/// context figure and then the key hint yield instead, and the path keeps its tail. That is the
/// opposite priority from the live footer, and deliberately: during work the footer's last
/// survivor is the interrupt affordance, while this idle row's reason to exist is saying
/// *where you are*.
const INFO_MIN_DIRECTORY_COLS: usize = 8;

/// Compact token count used by the live footer.
///
/// Upper-case suffixes match the compact work-state vocabulary used by the
/// reference surface, while one decimal keeps pressure changes visible before a
/// whole-thousand boundary.
fn compact_live_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

/// One blank column between the transcript and the ambient sidebar's rule.
///
/// Measured, not stylistic. Without it a wrapped row that used its full width ended flush
/// against the panel's `│`, and a sentence whose last word touches a vertical rule reads as
/// a sentence the panel cut off — which is exactly what a 120-column capture of a wrapped
/// notice was reported as. One column of air is the whole difference between "wrapped" and
/// "truncated" to a reader.
const SIDEBAR_GAP_COLS: u16 = 1;

/// The rows below the prompt that put the empty state's **input band** on the frame's middle.
///
/// Only ever non-zero while the transcript is empty. `head` is the welcome screen's own height
/// above the input, from [`crate::views::welcome::WelcomeView::head_rows`], and `body_max` is
/// what the body region would be with no tail at all.
///
/// # What is centred is the band, not the composite, and that is the third revision
///
/// The previous version halved the slack the *composite* had — block, strip and prompt as one
/// object — and balanced that. The arithmetic was exact and the screen was still wrong,
/// because a composite whose top nine-tenths is text puts the input near its bottom: measured
/// at 120×32 the band landed on rows 23–26 of 32 with fourteen dead rows below it, which is
/// what was reported for the third time. "Centre the input box" is a claim about the band, so
/// the band is what this balances:
///
/// ```text
/// height = above + band + tail + INFO_ROWS   (the frame, by definition)
/// below  = tail + INFO_ROWS = (height - band) / 2
///   ⟹    tail = (height - band) / 2 - INFO_ROWS
///   ⟹    above = ⌈(height - band) / 2⌉
/// ```
///
/// so the rows above the band and the rows below it differ by at most the odd row, whatever
/// the frame and whatever the band's height. That last clause is what makes growth safe: the
/// band and the tail move in opposite directions by the same amount, so a prompt growing from
/// four rows to ten stays centred the whole way.
///
/// **Every row drawn below the band is subtracted, because that is where it is drawn.**
/// [`INFO_ROWS`] is the only fixed band below the welcome composer; the reply identity
/// does not exist on this surface. Add another band below the prompt and it has to be
/// subtracted here or the centring silently drifts by its height.
///
/// # The head bounds it, and that is the only thing that can push the band off centre
///
/// The rows above the band are not free: the welcome head lives there, so the tail
/// cannot exceed `body_max - head` without clipping the wordmark. Both terms are taken and the
/// smaller wins. With a nine-row head the centring term is the smaller one from twenty-four
/// rows up — every common pane — and the head term binds only below that, where the frame
/// genuinely cannot hold both. Clipping a wordmark mid-glyph reads as a rendering fault, so on
/// those frames the band sits slightly low rather than the brand being cut.
///
/// # The body cannot be starved, and it is the head term that guarantees it
///
/// `tail <= body_max - max(head, 1)`, so the body keeps at least `max(head, 1)` rows whenever
/// `head <= body_max`, and when the head is *taller* than the region the subtraction saturates
/// to zero and the tail vanishes — the right answer, since a frame that cannot hold the head
/// has nothing to arrange. Either way `body >= 1` whenever `body_max >= 1`, which is what
/// holds at 20×10 where `Min(1)` would otherwise be starved by a `Length` tail. The `max(1)`
/// is load-bearing rather than defensive: without it a head measured as zero rows would let
/// the tail take the entire region.
///
/// A separate tail rather than moving the prompt out of the split keeps welcome
/// centring independent from the conversation layout. The tail becomes zero the moment
/// a message arrives.
///
/// This is *not* a per-keystroke reflow: the tail is a function of the frame, of the band and
/// of whether the transcript is empty. Typing into the prompt changes the band's height, and
/// the tail follows it, but nothing here re-measures per character.
fn welcome_tail_rows(empty: bool, height: u16, band: u16, body_max: u16, head: u16) -> u16 {
    if !empty {
        return 0;
    }
    let centred = (height.saturating_sub(band) / 2).saturating_sub(info_rows(height));
    let room = body_max.saturating_sub(head.max(1));
    centred.min(room)
}

/// Whether the ambient panel is drawn at all, for a `width`-column frame.
///
/// Three terms, and the middle one is the one that was missing. A panel is worth its columns
/// only when there is a transcript beside it: on the empty screen every figure it carries —
/// token spend, context used, LSP and MCP state — is zero or unresolved, so it spent a third of
/// the frame stating nothing while pushing the welcome surface and the composer off the axis
/// they are centred on. That is what was reported, and the fix is this term rather than
/// [`crate::views::SIDEBAR_MIN_WIDTH`]: the threshold still governs the used session, where the
/// panel has something to say.
///
/// Split out of [`Component::render`] because the welcome block's height is measured before the
/// vertical split that the horizontal split is carved from, so two places need the same answer
/// and neither may guess it.
const fn sidebar_drawn(sidebar_visible: bool, empty: bool, width: u16) -> bool {
    sidebar_visible && !empty && width >= crate::views::SIDEBAR_MIN_WIDTH
}

/// The left session column after the optional full-height sidebar is reserved.
const fn content_bounds(area: Rect, sidebar: bool) -> Rect {
    if !sidebar {
        return area;
    }
    Rect {
        width: area
            .width
            .saturating_sub(SIDEBAR_GAP_COLS.saturating_add(crate::views::ambient::SIDEBAR_WIDTH)),
        ..area
    }
}

/// The glyphs that close the composer's left and right edges.
///
/// Half blocks rather than `│`, because they are the crate's existing vocabulary for "this
/// region is the one in focus" — [`crate::views::message`] marks a speaking role with `▌` and
/// [`crate::views::welcome::COMPACT_BRAND`] leads with it. Drawn in the *margin* beside the
/// band rather than inside it, so the edges cost the text no columns and the band's own
/// arithmetic is untouched; a frame with no margin to spare simply has no rules, which is the
/// same degradation the panel and the wordmark already make.
const COMPOSER_LEFT_RULE: &str = "▌";
const COMPOSER_RIGHT_RULE: &str = "▐";

/// Maximum input width on the welcome surface.
///
/// The empty screen is a centred, bounded composition rather than a transcript, so a prompt
/// spanning two hundred columns reads as a page-wide divider instead of an input. Ninety-six
/// columns hold ordinary prompts and the agent/model footer without making the eye travel across
/// the terminal. Once a conversation starts the cap is removed: long edits should use all of the
/// left content column, including on frames with a sidebar.
const WELCOME_COMPOSER_MAX_COLS: u16 = 96;

/// Give the composer one column of breathing room per side and optionally cap the welcome form.
///
/// [`Component::render`] applies [`content_bounds`] before its vertical split, so `bounds`
/// cannot include sidebar or gap columns. Narrow panes keep every usable column. Wide welcome
/// panes centre a bounded composer; active conversations do not impose a prose-width cap.
const fn composer_region(bounds: Rect, welcome: bool) -> Rect {
    let margin: u16 = if bounds.width > 2 { 1 } else { 0 };
    let available = bounds.width.saturating_sub(margin.saturating_mul(2));
    let width = if welcome && available > WELCOME_COMPOSER_MAX_COLS {
        WELCOME_COMPOSER_MAX_COLS
    } else {
        available
    };
    let centring = available.saturating_sub(width) / 2;
    Rect {
        x: bounds.x.saturating_add(margin).saturating_add(centring),
        width,
        ..bounds
    }
}

/// Rows the prompt gets for `content_lines` of typed text on a `height`-row screen.
///
/// One row more than the content so the line the cursor is about to open is already
/// on screen; below the floor that extra row is what the floor supplies anyway.
///
/// # The clamp is provably safe, and that is the whole reason this is a function
///
/// `u16::clamp` **panics when its minimum exceeds its maximum**, so both bounds have to be
/// ordered before they are handed over. Two steps do that, and neither is optional:
///
/// 1. `cap` is raised to [`PROMPT_MIN_ROWS`]. `height / PROMPT_MAX_SHARE` is under two for
///    any viewport shorter than six rows, so without this the 20x10 pane a real terminal
///    reaches would abort the process.
/// 2. `floor` is lowered *to* `cap`. [`PROMPT_PREFERRED_ROWS`] is four, which exceeds the
///    cap on every viewport shorter than twelve rows — so a preferred floor granted *over*
///    the cap would move the abort from "shorter than six rows" to "shorter than twelve",
///    a far commoner size. Taking `min(cap)` after step 1 leaves `PROMPT_MIN_ROWS <= floor
///    <= cap` for every `height`, `u16::MAX` and `0` included.
///
/// The consequence is that the preferred floor is *earned*: it arrives at twelve rows and
/// above, three rows at nine to eleven, and two below that. A prompt is never more than a
/// third of the screen, whatever it would prefer.
fn prompt_rows(content_lines: usize, height: u16) -> u16 {
    let wanted = u16::try_from(content_lines)
        .unwrap_or(u16::MAX)
        .saturating_add(1);
    let cap = (height / PROMPT_MAX_SHARE).max(PROMPT_MIN_ROWS);
    let floor = PROMPT_PREFERRED_ROWS.min(cap);
    wanted.clamp(floor, cap)
}

/// The prompt band carved into a gutter, the buffer's own area, and the spacer below.
///
/// A function rather than inline `Layout` calls for the reason [`prompt_rows`] is one: every
/// subtraction here has to hold at 20x10, where the band is two rows and twenty columns, and
/// a helper is what lets that be asserted directly instead of inferred from a frame. The
/// gutter is `None` — chrome dropped rather than text squeezed — whenever the pane cannot
/// spare [`PROMPT_MIN_CONTENT_COLS`] after it, and the spacer is dropped whenever the band
/// is a single row, because a spacer that takes the only row leaves nowhere to type.
fn prompt_frame(band: Rect) -> (Option<Rect>, Rect) {
    // Clamped to the band rather than floored at one: a `max(1)` here fabricates a row the
    // band does not own, and writing into it panics inside ratatui's buffer. The three-band
    // split really does hand out a zero-row prompt — on a one-row viewport the footer takes
    // the only row — so this is a reachable input, not a defensive branch.
    let content_height = if band.height <= PROMPT_SPACER_ROWS {
        band.height
    } else {
        band.height - PROMPT_SPACER_ROWS
    };
    let content = Rect {
        height: content_height,
        ..band
    };
    let chrome = PROMPT_GUTTER_COLS.saturating_add(PROMPT_RIGHT_INSET);
    if content.width < chrome.saturating_add(PROMPT_MIN_CONTENT_COLS) {
        return (None, content);
    }
    let gutter = Rect {
        width: PROMPT_GUTTER_COLS,
        ..content
    };
    let editor = Rect {
        x: content.x + PROMPT_GUTTER_COLS,
        width: content.width - chrome,
        ..content
    };
    (Some(gutter), editor)
}

const fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerGesture {
    Transcript {
        column: u16,
        row: u16,
        dragged: bool,
    },
    Sidebar {
        dragged: bool,
    },
    Scrollbar {
        anchor: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptNotice {
    Confirm,
    Stopping,
}

/// The transcript, reply identity, composer, and live footer as one screen.
pub struct SessionScreen {
    transcript: TranscriptView,
    transcript_full: bool,
    summary_offset: usize,
    detailed_offset: usize,
    scrollbar: ScrollbarView,
    scrollbar_visible: bool,
    scrollbar_area: Option<Rect>,
    pointer: Option<PointerGesture>,
    status: StatusView,
    editor: InputEditor,
    autocomplete: AutocompleteView,
    autocomplete_area: Option<Rect>,
    slash: SlashRouter,
    welcome: crate::views::welcome::WelcomeView,
    welcome_enabled: bool,
    sidebar: crate::views::ambient::SidebarView,
    shutdown: mpsc::Sender<TerminalEvent>,
    prompts: Option<mpsc::Sender<PromptSubmission>>,
    mcp_toggles: Option<mpsc::Sender<crate::views::picker::McpToggleRequest>>,
    title: crate::views::ambient::SessionTitle,
    /// The session-name generation this screen last painted.
    ///
    /// Kept for exactly the reason [`Self::mcp_generation`] below is, and against the same
    /// failure: the name is published from the turn driver, so without a counter to compare
    /// nothing reports `redraw` and the panel keeps the frame it already had.
    title_generation: u64,
    mcp: crate::views::picker::McpProjection,
    /// The MCP generation this screen last painted.
    ///
    /// The MCP list and the sidebar both re-read [`Self::mcp`] while they draw, so they
    /// cannot disagree *within* a frame. What they could do — and did — is share one stale
    /// frame: the lifecycle worker publishes a state change and nudges the loop with a
    /// [`TerminalEvent::Wake`], and a wake repaints only if some component reports
    /// `redraw`. Nothing did, so a `◐ Connecting` row survived the connection's own
    /// 30-second timeout on both surfaces at once.
    ///
    /// One counter compared here is the whole report. Pushing the news into the dialog
    /// instead would need the push to find every open surface, and the surface a push
    /// forgets is precisely the one that goes stale.
    mcp_generation: u64,
    queued_inputs: crate::views::picker::QueuedInputProjection,
    queued_input_generation: u64,
    queue_mutations: Option<mpsc::Sender<QueuedInputMutation>>,
    work: crate::views::ambient::WorkState,
    work_generation: u64,
    /// The non-MCP halves of `§8.7`'s status census, resolved once by the host.
    ///
    /// MCP is deliberately *not* stored here: it is live, and `status_panel` reads it from
    /// [`Self::mcp`] at open time so the census and the MCP dialog cannot disagree about a
    /// server's state. These groups are static for the process — a language server's
    /// availability and the loaded plugin set do not change once the screen is up.
    census: Vec<crate::views::diagnostics::Group>,
    /// The runtime facts `§8.7`'s debug report states.
    debug: crate::views::diagnostics::DebugFacts,
    cancels: Option<mpsc::Sender<()>>,
    /// Language-server reports produced beside the loop.
    ///
    /// Drained with `try_recv` inside `handle_event`, which is the same non-blocking
    /// shape the permission bridge uses: a receiver awaited here would stop the one loop
    /// that consumes terminal input, engine events and the lease wake.
    reports: Option<mpsc::Receiver<crate::views::lsp::Report>>,
    background_executions: Option<Arc<zuno_pty::BackgroundExecutionService>>,
    background_session: Option<String>,
    /// Where the files a finished turn wrote are handed over for checking.
    edits: Option<crate::views::lsp::PendingEdits>,
    /// The files the running turn has written so far.
    ///
    /// Accumulated from the same `ToolDispatchCompleted` events the transcript renders,
    /// so what is checked is exactly what the user was shown. A second listener wired
    /// separately could disagree with the screen about what happened.
    touched: Vec<String>,
    submissions: Vec<String>,
    cancellations: usize,
    cancel_requested: bool,
    interrupt_armed_at_ms: Option<u64>,
    sidebar_visible: bool,
    /// The resolved palette and configuration, for the pickers this screen builds.
    context: ViewContext,
    /// The theme showing when the theme picker opened, for escape to put back.
    ///
    /// A whole [`crate::theme::Resolved`] rather than a name, so restoring costs no
    /// second walk of the theme's colour references and cannot fall back to something
    /// else than what was on screen.
    ///
    /// Held here and not in the picker because the picker is gone by the time its
    /// cancellation is routed: [`crate::views::dialog::DialogHost`] pops the dialog and
    /// *then* tells the base.
    theme_restore: Option<Arc<crate::theme::Resolved>>,
    /// The transcript index [`Self::message_actions`] last opened a menu for.
    ///
    /// Held for the reason [`Self::theme_restore`] is: the dialog is gone by the time its
    /// answer is routed. Cleared when the answer arrives, so a later outcome from some other
    /// dialog cannot be applied to a message the user is no longer looking at.
    message_menu: Option<usize>,
    /// The session id and original title whose rename prompt is open.
    ///
    /// The list is gone by the time the prompt answers, so this is the durable identity
    /// that connects the two stacked dialogs. The original title is retained only to
    /// re-open the prompt after an empty submission without querying storage from the
    /// view layer.
    session_rename: Option<(String, String)>,
    /// Pending memory candidate whose edit prompt is open.
    memory_edit: Option<String>,
    /// Durable input whose edit prompt is open: id, revision, and original text.
    queued_input_edit: Option<(String, i64, String)>,
    /// The user's resolved keymap, for the keybinding reference.
    ///
    /// Optional because every view test builds a screen without one, and a help view
    /// built from the shipped table instead would list the default spellings rather
    /// than the ones the user actually has.
    keymap: Option<crate::keybind::Keymap>,
    /// What the pickers offer, stated by the host.
    catalog: SessionCatalog,
    /// Dialogs asked for but not yet opened by the host.
    requested: Vec<Box<dyn crate::views::dialog::Dialog>>,
    /// Transient notices asked for but not yet raised by the host.
    ///
    /// A queue for exactly as long as it takes the host to drain it, even though the
    /// slot it feeds holds one. Handling one action can produce at most one notice, and
    /// the host takes the last; a `Vec` is what makes `drain_toasts` the same shape as
    /// `drain_dialogs` rather than a second, subtly different seam.
    toasts: Vec<Toast>,
    /// Selections the user made, for a host that applies them to the next turn.
    selections: Option<mpsc::Sender<Selection>>,
    /// The dialog currently over this screen, as [`Self::observe_modal`] last saw it.
    ///
    /// Recorded so a bracketed paste can be refused while a modal is up and so the
    /// escape that dismisses a human-input prompt can also arm interruption of the
    /// active turn. Without the latter, the modal consumes the first physical escape
    /// and the user's second press merely becomes the screen's first.
    /// [`crate::views::dialog::DialogHost`] forwards every *non-key* event to the base
    /// unconditionally — that single line is what keeps an open dialog from stalling
    /// the loop — and a paste is a non-key event, so without this the text would land
    /// in the prompt hidden behind a picker. That is the defect the host's own comment
    /// describes for keys: a modal owns the keyboard.
    modal: Option<&'static str>,
    /// The user's `scroll_speed` and `scroll_acceleration`, applied to wheel input.
    ///
    /// Held for the life of the screen rather than built per event, and that is the
    /// whole reason either key works. The curve is a function of the intervals
    /// *between* notches and the fractional carry is what survives a sub-row multiplier,
    /// so a scroller constructed inside `handle_event` would measure its first notch
    /// every time — reporting a multiplier of one forever, and rounding every
    /// `scroll_speed` under 1.0 to no movement at all. Nothing would fail loudly: a
    /// constant multiplier is a legal answer, so the defect would be invisible to any
    /// test that only asked whether the wheel moved something.
    scroller: Scroller,
    /// The monotonic origin wheel timestamps are measured from.
    ///
    /// A baseline plus an explicit `now_ms` parameter, rather than a clock read inside
    /// the curve, for the reason `KeyDispatcher::dispatch_key` takes its `Instant`: a
    /// streak that read the clock itself could only be tested by sleeping.
    started: Instant,
    /// Where [`EditorSignal::Copy`] puts the text.
    ///
    /// Injected, and `Arc` rather than `Box`, so a test can hold the same
    /// [`crate::views::external::MemoryClipboard`] the screen writes through and read it
    /// back afterwards. A process-global would make the assertion "did the copy land"
    /// order-dependent across the suite, which is the reason every other collaborator
    /// here is a field too.
    clipboard: Arc<dyn Clipboard>,
    editor_requests: Option<mpsc::Sender<EditorRequest>>,
    editor_results: Option<mpsc::Receiver<Result<Option<String>, ExternalError>>>,
}

/// One choice the user made in a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// A different `provider/model` for subsequent turns.
    Model(String),
    /// A different agent for subsequent turns.
    Agent(String),
    /// A different session to continue in.
    Session(String),
    /// A fresh lazily materialized session in the current directory.
    NewSession,
    /// Rename a session after its prompt has supplied a non-empty title.
    SessionRename { id: String, title: String },
    /// Delete a session after the list has confirmed the destructive action.
    SessionDelete(String),
    /// Cancel one running background subagent job.
    JobCancel(String),
    /// Approve one durable memory candidate.
    MemoryApply(String),
    /// Reject one durable memory candidate.
    MemoryReject(String),
    /// Undo one applied durable memory candidate.
    MemoryUndo(String),
    /// Replace candidate content and approve it in one audited operation.
    MemoryEditApply { id: String, content: String },
    /// Remove one current resident-memory entry.
    MemoryRemove {
        scope: zuno_types::MemoryScope,
        content: String,
    },
    /// A different theme.
    Theme(String),
    /// A different reasoning level for subsequent turns.
    Effort(zuno_llm::effort::ReasoningEffort),
}

/// A durable queued-input mutation that must be acknowledged after SQLite commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueuedInputMutation {
    Edit {
        id: String,
        expected_revision: i64,
        text: String,
    },
    Cancel {
        id: String,
        expected_revision: i64,
    },
}

/// A prompt-channel message. Catalog invocations stay typed until the CLI host
/// resolves their templates; plain text goes directly to the turn driver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum PromptSubmission {
    /// Ordinary model input.
    Text(String),
    /// Model input after the host resolved one or more `@` references.
    ///
    /// Kept on the prompt channel rather than a parallel attachment channel so the text
    /// and its blocks cannot be reordered across turns. `text` is the user-authored form
    /// retained for hooks and diagnostics; `content` is what the provider receives.
    Content {
        text: String,
        content: Vec<zuno_llm::event::RequestContentBlock>,
    },
    /// A catalog command plus its still-unexpanded argument tail.
    Command { name: String, arguments: String },
    /// One exact Skill selected from slash discovery before optional task text.
    Skill {
        name: String,
        source: String,
        arguments: String,
    },
    /// A session-local operation executed by the runtime host.
    Host(HostCommand),
    /// Explicitly retain a submission in the durable FIFO even if the active turn ends
    /// before the driver reads the handoff channel.
    Queue(Box<PromptSubmission>),
    /// Explicitly steer a running generation at its next safe boundary.
    ///
    /// Ordinary submissions are durable FIFO work. Keeping the override in the
    /// value prevents the user's gesture from being lost between the terminal,
    /// admission worker, durable inbox, and resumed turn.
    Steer(Box<PromptSubmission>),
}

/// What the pickers can offer, as the host resolved it.
///
/// Plain lists rather than a live query: a picker redraws on every keystroke, and a
/// surface that re-listed sessions per frame would put a database read in the render
/// path. The host states them once and restates them when they change.
#[derive(Debug, Clone, Default)]
pub struct SessionCatalog {
    /// Every model the catalog offers.
    pub models: Vec<crate::views::picker::ModelEntry>,
    /// Every agent discovery found.
    pub agents: Vec<crate::views::picker::AgentEntry>,
    /// Recent sessions.
    pub sessions: Vec<crate::views::picker::SessionEntry>,
    /// The session currently open, so its row is focused rather than looking switchable.
    pub session: Option<String>,
    /// `provider/model` currently in use, so the picker opens on it.
    pub model: Option<String>,
    /// The agent currently in use.
    pub agent: Option<String>,
    /// Whether the model in use accepts a reasoning level at all.
    ///
    /// Stated by the host from the resolved catalog, because the view cannot know it: a
    /// model's reasoning capability is a catalog fact. It gates the cycling key, so a
    /// model without reasoning gets a key that says why rather than one that relabels a
    /// control the request would not send.
    pub reasoning: bool,
    /// Canonical reasoning levels available for each `provider/model`.
    ///
    /// Kept per model so switching rows immediately changes the cycle before the next
    /// host rebuild. An absent entry falls back to the full scale only for a model whose
    /// coarse `reasoning` flag is true, which preserves catalogs that expose no variants.
    pub reasoning_efforts: BTreeMap<String, Vec<zuno_llm::effort::ReasoningEffort>>,
    /// The reasoning level in use, when one was chosen.
    pub effort: Option<zuno_llm::effort::ReasoningEffort>,
}

impl SessionScreen {
    /// A screen that requests shutdown through `shutdown` when `app_exit` resolves.
    #[must_use]
    pub fn new(context: ViewContext, shutdown: mpsc::Sender<TerminalEvent>) -> Self {
        let slash = SlashRouter::default();
        let mut transcript = TranscriptView::new(context.clone());
        transcript.set_activity_display(crate::views::message::ActivityDisplay::Summary);
        Self {
            transcript,
            transcript_full: false,
            summary_offset: 0,
            detailed_offset: 0,
            scrollbar: ScrollbarView::new(context.clone()),
            scrollbar_visible: true,
            scrollbar_area: None,
            pointer: None,
            status: StatusView::new(context.clone()),
            welcome: crate::views::welcome::WelcomeView::new(context.clone()),
            welcome_enabled: true,
            sidebar: crate::views::ambient::SidebarView::new(context.clone()),
            editor: InputEditor::new(context.clone()).with_placeholder(PROMPT_PLACEHOLDER),
            autocomplete: AutocompleteView::new(
                context.clone(),
                Box::new(SlashSource::new(slash.clone())),
            ),
            autocomplete_area: None,
            slash,
            shutdown,
            prompts: None,
            mcp_toggles: None,
            title: crate::views::ambient::SessionTitle::default(),
            title_generation: 0,
            mcp: crate::views::picker::McpProjection::default(),
            mcp_generation: 0,
            queued_inputs: crate::views::picker::QueuedInputProjection::default(),
            queued_input_generation: 0,
            queue_mutations: None,
            work: crate::views::ambient::WorkState::default(),
            work_generation: 0,
            census: Vec::new(),
            debug: crate::views::diagnostics::DebugFacts::default(),
            cancels: None,
            reports: None,
            background_executions: None,
            background_session: None,
            edits: None,
            touched: Vec::new(),
            submissions: Vec::new(),
            cancellations: 0,
            cancel_requested: false,
            interrupt_armed_at_ms: None,
            sidebar_visible: true,
            keymap: None,
            catalog: SessionCatalog::default(),
            requested: Vec::new(),
            toasts: Vec::new(),
            selections: None,
            theme_restore: None,
            message_menu: None,
            session_rename: None,
            memory_edit: None,
            queued_input_edit: None,
            modal: None,
            scroller: Scroller::new(&context.config),
            started: Instant::now(),
            // The real host clipboard, so a copy works in production without the CLI
            // constructing anything: `SystemClipboard::host` resolves the platform, the
            // installed programs and whether stdout is a terminal, and yields a
            // clipboard with no mechanisms when there is no terminal — which is also
            // what keeps the suite from spawning `xclip` or painting escape sequences
            // into captured test output. `with_clipboard` replaces it.
            clipboard: Arc::new(SystemClipboard::host()),
            editor_requests: None,
            editor_results: None,
            // Last, because the two fields above borrow it and a struct literal
            // evaluates its fields in written order.
            context,
        }
    }

    /// Open a fresh in-app conversation shell without re-entering the launch welcome page.
    ///
    /// This changes presentation only. The transcript remains empty and the host still
    /// materializes the durable session lazily on the first real model input.
    #[must_use]
    pub const fn without_welcome(mut self) -> Self {
        self.welcome_enabled = false;
        self
    }

    fn welcome_active(&self) -> bool {
        self.welcome_enabled && !self.transcript.transcript().conversation_started()
    }

    /// Install prompts a previous run submitted, and record new ones to `records`.
    ///
    /// The entries and the sink arrive together because they are two halves of one
    /// feature, and the host supplies both: `zuno-tui` names the file
    /// ([`crate::views::editor::PROMPT_HISTORY_FILE`]) but resolves no directory, so
    /// the reading and the writing both live in `crates/zuno-cli/src/cmd/tui.rs`.
    ///
    /// Only prompts typed into this editor are recorded. A prompt supplied on the
    /// command line goes through [`Self::submit_prompt`], which never touches the
    /// editor — it was not typed here, and treating it as though it were would put an
    /// unattended invocation into the list a user walks back with.
    #[must_use]
    pub fn with_prompt_history(
        mut self,
        entries: Vec<String>,
        records: mpsc::Sender<String>,
    ) -> Self {
        self.editor.load_history(entries);
        self.editor.record_history_to(records);
        self
    }

    /// Send copied text somewhere other than the host's own clipboard.
    ///
    /// Optional for the reason every other collaborator here is: the default already
    /// works, and a test needs a clipboard it can read back.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.clipboard = clipboard;
        self
    }

    /// Connect external-editor requests to a host worker and receive its results.
    #[must_use]
    pub fn with_external_editor(
        mut self,
        requests: mpsc::Sender<EditorRequest>,
        results: mpsc::Receiver<Result<Option<String>, ExternalError>>,
    ) -> Self {
        self.editor_requests = Some(requests);
        self.editor_results = Some(results);
        self
    }

    /// Forward every submitted prompt to a turn driver.
    ///
    /// A channel and not a callback for the reason the dialog set has one: a
    /// callback would run inside `handle_action`, which is the one frame a turn must
    /// not be started from — the loop that has to draw the turn's events is the
    /// caller. `try_send` for the same reason the shutdown sender uses it.
    ///
    /// Optional because a screen with no driver is still a legitimate screen — every
    /// view test builds one — and a `Sender` it could not answer would be worse.
    #[must_use]
    pub fn with_prompt_sink(mut self, prompts: mpsc::Sender<PromptSubmission>) -> Self {
        self.prompts = Some(prompts);
        self
    }

    /// Install the live MCP projection and non-blocking lifecycle request sink.
    #[must_use]
    pub fn with_mcp_control(
        mut self,
        projection: crate::views::picker::McpProjection,
        toggles: mpsc::Sender<crate::views::picker::McpToggleRequest>,
    ) -> Self {
        self.mcp_generation = projection.generation();
        self.mcp = projection;
        self.mcp_toggles = Some(toggles);
        self
    }

    /// Install the durable queued-input projection and mutation sink.
    #[must_use]
    pub fn with_queued_inputs(
        mut self,
        projection: crate::views::picker::QueuedInputProjection,
        mutations: mpsc::Sender<QueuedInputMutation>,
    ) -> Self {
        self.queued_input_generation = projection.generation();
        self.queued_inputs = projection;
        self.queue_mutations = Some(mutations);
        self
    }

    /// Install the live session-name projection the sidebar reads.
    #[must_use]
    pub fn with_session_title(mut self, title: crate::views::ambient::SessionTitle) -> Self {
        self.title_generation = title.generation();
        self.title = title;
        self
    }

    /// Install the live durable work-state projection.
    #[must_use]
    pub fn with_work_state(mut self, work: crate::views::ambient::WorkState) -> Self {
        self.work_generation = work.generation();
        self.work = work;
        self
    }

    /// Install host-projected catalog metadata without importing the catalog crate.
    #[must_use]
    pub fn with_slash_commands(
        mut self,
        commands: impl IntoIterator<Item = CatalogCommand>,
    ) -> Self {
        self.slash = SlashRouter::new(commands);
        self.autocomplete
            .set_source(Box::new(SlashSource::new(self.slash.clone())));
        self
    }

    /// Install the host's `@` candidates without teaching this leaf crate about filesystems.
    ///
    /// Completion is called from the keystroke path while the UI state is locked, so the
    /// implementation supplied here must already be bounded and must not perform a walk.
    /// The production CLI satisfies that contract with a capped index built before raw mode;
    /// tests keep using [`crate::views::autocomplete::StaticSource`].
    #[must_use]
    pub fn with_reference_source(
        mut self,
        source: Box<dyn crate::views::autocomplete::CompletionSource>,
    ) -> Self {
        self.autocomplete.set_reference_source(source);
        self
    }

    /// Let an exit chord cancel a running turn instead of leaving the application.
    ///
    /// Optional for the same reason [`Self::with_prompt_sink`] is: a screen with no
    /// driver has no turn to cancel. Without it, an exit chord leaves immediately.
    #[must_use]
    pub fn with_cancel_sink(mut self, cancels: mpsc::Sender<()>) -> Self {
        self.cancels = Some(cancels);
        self
    }

    /// How many times an exit chord has cancelled a running turn.
    ///
    /// Retained for the same reason [`Self::submissions`] is: a test should be able
    /// to tell "cancelled the turn" from "left the application" without owning the
    /// far side of either channel.
    #[must_use]
    pub const fn cancellations(&self) -> usize {
        self.cancellations
    }

    /// Report the files each finished turn wrote, for checking.
    #[must_use]
    pub fn with_edit_sink(mut self, edits: crate::views::lsp::PendingEdits) -> Self {
        self.edits = Some(edits);
        self
    }

    /// Note the files a completed call wrote, and hand them over when the turn ends.
    ///
    /// The paths come from [`TurnEvent::ToolDispatchCompleted::written_paths`], which
    /// each writing tool fills where it writes. Nothing here matches on a tool's *name*:
    /// this used to hold a `WRITING_TOOLS` list of `["edit", "write", "patch"]`, and the
    /// registry's third id is `apply_patch`, so on the models whose only writing tool is
    /// `apply_patch` — every GPT model, which sees just `read` and `apply_patch` — a
    /// successful patch never entered the set and no file was ever checked. A list that
    /// has to be kept in step with a registry by hand is the same defect waiting to
    /// happen again; a path the tool reported cannot drift.
    ///
    /// `read` needs no exclusion any more for the same reason: a tool that writes nothing
    /// reports nothing, so it cannot attribute a file's pre-existing diagnostics to this
    /// turn.
    fn observe_edits(&mut self, event: &AppEvent) {
        let AppEvent::Engine(turn) = event else {
            return;
        };
        match turn {
            zuno_engine::r#loop::TurnEvent::ToolDispatchCompleted {
                written_paths,
                is_error,
                ..
            } => {
                // A failed write changed nothing, so its diagnostics would describe the
                // file as it already was.
                if *is_error {
                    return;
                }
                for path in written_paths {
                    let path = path.trim();
                    if !path.is_empty() && !self.touched.iter().any(|seen| seen == path) {
                        self.touched.push(path.to_owned());
                    }
                }
            }
            zuno_engine::r#loop::TurnEvent::TurnCompleted { .. }
            | zuno_engine::r#loop::TurnEvent::TurnInterrupted { .. }
            | zuno_engine::r#loop::TurnEvent::TurnFailed { .. } => {
                if self.touched.is_empty() {
                    return;
                }
                let batch = std::mem::take(&mut self.touched);
                if let Some(edits) = self.edits.as_ref() {
                    edits.merge(batch);
                }
            }
            _ => {}
        }
    }

    fn observe_session_materialized(&mut self, event: &AppEvent) -> EventResult {
        let AppEvent::Engine(zuno_engine::r#loop::TurnEvent::SessionMaterialized {
            session_id,
            title,
        }) = event
        else {
            return EventResult::IGNORED;
        };
        self.catalog.session = Some(session_id.clone());
        self.background_session = Some(session_id.clone());
        if !self
            .catalog
            .sessions
            .iter()
            .any(|session| session.id == *session_id)
        {
            self.catalog.sessions.insert(
                0,
                crate::views::picker::SessionEntry {
                    id: session_id.clone(),
                    title: title.clone(),
                    when: String::from("now"),
                },
            );
        }
        EventResult::REDRAW
    }

    /// Take language-server reports from `reports` as they arrive.
    ///
    /// Optional for the same reason the other two sinks are: a screen with no host has
    /// nothing querying language servers, and a receiver it could never be fed would be
    /// worse than none.
    #[must_use]
    pub fn with_diagnostics_source(
        mut self,
        reports: mpsc::Receiver<crate::views::lsp::Report>,
    ) -> Self {
        self.reports = Some(reports);
        self
    }

    /// Install the census groups and runtime facts `§8.7`'s two panels report.
    ///
    /// One setter for both because a host that resolved one and not the other would ship a
    /// half-populated troubleshooting surface, and the omission would look like a fact
    /// that failed to load rather than one nobody wired.
    ///
    /// A setter rather than a builder because several of these facts come from the turn
    /// host, which does not exist until after the screen is constructed — the same reason
    /// `status_mut().describe` is a setter.
    pub fn set_diagnostics(
        &mut self,
        census: Vec<crate::views::diagnostics::Group>,
        debug: crate::views::diagnostics::DebugFacts,
    ) {
        self.census = census;
        self.debug = debug;
    }

    /// Drain every report that has arrived.
    fn drain_reports(&mut self) -> EventResult {
        let mut drained = Vec::new();
        if let Some(reports) = self.reports.as_mut() {
            while let Ok(report) = reports.try_recv() {
                drained.push(report);
            }
        }
        if drained.is_empty() {
            return EventResult::IGNORED;
        }
        for report in drained {
            self.report_diagnostics(report);
        }
        EventResult::REDRAW
    }

    fn drain_editor_results(&mut self) -> EventResult {
        let mut drained = Vec::new();
        if let Some(results) = self.editor_results.as_mut() {
            while let Ok(result) = results.try_recv() {
                drained.push(result);
            }
        }
        if drained.is_empty() {
            return EventResult::IGNORED;
        }
        for result in drained {
            match result {
                Ok(Some(text)) => self.editor.set_text(&text),
                Ok(None) => {}
                // An alert rather than the transcript line this used to be, and rather
                // than a toast. `$EDITOR` failures carry the child's own diagnostic —
                // a path, an exit status, sometimes several lines — which a five-second
                // corner notice can only truncate, and the transcript scrolls it away
                // behind whatever the turn prints next. The user has to read this one to
                // know whether their draft survived, so it waits to be dismissed.
                Err(error) => {
                    self.requested
                        .push(Box::new(crate::views::basics::AlertDialog::new(
                            self.context.clone(),
                            EDITOR_ALERT_DIALOG_ID,
                            "External editor failed",
                            format!("{error}\n\nThe prompt is unchanged."),
                        )))
                }
            }
        }
        EventResult::REDRAW
    }

    fn request_external_editor(&mut self) -> EventResult {
        let Some(requests) = self.editor_requests.as_ref() else {
            // A prompt dialog rather than the dead end this used to be. The action means
            // "give me more room for this text"; answering it with one line saying no
            // leaves the request unserved. The dialog serves it through the *same* sink
            // the real editor's result uses — `self.editor.set_text` in
            // `drain_editor_results` above — so the two routes cannot diverge.
            self.requested
                .push(Box::new(crate::views::basics::PromptDialog::new(
                    self.context.clone(),
                    EDITOR_FALLBACK_DIALOG_ID,
                    "Edit prompt (no $EDITOR available)",
                    self.editor.text(),
                )));
            return EventResult::REDRAW;
        };
        let request = EditorRequest::new(self.editor.text());
        if let Err(error) = requests.try_send(request) {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "an external editor is already running",
                mpsc::error::TrySendError::Closed(_) => "the external editor worker has stopped",
            };
            self.transcript
                .transcript_mut()
                .push(Message::notice(reason));
        }
        EventResult::REDRAW
    }

    /// Append a language-server report to the transcript.
    ///
    /// A method rather than letting the host reach through `transcript_mut`, so every
    /// diagnostics producer uses the same durable-looking transcript representation.
    pub fn report_diagnostics(&mut self, report: crate::views::lsp::Report) {
        self.transcript
            .transcript_mut()
            .push(Message::diagnostics(report));
    }

    /// The transcript, for a host that appends locally composed messages.
    pub const fn transcript_mut(&mut self) -> &mut TranscriptView {
        &mut self.transcript
    }

    #[must_use]
    pub fn with_background_executions(
        mut self,
        service: Arc<zuno_pty::BackgroundExecutionService>,
        session_id: impl Into<String>,
    ) -> Self {
        self.background_executions = Some(service);
        self.background_session = Some(session_id.into());
        self
    }

    #[must_use]
    pub const fn transcript_full(&self) -> bool {
        self.transcript_full
    }

    fn toggle_transcript(&mut self) -> EventResult {
        if self.transcript_full {
            self.detailed_offset = self.transcript.offset();
            self.transcript
                .set_activity_display(crate::views::message::ActivityDisplay::Summary);
            self.transcript.set_offset(self.summary_offset);
            self.transcript_full = false;
        } else {
            self.summary_offset = self.transcript.offset();
            let following = self.transcript.is_following();
            self.transcript
                .set_activity_display(crate::views::message::ActivityDisplay::Detailed);
            self.transcript.set_offset(self.detailed_offset);
            if following {
                self.transcript.follow();
            }
            self.transcript_full = true;
        }
        self.sidebar.clear_selection();
        EventResult::REDRAW
    }

    /// The welcome screen, for the host that resolves the facts it states.
    pub const fn welcome_mut(&mut self) -> &mut crate::views::welcome::WelcomeView {
        &mut self.welcome
    }

    /// Supply the resolved keymap the keybinding reference is built from.
    #[must_use]
    pub fn with_keymap(mut self, keymap: crate::keybind::Keymap) -> Self {
        self.keymap = Some(keymap);
        self
    }

    /// State what the pickers offer.
    #[must_use]
    pub fn with_catalog(mut self, catalog: SessionCatalog) -> Self {
        self.status.set_model_names(
            catalog
                .models
                .iter()
                .map(|model| (model.id.clone(), model.name.clone())),
        );
        self.welcome.facts_mut().agent = catalog.agent.clone();
        self.welcome.facts_mut().model = catalog.model.clone();
        if let Some(agent) = catalog.agent.as_deref() {
            self.status.set_configured_agent(agent);
        }
        if let Some(model) = catalog.model.as_deref() {
            self.status.set_configured_model(model);
        }
        self.status.set_effort(catalog.effort);
        self.welcome.facts_mut().reasoning =
            catalog.effort.map(|effort| effort.as_str().to_owned());
        self.catalog = catalog;
        self
    }

    /// Forward every picker choice to a host that can apply it.
    ///
    /// Optional and `try_send`, for the same reasons the prompt sink is: a screen with
    /// no host is still a legitimate screen, and blocking here would stall the loop
    /// that has to draw the choice.
    #[must_use]
    pub fn with_selection_sink(mut self, selections: mpsc::Sender<Selection>) -> Self {
        self.selections = Some(selections);
        self
    }

    /// What the pickers offer, mutably, for a host that restates it.
    pub const fn catalog_mut(&mut self) -> &mut SessionCatalog {
        &mut self.catalog
    }

    /// The reply identity, for the host that states the configured agent and model.
    pub const fn status_mut(&mut self) -> &mut StatusView {
        &mut self.status
    }

    /// The ambient panel, for the host that resolves its services.
    pub const fn sidebar_mut(&mut self) -> &mut crate::views::ambient::SidebarView {
        &mut self.sidebar
    }

    /// Whether the ambient panel is drawn when the terminal is wide enough.
    #[must_use]
    pub const fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    /// Every text the user has submitted, oldest first.
    ///
    /// Retained as well as forwarded: a screen with no driver attached still has to
    /// show that the submission was received, and a test asserting what the user
    /// sent should not have to own the other end of a channel to read it.
    #[must_use]
    pub fn submissions(&self) -> &[String] {
        &self.submissions
    }

    /// Submit `text` as though the user had typed and sent it.
    ///
    /// The one path a host needs for a prompt supplied on the command line. It goes
    /// through the same code an interactive submission does so that an unattended
    /// invocation and a typed one cannot diverge — which is exactly the divergence a
    /// host that pushed to the transcript itself would introduce.
    pub fn submit_prompt(&mut self, text: impl Into<String>) {
        self.submit(text.into(), false);
    }

    /// Hand `text` to the driver, or say in the transcript that nobody took it.
    ///
    /// Reporting the refusal is the point. A prompt that vanished because the driver
    /// had gone away, rendered identically to one accepted, is the defect where "no
    /// results" and "cannot see the data" look the same.
    fn submit(&mut self, text: String, force: bool) {
        match self.slash.resolve(&text) {
            SlashSubmission::Prompt(prompt) => {
                let submission = PromptSubmission::Text(prompt.clone());
                let submission = if force {
                    PromptSubmission::Steer(Box::new(submission))
                } else {
                    submission
                };
                self.submit_to_driver(prompt, submission);
            }
            SlashSubmission::UiAction(action) => {
                self.dispatch_action(action);
            }
            SlashSubmission::Catalog { command, arguments } => self.submit_to_driver(
                text,
                PromptSubmission::Command {
                    name: command,
                    arguments,
                },
            ),
            SlashSubmission::Skill {
                name,
                source,
                arguments,
            } => self.submit_to_driver(
                text,
                PromptSubmission::Skill {
                    name,
                    source,
                    arguments,
                },
            ),
            SlashSubmission::Host(HostCommand::Undo) => {
                self.requested.push(Box::new(
                    crate::views::basics::ConfirmDialog::new(
                        self.context.clone(),
                        UNDO_CONFIRM_DIALOG_ID,
                        "Undo the last turn",
                        "The worktree is restored to the boundary before the last completed \
                         turn. Anything edited since is discarded and cannot be recovered.",
                    )
                    .with_labels("Restore", "Keep"),
                ));
            }
            SlashSubmission::Host(HostCommand::Plan) => {
                if self.plan_mode_active() {
                    self.request_start_work();
                } else {
                    self.request_start_plan();
                }
            }
            SlashSubmission::Host(HostCommand::StartPlan) => {
                self.select_collaboration_agent("plan");
            }
            SlashSubmission::Host(HostCommand::StartWork) => {
                self.request_start_work();
            }
            SlashSubmission::Host(HostCommand::Stop(None)) => {
                let dialog = self.background_view();
                self.request(dialog);
            }
            SlashSubmission::Host(HostCommand::Stop(Some(execution_id))) => {
                self.cancel_background(&execution_id);
            }
            SlashSubmission::Host(command) => {
                self.submit_to_driver(text, PromptSubmission::Host(command));
            }
            SlashSubmission::Unknown(name) => {
                let shown = if name.is_empty() {
                    String::from("/")
                } else {
                    format!("/{name}")
                };
                self.toasts.push(Toast::warning_for(
                    format!(
                        "unknown command `{shown}`; type `/` to browse commands or press ctrl+p"
                    ),
                    crate::views::toast::TOAST_TTL,
                ));
            }
        }
    }

    fn plan_mode_active(&self) -> bool {
        self.catalog.agent.as_deref() == Some("plan")
    }

    fn select_collaboration_agent(&mut self, agent: &str) {
        if self.catalog.agent.as_deref() == Some(agent) {
            self.toasts.push(Toast::info(if agent == "plan" {
                "Plan mode is already active"
            } else {
                "Work mode is already active"
            }));
            return;
        }
        let (notice, level) = self.commit_selection(Selection::Agent(agent.to_owned()));
        if level == ToastLevel::Success {
            self.catalog.agent = Some(agent.to_owned());
            self.status.set_configured_agent(agent);
            self.sidebar.ambient_mut().agent = Some(agent.to_owned());
            self.welcome.facts_mut().agent = Some(agent.to_owned());
        }
        self.toasts.push(Toast::new(level, notice));
    }

    fn request_start_plan(&mut self) {
        self.requested.push(Box::new(
            crate::views::basics::ConfirmDialog::new(
                self.context.clone(),
                PLAN_START_CONFIRM_DIALOG_ID,
                "Start Plan mode",
                "Switch to the read-only plan agent. It may inspect the repository, ask questions, and update the durable plan, but cannot modify product files.",
            )
            .with_labels("Start plan", "Keep working"),
        ));
    }

    fn request_start_work(&mut self) {
        let Some(plan) = self.work.snapshot().plan else {
            self.toasts.push(Toast::warning(
                "no durable plan is ready; use /start-plan and let the plan agent create one",
            ));
            return;
        };
        let completed = plan
            .steps
            .iter()
            .filter(|step| step.status == "completed")
            .count();
        self.requested.push(Box::new(
            crate::views::basics::ConfirmDialog::new(
                self.context.clone(),
                WORK_START_CONFIRM_DIALOG_ID,
                "Start implementation",
                format!(
                    "Use the durable plan ‘{}’ (revision {}, {completed}/{} steps already complete) and switch to the orchestrator?",
                    plan.title,
                    plan.revision,
                    plan.steps.len(),
                ),
            )
            .with_labels("Start work", "Keep planning"),
        ));
    }

    fn submit_to_driver(&mut self, shown: String, submission: PromptSubmission) {
        let submission = if self.status.is_running()
            && !matches!(
                submission,
                PromptSubmission::Queue(_) | PromptSubmission::Steer(_)
            ) {
            PromptSubmission::Queue(Box::new(submission))
        } else {
            submission
        };
        self.transcript
            .transcript_mut()
            .push(Message::user(shown.clone()));
        if let Some(prompts) = self.prompts.as_ref() {
            let tracks_model_turn = matches!(
                submission,
                PromptSubmission::Text(_)
                    | PromptSubmission::Content { .. }
                    | PromptSubmission::Command { .. }
                    | PromptSubmission::Skill { .. }
                    | PromptSubmission::Queue(_)
                    | PromptSubmission::Steer(_)
            );
            match prompts.try_send(submission) {
                Ok(()) => {
                    if tracks_model_turn {
                        self.mark_turn_accepted();
                    }
                }
                Err(error) => {
                    let reason = match error {
                        mpsc::error::TrySendError::Full(_) => "the durable input queue is full",
                        mpsc::error::TrySendError::Closed(_) => "the turn driver has stopped",
                    };
                    self.transcript
                        .transcript_mut()
                        .push(Message::user(format!("not sent: {reason}")));
                }
            }
        }
        self.submissions.push(shown);
    }

    fn refresh_autocomplete(&mut self) {
        let text = self.editor.text();
        let cursor = self.editor.cursor();
        let before = text
            .split('\n')
            .take(cursor.line)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>()
            .saturating_add(cursor.column);
        self.autocomplete.refresh(&text, before);
    }

    fn complete_autocomplete(&mut self) -> EventResult {
        let text = self.editor.text();
        let Some((completed, cursor)) = self.autocomplete.complete(&text) else {
            return EventResult::IGNORED;
        };
        self.editor.apply_completion(&completed, cursor);
        self.refresh_autocomplete();
        EventResult::REDRAW
    }

    fn autocomplete_step(&mut self, action: &'static str) -> EventResult {
        let Some(definition) = crate::keybind::definition(action) else {
            return EventResult::IGNORED;
        };
        match self.autocomplete.handle_action(definition) {
            AutocompleteStep::Ignored => EventResult::IGNORED,
            AutocompleteStep::Redraw => EventResult::REDRAW,
            AutocompleteStep::Complete => self.complete_autocomplete(),
        }
    }

    /// Put `text` into the prompt, and submit nothing.
    ///
    /// Submitting nothing is the whole behaviour being bought here. A real-terminal
    /// session before this existed turned an eight-line paste into eight turns and
    /// filled the transcript with `not sent: a turn is already running`, because
    /// without bracketed paste each newline was a separate key that resolved to
    /// `input_submit`.
    fn paste(&mut self, text: &str) -> EventResult {
        if let Some(dialog) = self.modal {
            // Refused rather than swallowed silently: a picker's filter box cannot take
            // pasted text — `Dialog::handle_typed` receives a key, not a string — and a
            // paste that vanished with nothing said is indistinguishable from a broken
            // terminal. The notice is behind the dialog and reads once it closes, which
            // is when the user can act on it.
            self.transcript
                .transcript_mut()
                .push(Message::notice(format!(
                    "paste ignored: `{dialog}` is open and owns the keyboard"
                )));
            return EventResult::REDRAW;
        }
        if self.editor.insert_paste(text) == EditorSignal::None {
            return EventResult::IGNORED;
        }
        self.refresh_autocomplete();
        EventResult::REDRAW
    }

    /// Insert whatever the clipboard holds, or say why it could not be read.
    ///
    /// The `input_paste` binding, for terminals that deliver a paste chord as an
    /// ordinary key rather than as a bracketed paste. A bracketed paste arrives as an
    /// event and never reaches here.
    ///
    /// Reporting the refusal is the point, and it is what makes
    /// [`Clipboard::read`]'s deliberate error worth returning: the binding used to fall
    /// into a bare redraw, so pressing it did nothing and said nothing.
    fn paste_from_clipboard(&mut self) -> EventResult {
        // The three outcomes are not one grade: an unsupported kind and an empty clipboard
        // are refusals the user can act on, while a clipboard that errored is a failure —
        // `§11.5` gives those different colours, and the copy path beside this one already
        // makes exactly that distinction with its toasts.
        let (level, notice) = match self.clipboard.read() {
            Ok(Some(content)) if content.is_image() => (
                ToastLevel::Warning,
                String::from(
                    "the clipboard holds an image; pasting an attachment is not supported yet",
                ),
            ),
            Ok(Some(content)) => return self.paste(&content.data),
            Ok(None) => (
                ToastLevel::Warning,
                String::from("nothing to paste: the clipboard is empty"),
            ),
            Err(error) => (ToastLevel::Error, format!("paste failed: {error}")),
        };
        self.transcript
            .transcript_mut()
            .push(Message::noticed(level, notice));
        EventResult::REDRAW
    }

    /// Put `text` on the clipboard, and raise a toast saying what happened.
    ///
    /// Both outcomes are reported, not just the failure. A copy key that paints nothing
    /// teaches the user the binding is broken, so "it worked" and "it did not" have to
    /// be told apart on screen — the same reason [`Self::submit`] reports a refused
    /// prompt and [`Self::adopt`] reports a selection nothing listened to.
    ///
    /// A toast rather than the transcript, which is where this used to go. Two things
    /// were wrong with a transcript line. It is *permanent*: `copied 41 characters`
    /// stays in the conversation forever and is then exported and re-read as though the
    /// user had said it. And it is *invisible when it matters* — a copy made while a
    /// picker is open lands behind the modal, so the one keystroke whose feedback the
    /// user is actively waiting for is the one they cannot see. `§11.4` puts the toast
    /// above the dialog for exactly this case, and `§6.1` names copy feedback as the
    /// example of a fact that must not interrupt the input flow.
    ///
    /// Not the reply identity or footer either: neither is durable notification history,
    /// and a notice pinned there would still be claiming a copy minutes later.
    fn copy(&mut self, text: String) -> EventResult {
        // An empty buffer with nothing selected is not a copy, and writing the empty
        // string would destroy whatever the user already had on their clipboard.
        self.toasts.push(if text.is_empty() {
            // `warning`, not `error`: nothing failed, and there is something the user can
            // do about it. `§11.5` reserves `error` for a failure.
            Toast::warning("nothing to copy: the prompt is empty and no text is selected")
        } else {
            match self.clipboard.write(&text) {
                Ok(()) => Toast::success(format!(
                    "copied {} characters to the clipboard",
                    text.chars().count()
                )),
                Err(error) => Toast::error(format!("copy failed: {error}")),
            }
        });
        EventResult::REDRAW
    }
}

impl Component for SessionScreen {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let now_ms = self.now_ms();
        self.prune_interrupt_at(now_ms);
        self.autocomplete_area = None;
        // Recorded with the servers it belongs to, so the generation this frame claims to
        // have painted is the generation it did paint. See `Self::mcp_generation`.
        let (generation, servers) = self.mcp.observe();
        self.mcp_generation = generation;
        self.sidebar.ambient_mut().mcp = servers
            .iter()
            .map(crate::views::picker::McpServer::service)
            .collect();
        let (work_generation, work) = self.work.observe();
        self.work_generation = work_generation;
        self.sidebar.ambient_mut().work = work;
        if self.transcript_full {
            self.sidebar.forget_hit_targets();
            crate::views::fill(frame.buffer_mut(), area, self.context.surface());
            let [header, body, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(area);
            ratatui::widgets::Widget::render(
                ratatui::widgets::Paragraph::new(crate::views::padded(
                    " Full transcript",
                    header.width,
                    self.context.title(),
                )),
                header,
                frame.buffer_mut(),
            );
            let (main, scrollbar) = if self.scrollbar_visible && body.width > 1 {
                let [main, scrollbar] =
                    Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(body);
                (main, Some(scrollbar))
            } else {
                (body, None)
            };
            self.scrollbar_area = scrollbar;
            self.transcript.render(frame, main);
            if let Some(scrollbar) = scrollbar {
                self.scrollbar.total = self.transcript.content_height();
                self.scrollbar.viewport = self.transcript.viewport_height();
                self.scrollbar.offset = self.transcript.offset();
                self.scrollbar.render(frame, scrollbar);
            }
            ratatui::widgets::Widget::render(
                ratatui::widgets::Paragraph::new(crate::views::padded(
                    " Ctrl+T / Esc back · scroll to inspect durable history and live activity",
                    footer.width,
                    self.context.muted(),
                )),
                footer,
                frame.buffer_mut(),
            );
            return;
        }
        let empty = self.welcome_active();
        // Split the whole frame first. The transcript, reply identity, prompt, and footer
        // share one left column, so none of those bands can run underneath the sidebar.
        let sidebar_drawn = sidebar_drawn(self.sidebar_visible, empty, area.width);
        let (content, gap, aside) = if sidebar_drawn {
            let content = content_bounds(area, true);
            let gap = Rect {
                x: content.x.saturating_add(content.width),
                width: SIDEBAR_GAP_COLS,
                ..area
            };
            let aside = Rect {
                x: gap.x.saturating_add(gap.width),
                width: crate::views::ambient::SIDEBAR_WIDTH,
                ..area
            };
            (content, Some(gap), Some(aside))
        } else {
            (area, None, None)
        };
        if let Some(gap) = gap {
            crate::views::fill(frame.buffer_mut(), gap, self.context.surface());
        }
        let (prompt_band, welcome_tail) = self.prompt_and_tail(content.width, content.height);
        let info_height = info_rows(content.height);
        let transcript_width = if !empty && self.scrollbar_visible && content.width > 1 {
            content.width - 1
        } else {
            content.width
        };
        // The identity belongs to the reply, not the composer. A short reply therefore
        // keeps only the rows it actually needs and leaves the remaining viewport below
        // its identity. Once the transcript consumes the available pane, that spare run
        // reaches zero and the same identity row becomes sticky immediately above the
        // composer. The welcome screen carries the configured launch identity in its head,
        // but does not allocate the sticky reply-identity band before a reply exists.
        let (body, identity, spacer, prompt, info) = if empty {
            let [body, prompt, spacer, info] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(prompt_band),
                Constraint::Length(welcome_tail),
                Constraint::Length(info_height),
            ])
            .areas(content);
            (body, Rect::default(), spacer, prompt, info)
        } else {
            let available = content
                .height
                .saturating_sub(prompt_band.saturating_add(info_height));
            let identity_rows = u16::from(self.status.has_identity() && available > IDENTITY_ROWS);
            let capacity = available.saturating_sub(identity_rows);
            let measured = u16::try_from(self.transcript.measure_content_height(transcript_width))
                .unwrap_or(u16::MAX);
            let transcript_rows = measured.max(1).min(capacity);
            let spacer_rows = capacity.saturating_sub(transcript_rows);
            let [body, identity, spacer, prompt, info] = Layout::vertical([
                Constraint::Length(transcript_rows),
                Constraint::Length(identity_rows),
                Constraint::Length(spacer_rows),
                Constraint::Length(prompt_band),
                Constraint::Length(info_height),
            ])
            .areas(content);
            (body, identity, spacer, prompt, info)
        };

        let (main, scrollbar) = if !empty && self.scrollbar_visible && body.width > 1 {
            let [main, scrollbar] =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(body);
            (main, Some(scrollbar))
        } else {
            (body, None)
        };
        self.scrollbar_area = scrollbar;

        // The transcript owns this region as soon as there is anything to show, so the
        // welcome screen can never hide content — it only fills rows that would
        // otherwise be blank.
        if empty {
            self.welcome.render(frame, main);
            // Both, not either: the welcome screen is in force because no *turn* has
            // happened, which says nothing about whether the session has already had to
            // report something — a theme that fell back or a prompt history it could not
            // read are pushed before frame one. Drawing only the welcome screen would
            // make those warnings unreachable, and drawing only the transcript is the
            // bug this pair replaces.
            self.render_session_notices(frame, main, area.height);
            // *After* those notices, because drawing them goes through the transcript and so
            // records their rows as click targets. `message_actions` refuses a `Role::System`
            // message anyway, but the map is retracted rather than relied on to be harmless:
            // the geometry of a two-row notice region has nothing to do with where the
            // transcript's own rows land once a conversation starts.
            self.transcript.forget_hit_targets();
        } else {
            self.transcript.render(frame, main);
            if let Some(scrollbar) = scrollbar {
                self.scrollbar.total = self.transcript.content_height();
                self.scrollbar.viewport = self.transcript.viewport_height();
                self.scrollbar.offset = self.transcript.offset();
                self.scrollbar.render(frame, scrollbar);
            }
        }

        // Both the panel and the footer read the transcript's single accumulator rather than
        // folding the provider stream again, which is what keeps the two context figures on
        // screen from ever disagreeing.
        //
        // Refreshed on *every* frame, and that is a fix rather than a move. It used to happen
        // inside the branch that draws the panel, so on a frame with no panel the ambient facts
        // kept whatever the last panel-bearing frame left there. That was invisible while the
        // panel was their only reader; the info row reads `context_used` as well, so at 80
        // columns it would have reported a figure from an unrelated frame — or, on a session
        // that never drew a panel, nothing at all.
        // Read here with the token figures, and outside the `if let Some(aside)` below for
        // the reason recorded there: facts refreshed only on a panel-bearing frame keep
        // whatever an unrelated frame left behind.
        let (title_generation, title) = self.title.observe();
        self.title_generation = title_generation;
        let ambient = self.sidebar.ambient_mut();
        ambient.title = title;
        ambient.tokens = self.transcript.transcript().tokens();
        ambient.usage_state = self.transcript.transcript().usage_state();
        ambient.failed_turns = self.transcript.transcript().failed_turns();
        ambient.context = self.transcript.transcript().context_window();
        let loaded_skills = self.transcript.loaded_skills();
        let mut skill_name_counts = BTreeMap::<String, usize>::new();
        for skill in &ambient.skills {
            *skill_name_counts.entry(skill.name.clone()).or_default() += 1;
        }
        for skill in &mut ambient.skills {
            let exact = crate::views::message::LoadedSkillIdentity {
                name: skill.name.clone(),
                source: Some(skill.source.clone()),
            };
            let unique_name = skill_name_counts.get(&skill.name) == Some(&1);
            let by_unique_name = crate::views::message::LoadedSkillIdentity {
                name: skill.name.clone(),
                source: None,
            };
            skill.loaded = loaded_skills.contains(&exact)
                || (unique_name && loaded_skills.contains(&by_unique_name));
        }
        if let Some(aside) = aside {
            self.sidebar.render(frame, aside);
        } else {
            // The panel's click targets are frame geometry, so the frame that stops drawing
            // it is the frame that has to retract them — otherwise the sidebar toggle hides
            // the panel and its old rows keep swallowing clicks on the transcript beneath.
            self.sidebar.forget_hit_targets();
        }

        // Either way the centring band is painted, so the frame has one background from top to
        // bottom. Unpainted it kept ratatui's `Color::Reset` and rendered as the *terminal's*
        // background, which put a colour seam under the composer on any theme whose panel is
        // not the user's terminal colour — and made "where does the band end" a question the
        // frame could not be asked, since an unpainted row is indistinguishable from a
        // deliberately plain one. See `the_prompt_band_is_centred_on_the_frame`, which locates
        // the band's bottom edge by exactly that difference.
        //
        // Empty means these rows carry the far half of the welcome surface — the lead line, the
        // tip and the hint grid — which fills them itself. The `empty` guard is the same one
        // the head is drawn under, and it is what stops a hint block from surviving under a
        // transcript.
        crate::views::fill(frame.buffer_mut(), spacer, self.context.surface());
        if empty {
            // The full frame width, and so is the head's region: the two halves of this surface
            // centre their rows independently, so both have to be handed the same measure or the
            // wordmark and the lead line land on different axes. That used to mean subtracting
            // the panel's columns from both — measured at 120x32 with the panel drawn, the brand
            // began at column 25 and `type / for commands` at column 39, fourteen columns apart
            // and reading as two unrelated blocks. The panel is no longer drawn on this screen
            // at all (see `sidebar_drawn`), so the shared measure is simply the frame.
            self.welcome.render_foot(frame, spacer);
        }
        // The composer's two rows are narrower than the frame, so the columns beside them belong
        // to the body surface and are painted with it *first* — an unpainted margin keeps
        // ratatui's `Color::Reset` and renders as the terminal's own background, which is the
        // colour seam the centring band's fill exists to avoid, reintroduced sideways.
        crate::views::fill(frame.buffer_mut(), prompt, self.context.surface());
        crate::views::fill(frame.buffer_mut(), identity, self.context.surface());
        let composer = composer_region(prompt, empty);
        // The whole band is painted next, so the spacer row and the right inset carry the
        // prompt's own background rather than whatever the previous frame left there. `element`
        // rather than `text`: they differ only in background, and `text`'s is the surface the
        // transcript already uses, which is why four allocated rows read as one. See
        // `PROMPT_GUTTER_COLS`.
        crate::views::fill(frame.buffer_mut(), composer, self.context.element());
        self.status.render(frame, identity);
        self.composer_rules(frame, prompt, composer);
        self.render_info(frame, info, now_ms);
        let (gutter, buffer) = prompt_frame(composer);
        if let Some(gutter) = gutter {
            crate::views::editor::PromptGutter::new(self.context.clone(), PROMPT_MARKER.to_owned())
                .render(frame, gutter);
        }
        self.editor.render(frame, buffer);
        // Last, and over `main` rather than inside the split above: the popup is a floating
        // layer, so opening it cannot reflow the transcript. It owns its own geometry —
        // see `AutocompleteView::overlay_frame`.
        self.autocomplete_area = self.autocomplete.overlay_frame(main);
        if let Some(overlay) = self.autocomplete_area {
            self.autocomplete.render(frame, overlay);
        }
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        // A bracketed paste is one event carrying the whole block, so it goes straight
        // to the editor and resolves to no action at all. That is the point: before
        // bracketed paste was enabled the same paste arrived as individual keys, and
        // every newline among them resolved to `input_submit`.
        if !self.transcript_full
            && let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Paste(text))) = event
        {
            return self.paste(text);
        }
        // A printable key resolves to no action, so the dispatcher forwards it here
        // and the screen is what routes it into the prompt. Without this the editor
        // could not be typed into at all — see `permission::typed_character`, the
        // same seam the reject box uses.
        if let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Key(key))) = event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && !self.transcript_full
            && let Some(character) = typed_character(key)
        {
            self.editor.insert_char(character);
            self.refresh_autocomplete();
            return EventResult::REDRAW;
        }
        // A wheel notch is the one terminal event the transcript acts on. Merged rather
        // than returned early, so the drain below still runs on a scroll — see its
        // comment: an event that skips it can be the last event the loop ever sees.
        let wheel = match event {
            AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Mouse(mouse))) => {
                self.handle_mouse(mouse, self.now_ms())
            }
            _ => EventResult::IGNORED,
        };
        let interrupt = self.observe_interrupt(event, self.now_ms());
        // Drained on every event rather than only on a wake, for the reason the
        // permission bridge pumps on every event: a dropped nudge must not leave a
        // verdict the user is waiting for sitting in a channel forever.
        self.observe_edits(event);
        let projections = wheel
            .merge(interrupt)
            .merge(self.observe_session_materialized(event))
            .merge(self.observe_session_title())
            .merge(self.observe_mcp())
            .merge(self.observe_queued_inputs())
            .merge(self.observe_work_state())
            .merge(self.drain_editor_results())
            .merge(self.drain_reports());
        if self.suppress_live_event_while_cancelling(event) {
            return projections;
        }
        projections
            .merge(self.transcript.handle_event(event))
            .merge(self.status.handle_event(event))
    }
}

impl SessionScreen {
    /// The prompt band's height and the tail below it, for a `width` by `height` frame.
    ///
    /// One function rather than two calls at each site, because the tail depends on the band
    /// — the band is chrome the slack is measured after — and on the welcome block, which is
    /// measured at the width the sidebar leaves. Composing those three facts in more than one
    /// place is how a test comes to locate the prompt one row off from where `render` put it,
    /// and the row it would then read is blank, so the failure names the wrong thing.
    /// Close the composer's left and right edges in the margins of `band`.
    ///
    /// `band` is the composer's two rows at their full frame width and `composer` is the region
    /// actually filled, so the difference between them is the air the rules are painted into.
    /// Written cell by cell rather than as two one-column `Paragraph`s because a rule is one
    /// glyph repeated down a column, and a widget per column would be two more render calls for
    /// a shape that has no layout of its own.
    ///
    /// Both edges are optional and independently so. A frame with no margin — the 80-column pane
    /// where the composer is already the frame, or any used session — gets neither, and the band
    /// falls back to being told from its surroundings by its fill alone. That is the same
    /// degradation the panel and the wordmark make, and it is why this cannot be the *only*
    /// thing distinguishing the composer.
    fn composer_rules(&self, frame: &mut Frame<'_>, band: Rect, composer: Rect) {
        if composer.width == 0 || composer.height == 0 {
            return;
        }
        let style = self.context.accent();
        let left = composer.x.checked_sub(1).filter(|x| *x >= band.x);
        let right = Some(composer.x + composer.width).filter(|x| *x < band.x + band.width);
        let buffer = frame.buffer_mut();
        for y in band.y..band.y.saturating_add(band.height) {
            if let Some(x) = left {
                buffer[(x, y)]
                    .set_symbol(COMPOSER_LEFT_RULE)
                    .set_style(style);
            }
            if let Some(x) = right {
                buffer[(x, y)]
                    .set_symbol(COMPOSER_RIGHT_RULE)
                    .set_style(style);
            }
        }
    }

    /// Draw the session footer.
    ///
    /// Idle frames state the directory, context occupancy and command key. An active
    /// turn replaces the directory with one compact control surface: animated pulse,
    /// interrupt key, context occupancy and command discovery. It remains the final row
    /// in both states, so starting, confirming or stopping a turn never reflows the
    /// transcript or composer.
    ///
    /// # Idle facts, in ascending priority
    ///
    /// The directory is truncated from the left when it does not fit
    /// ([`crate::views::ambient::elide_left`], whose note explains why the tail is what
    /// identifies a path), and the right-hand pair is dropped whole rather than cut — the same
    /// same all-or-nothing rule the live footer uses, for the same reason: a fragment of a
    /// key name names no key.
    ///
    /// The command hint comes from [`crate::views::pressable_label`] rather than a literal, so
    /// a user who rebound `command_list` is told their own chord. A hint that resolves to
    /// nothing is omitted instead of printed as `none`.
    fn render_info(&self, frame: &mut Frame<'_>, area: Rect, now_ms: u64) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        crate::views::fill(frame.buffer_mut(), area, self.context.surface());
        let line = if self.status.is_running() || self.interrupt_notice_at(now_ms).is_some() {
            self.live_info_line(area.width, now_ms)
        } else {
            self.info_line(area.width)
        };
        ratatui::widgets::Widget::render(
            ratatui::widgets::Paragraph::new(vec![line]).style(self.context.surface()),
            area,
            frame.buffer_mut(),
        );
    }

    /// The active turn's one-line control surface.
    fn live_info_line(&self, width: u16, now_ms: u64) -> ratatui::text::Line<'static> {
        let key = crate::views::key_label("session_interrupt", &self.context)
            .map(|key| {
                if key == "escape" {
                    String::from("esc")
                } else {
                    key
                }
            })
            .unwrap_or_else(|| String::from("esc"));
        let mut left = vec![ratatui::text::Span::styled(
            String::from(" "),
            self.context.surface(),
        )];
        match (
            self.status.awaiting_user(),
            self.interrupt_notice_at(now_ms),
        ) {
            (Some(awaiting), _) => {
                left.push(ratatui::text::Span::styled(
                    String::from("△ "),
                    self.context.warning(),
                ));
                left.push(ratatui::text::Span::styled(
                    awaiting.status_text().to_owned(),
                    self.context.warning(),
                ));
            }
            (None, Some(InterruptNotice::Stopping)) => {
                left.push(ratatui::text::Span::styled(
                    String::from("interrupting…"),
                    self.context.muted(),
                ));
            }
            (None, Some(InterruptNotice::Confirm)) => {
                left.push(ratatui::text::Span::styled(
                    format!("{} ", self.transcript.transcript().work_pulse()),
                    self.context.warning(),
                ));
                left.push(ratatui::text::Span::styled(key, self.context.warning()));
                left.push(ratatui::text::Span::styled(
                    String::from(" again to interrupt"),
                    self.context.warning(),
                ));
            }
            (None, None) => {
                left.push(ratatui::text::Span::styled(
                    format!("{} ", self.transcript.transcript().work_pulse()),
                    self.context.accent(),
                ));
                left.push(ratatui::text::Span::styled(key, self.context.title()));
                left.push(ratatui::text::Span::styled(
                    String::from(" interrupt"),
                    self.context.muted(),
                ));
            }
        }

        let ambient = self.sidebar.ambient();
        let context = ambient.context.map(|context| {
            format!(
                "{}{} ({:.0}%)",
                if context.estimated { "≈" } else { "" },
                compact_live_tokens(context.prompt_tokens),
                context.percent()
            )
        });
        let commands = crate::views::pressable_label("command_list", &self.context)
            .map(|key| format!("{key} commands"));
        let mut trailing = Vec::new();
        if let Some(context) = context {
            trailing.push(context);
        }
        if let Some(commands) = commands {
            trailing.push(commands);
        }

        let columns = usize::from(width);
        let left_width = crate::views::markdown::row_width(&left);
        // Richest first. On a narrow pane command discovery yields before context,
        // because context pressure is the active turn's safety signal.
        for keep in (0..=trailing.len()).rev() {
            let right = trailing[..keep].join(INFO_SEPARATOR);
            let right_width = crate::views::display_width(&right);
            let minimum_gap = usize::from(!right.is_empty());
            if left_width + right_width + minimum_gap > columns {
                continue;
            }
            let gap = columns.saturating_sub(left_width + right_width);
            let mut spans = left;
            spans.push(ratatui::text::Span::styled(
                " ".repeat(gap),
                self.context.surface(),
            ));
            if !right.is_empty() {
                spans.push(ratatui::text::Span::styled(right, self.context.muted()));
            }
            return ratatui::text::Line::from(spans);
        }

        ratatui::text::Line::from(crate::views::markdown::truncate_row(left, columns))
    }

    /// The info row's single line, for a `width`-column frame.
    pub(crate) fn info_line(&self, width: u16) -> ratatui::text::Line<'static> {
        let ambient = self.sidebar.ambient();
        let directory = ambient.directory.clone().unwrap_or_default();
        let mut trailing = Vec::new();
        if let Some(context) = ambient.context {
            trailing.push(format!(
                "ctx {}{}/{} ({}{:.1}%)",
                if context.estimated { "≈" } else { "" },
                crate::views::ambient::compact(context.prompt_tokens),
                crate::views::ambient::compact(context.limit),
                if context.estimated { "≈" } else { "" },
                context.percent()
            ));
        }
        if let Some(key) = crate::views::pressable_label("command_list", &self.context) {
            trailing.push(format!("{key} commands"));
        };
        let columns = usize::from(width);
        let muted = self.context.muted();
        // Rungs richest first, dropping the leftmost — which is to say the lowest-ranked —
        // field still present, so the right edge does not reflow as the terminal narrows. The
        // same all-or-nothing trailer construction the live footer uses.
        for dropped in 0..=trailing.len() {
            let trailer = trailing[dropped..].join(INFO_SEPARATOR);
            let right = crate::views::display_width(&trailer);
            // One column of air at each end plus one between the two halves, so neither half
            // ever touches the frame's edge or the other.
            let room = columns.saturating_sub(right + if right == 0 { 2 } else { 3 });
            if room < INFO_MIN_DIRECTORY_COLS && !directory.is_empty() {
                continue;
            }
            let left = crate::views::ambient::elide_left(&directory, room);
            let gap = columns
                .saturating_sub(crate::views::display_width(&left) + right + 2)
                .max(usize::from(right > 0));
            return ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(String::from(" "), self.context.surface()),
                ratatui::text::Span::styled(left, muted),
                ratatui::text::Span::styled(" ".repeat(gap), self.context.surface()),
                ratatui::text::Span::styled(trailer, muted),
                ratatui::text::Span::styled(String::from(" "), self.context.surface()),
            ]);
        }
        crate::views::padded(&format!(" {directory}"), width, muted)
    }

    /// How many rows the session's own notices need at `width`.
    ///
    /// One function for two callers, because they must agree: [`Self::prompt_and_tail`]
    /// counts these rows into the head so the tail leaves room for them, and
    /// [`Self::render_session_notices`] sizes the region it draws them into. If the two
    /// measured separately, a notice would be allotted rows it did not fill or — worse —
    /// clipped in a frame whose arithmetic said it fitted.
    ///
    /// Measured through the transcript's own renderer rather than by counting messages: a
    /// notice wraps, and a long one is capped with a line saying how much was held back
    /// (`NOTICE_MAX_ROWS`), so message count and row count are unrelated numbers.
    fn session_notice_rows(&self, width: u16) -> u16 {
        u16::try_from(self.transcript.lines(width).len()).unwrap_or(u16::MAX)
    }

    /// Draw the session's own notices into the rows the welcome head does not use.
    ///
    /// # Why this costs no geometry
    ///
    /// The head is **bottom-anchored** in the body region — [`WelcomeView::lines_in`] pads
    /// with blank rows above it, deliberately, so the brand sits directly on the status
    /// prompt. Those leading rows are the only part of this screen that is blank by
    /// construction, so notices drawn there displace nothing: `head_rows`,
    /// [`welcome_tail_rows`], [`composer_region`] and the band order are all untouched, and
    /// the head can never be clipped because the region handed over is what is left after
    /// subtracting the head's own height.
    ///
    /// # Bottom-anchored, for the reason the head is
    ///
    /// The slice is placed flush *above* the head rather than at the top of the body. On a
    /// 200×50 frame the body's blank run is tall, and a warning pinned to row zero with
    /// twenty empty rows under it reads as an unrelated third block — the same "two blocks
    /// a third of a screen apart" failure [`WelcomeView::lines_in`] and
    /// [`WelcomeView::render_foot`] both exist to avoid. Sized to the content and pushed
    /// down against the brand, the notices read as one column with it.
    ///
    /// The transcript renders them, rather than this drawing its own rows, because the
    /// transcript is where they live: a second store would be a second copy of one fact,
    /// and it would be the copy that goes stale. It also means a notice looks the same
    /// before and after the first prompt.
    fn render_session_notices(&mut self, frame: &mut Frame<'_>, main: Rect, frame_height: u16) {
        if main.width == 0 || main.height == 0 {
            return;
        }
        let wanted = self.session_notice_rows(main.width);
        if wanted == 0 {
            return;
        }
        // `frame_height`, not `main.height`: the wordmark's fit is decided by the frame, so
        // asking the head how tall it is with the region's height would answer a different
        // question than the one the tail was computed from — see `WelcomeView::head_rows`.
        let head = self.welcome.head_rows(main.width, frame_height);
        let blank = main.height.saturating_sub(head);
        // Prefer the blank run above the welcome head, but never let decorative
        // welcome content make an actionable startup diagnostic disappear on a short
        // pane. When both cannot fit, the notice overlays the top of the welcome
        // region; the composer and footer remain untouched.
        let rows = wanted.min(main.height);
        let area = Rect {
            x: main.x,
            y: main.y.saturating_add(blank.saturating_sub(rows)),
            width: main.width,
            height: rows,
        };
        self.transcript.render(frame, area);
    }

    pub(crate) fn prompt_and_tail(&self, width: u16, height: u16) -> (u16, u16) {
        let band = prompt_rows(self.editor.height(), height);
        let empty = self.welcome_active();
        // At the *frame* height and the *frame* width — the same pair `WelcomeView::render`
        // decides the wordmark from. The width is no longer adjusted for the panel because the
        // panel is not drawn while this head is: see `sidebar_drawn`, and the head's own
        // measurement note in `welcome_tail_rows`.
        // The session's notices are counted into the head, and that reuses the existing
        // mechanism rather than adding one: `head` is already "the rows above the band that
        // must not be clipped", and `welcome_tail_rows` already trades the tail against it.
        // So a frame carrying a theme warning gives the band a shorter tail and a taller
        // body, which is the documented behaviour when the head binds — the band sits
        // slightly low rather than the warning being cut. Measured at `width`, the frame's
        // own, which is what the notices are drawn at: the panel that would narrow the body
        // is never drawn while this head is (see `sidebar_drawn`).
        let head = if empty {
            self.welcome
                .head_rows(width, height)
                .saturating_add(self.session_notice_rows(width))
        } else {
            0
        };
        // Every band other than the body and the tail, which is what `body_max` means: the rows
        // the body would get if the tail took none. `info_rows` belongs here, and leaving it
        // out is not a rounding error — it overstates the room the tail may take by one, so
        // the tail takes a row the body needed and the head this term exists to protect is
        // clipped by exactly that row.
        let body_max = height.saturating_sub(info_rows(height).saturating_add(band));
        (band, welcome_tail_rows(empty, height, band, body_max, head))
    }

    fn interrupt_notice_at(&self, now_ms: u64) -> Option<InterruptNotice> {
        if self.cancel_requested {
            return Some(InterruptNotice::Stopping);
        }
        self.interrupt_armed_at_ms
            .filter(|armed| now_ms.saturating_sub(*armed) <= INTERRUPT_CONFIRM_WINDOW_MS)
            .map(|_| InterruptNotice::Confirm)
    }

    fn prune_interrupt_at(&mut self, now_ms: u64) -> bool {
        let expired = self
            .interrupt_armed_at_ms
            .is_some_and(|armed| now_ms.saturating_sub(armed) > INTERRUPT_CONFIRM_WINDOW_MS);
        if expired {
            self.interrupt_armed_at_ms = None;
        }
        expired
    }

    fn observe_interrupt(&mut self, event: &AppEvent, now_ms: u64) -> EventResult {
        if matches!(
            event,
            AppEvent::Engine(
                zuno_engine::r#loop::TurnEvent::TurnCompleted { .. }
                    | zuno_engine::r#loop::TurnEvent::TurnInterrupted { .. }
                    | zuno_engine::r#loop::TurnEvent::TurnFailed { .. }
            )
        ) {
            let changed = self.cancel_requested || self.interrupt_armed_at_ms.is_some();
            self.cancel_requested = false;
            self.interrupt_armed_at_ms = None;
            return if changed {
                EventResult::REDRAW
            } else {
                EventResult::IGNORED
            };
        }
        if matches!(event, AppEvent::AnimationFrame) && self.prune_interrupt_at(now_ms) {
            return EventResult::REDRAW;
        }
        EventResult::IGNORED
    }

    /// Once a hard interrupt is accepted, keep late frames from the cancelled turn out
    /// of the live projection until the engine reports its terminal boundary.
    ///
    /// Durable persistence and edit diagnostics still run. This is only a presentation
    /// barrier for events that were already queued when the user confirmed cancellation.
    fn suppress_live_event_while_cancelling(&self, event: &AppEvent) -> bool {
        self.cancel_requested
            && matches!(
                event,
                AppEvent::Engine(
                    zuno_engine::r#loop::TurnEvent::TurnStarted { .. }
                        | zuno_engine::r#loop::TurnEvent::HistoryRepaired { .. }
                        | zuno_engine::r#loop::TurnEvent::AgentResolved { .. }
                        | zuno_engine::r#loop::TurnEvent::ModelResolved { .. }
                        | zuno_engine::r#loop::TurnEvent::AssistantMessageCreated { .. }
                        | zuno_engine::r#loop::TurnEvent::ToolSnapshotLocked { .. }
                        | zuno_engine::r#loop::TurnEvent::ProviderRequestStarted { .. }
                        | zuno_engine::r#loop::TurnEvent::Provider { .. }
                        | zuno_engine::r#loop::TurnEvent::AssistantCheckpointed { .. }
                        | zuno_engine::r#loop::TurnEvent::ToolDispatchStarted { .. }
                        | zuno_engine::r#loop::TurnEvent::ToolDispatchBlocked { .. }
                        | zuno_engine::r#loop::TurnEvent::ToolDispatchCompleted { .. }
                        | zuno_engine::r#loop::TurnEvent::ToolResultAppended { .. }
                        | zuno_engine::r#loop::TurnEvent::StepCompleted { .. }
                )
            )
    }

    /// Report whether the MCP projection moved since this screen last painted.
    ///
    /// The only thing that turns a lifecycle change into a frame. The worker's
    /// [`TerminalEvent::Wake`] reaches [`Component::handle_event`] on every surface and is
    /// claimed by none of them, so before this a server's transition from `Connecting` to
    /// a 30-second timeout changed the shared projection and changed nothing on the
    /// terminal — the open MCP list *and* the sidebar both kept showing the previous
    /// frame's bytes.
    ///
    /// Not `REDRAW` unconditionally: the worker republishes on a broadcast lag and after
    /// every completed toggle, and a frame per republication would repaint identical rows
    /// on a screen the redraw scheduler is otherwise allowed to leave alone.
    /// Read-only on purpose: the generation is recorded by [`Component::render`], the one
    /// place that actually paints. A reader that recorded here would mark the change seen
    /// on an event whose frame the app then declined to draw — a suspended terminal holding
    /// a plugin's lease is exactly that case — and the repaint would be owed and forgotten.
    fn observe_mcp(&self) -> EventResult {
        if self.mcp.generation() == self.mcp_generation {
            return EventResult::IGNORED;
        }
        EventResult::REDRAW
    }

    fn observe_session_title(&self) -> EventResult {
        if self.title.generation() == self.title_generation {
            return EventResult::IGNORED;
        }
        EventResult::REDRAW
    }

    fn observe_queued_inputs(&mut self) -> EventResult {
        let (generation, _inputs, notice) = self.queued_inputs.observe();
        if generation == self.queued_input_generation {
            return EventResult::IGNORED;
        }
        self.queued_input_generation = generation;
        if let Some(notice) = notice {
            use crate::views::picker::QueuedInputNoticeKind;
            self.toasts.push(match notice.kind {
                QueuedInputNoticeKind::Admitted(
                    crate::views::picker::QueuedInputDelivery::Queue,
                ) => Toast::info("request queued for the next turn"),
                QueuedInputNoticeKind::Admitted(
                    crate::views::picker::QueuedInputDelivery::Steer,
                ) => Toast::info("message queued; it will steer at the next safe point"),
                QueuedInputNoticeKind::Edited => {
                    Toast::success(format!("updated queued input {}", notice.input_id))
                }
                QueuedInputNoticeKind::Cancelled => {
                    Toast::success(format!("cancelled queued input {}", notice.input_id))
                }
                QueuedInputNoticeKind::Failed(message) => Toast::error(message),
            });
        }
        EventResult::REDRAW
    }

    fn observe_work_state(&self) -> EventResult {
        if self.work.generation() == self.work_generation {
            return EventResult::IGNORED;
        }
        EventResult::REDRAW
    }

    fn mark_turn_accepted(&mut self) {
        self.cancel_requested = false;
        self.interrupt_armed_at_ms = None;
        self.transcript.transcript_mut().mark_running();
        self.status.mark_running();
    }

    /// Milliseconds since this screen was built.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Act on one mouse event observed at `now_ms`.
    ///
    /// The single `MouseEventKind` match on this screen, and it has to stay single:
    /// `app::is_consumable_mouse` filters the same set *before* the bounded channel so an
    /// unconsumed event cannot delay a keystroke, and
    /// `app_the_input_filter_forwards_exactly_what_a_screen_consumes` scans this function's
    /// body to prove the two lists agree. An arm added in a sibling method would be a kind
    /// the filter still drops, so the arm could never run.
    ///
    /// Button-event reporting (`?1002`) supplies drag and release events. All-motion
    /// reporting (`?1003`) remains disabled, so moving the pointer without a held button
    /// still costs no channel traffic.
    fn handle_mouse(&mut self, mouse: &MouseEvent, now_ms: u64) -> EventResult {
        let notches: f64 = match mouse.kind {
            MouseEventKind::ScrollUp => -1.0,
            MouseEventKind::ScrollDown => 1.0,
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                return self.begin_pointer(mouse.column, mouse.row);
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                return self.drag_pointer(mouse.column, mouse.row);
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                return self.end_pointer(mouse.column, mouse.row);
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Right) => {
                return self.copy_pointer_selection(mouse.column, mouse.row);
            }
            // Horizontal wheels and free pointer movement have no consumer.
            _ => return EventResult::IGNORED,
        };
        if self.sidebar.contains(mouse.column, mouse.row) {
            let rows = if notches.is_sign_negative() { -3 } else { 3 };
            return if self.sidebar.scroll_lines(rows) {
                EventResult::REDRAW
            } else {
                EventResult::IGNORED
            };
        }
        self.scroll_transcript(notches, now_ms)
    }

    fn begin_pointer(&mut self, column: u16, row: u16) -> EventResult {
        if let Some(area) = self.autocomplete_area
            && self.autocomplete.select_at(column, row, area)
        {
            self.pointer = None;
            return self.complete_autocomplete();
        }
        if self.sidebar.click(column, row) {
            self.transcript.clear_selection();
            self.sidebar.clear_selection();
            return EventResult::REDRAW;
        }
        if self.sidebar.begin_selection(column, row) {
            self.transcript.clear_selection();
            self.pointer = Some(PointerGesture::Sidebar { dragged: false });
            return EventResult::REDRAW;
        }
        if let Some(area) = self
            .scrollbar_area
            .filter(|area| rect_contains(*area, column, row))
        {
            self.transcript.clear_selection();
            self.sidebar.clear_selection();
            let anchor = self.scrollbar.drag_anchor(row, area);
            self.pointer = Some(PointerGesture::Scrollbar { anchor });
            return self.drag_scrollbar(row, area, anchor);
        }
        if self.transcript.begin_selection(column, row) {
            self.sidebar.clear_selection();
            self.pointer = Some(PointerGesture::Transcript {
                column,
                row,
                dragged: false,
            });
            return EventResult::REDRAW;
        }
        self.pointer = None;
        self.handle_click(column, row)
    }

    fn drag_pointer(&mut self, column: u16, row: u16) -> EventResult {
        match self.pointer {
            Some(PointerGesture::Scrollbar { anchor }) => {
                let Some(area) = self.scrollbar_area else {
                    self.pointer = None;
                    return EventResult::IGNORED;
                };
                self.drag_scrollbar(row, area, anchor)
            }
            Some(PointerGesture::Transcript {
                column: start_column,
                row: start_row,
                ..
            }) => {
                if !self.transcript.update_selection(column, row) {
                    return EventResult::IGNORED;
                }
                self.pointer = Some(PointerGesture::Transcript {
                    column: start_column,
                    row: start_row,
                    dragged: true,
                });
                EventResult::REDRAW
            }
            Some(PointerGesture::Sidebar { .. }) => {
                if !self.sidebar.update_selection(column, row) {
                    return EventResult::IGNORED;
                }
                self.pointer = Some(PointerGesture::Sidebar { dragged: true });
                EventResult::REDRAW
            }
            None => EventResult::IGNORED,
        }
    }

    fn end_pointer(&mut self, column: u16, row: u16) -> EventResult {
        match self.pointer.take() {
            Some(PointerGesture::Scrollbar { anchor }) => {
                let Some(area) = self.scrollbar_area else {
                    return EventResult::IGNORED;
                };
                self.drag_scrollbar(row, area, anchor)
            }
            Some(PointerGesture::Transcript {
                column: start_column,
                row: start_row,
                dragged,
            }) => {
                let _ = self.transcript.update_selection(column, row);
                if dragged {
                    match self.transcript.selected_text() {
                        Some(text) => self.copy(text),
                        None => EventResult::REDRAW,
                    }
                } else {
                    self.transcript.clear_selection();
                    self.handle_click(start_column, start_row)
                }
            }
            Some(PointerGesture::Sidebar { dragged }) => {
                let _ = self.sidebar.update_selection(column, row);
                if dragged {
                    match self.sidebar.selected_text() {
                        Some(text) => self.copy(text),
                        None => EventResult::REDRAW,
                    }
                } else {
                    self.sidebar.clear_selection();
                    EventResult::REDRAW
                }
            }
            None => EventResult::IGNORED,
        }
    }

    fn copy_pointer_selection(&mut self, column: u16, row: u16) -> EventResult {
        let selected = if self.sidebar.contains(column, row) {
            self.sidebar.selected_text()
        } else {
            self.transcript.selected_text()
        };
        selected.map_or(EventResult::IGNORED, |text| self.copy(text))
    }

    fn drag_scrollbar(&mut self, row: u16, area: Rect, anchor: usize) -> EventResult {
        let offset = self.scrollbar.offset_at(row, area, anchor);
        self.transcript.set_offset(offset);
        self.scroller.total = self.transcript.content_height();
        self.scroller.viewport = self.transcript.viewport_height();
        self.scroller.sync_offset(self.transcript.offset());
        EventResult::REDRAW
    }

    /// Give a press to whichever surface owns the cell under it.
    ///
    /// Hit-tested against the geometry of the frame that **was drawn**, which the sidebar
    /// records while it paints. Deriving it there rather than here is the point: this method
    /// has no `Rect`, and a copy kept on this screen would need re-deriving on every resize
    /// and every sidebar toggle — the update that is forgotten is the one that makes a click
    /// land on a row that has moved.
    fn handle_click(&mut self, column: u16, row: u16) -> EventResult {
        if self.sidebar.click(column, row) {
            return EventResult::REDRAW;
        }
        if let Some(call_id) = self.transcript.tool_at(column, row).map(str::to_owned) {
            self.transcript.toggle_tool(&call_id);
            return EventResult::REDRAW;
        }
        if self.transcript.toggle_reasoning_at(column, row) {
            return EventResult::REDRAW;
        }
        // The panel first so its heading cells remain owned by the disclosure controls even
        // if a future layout puts another clickable surface beside or beneath them.
        if let Some(dialog) = self.message_actions(column, row) {
            self.requested.push(dialog);
            return EventResult::REDRAW;
        }
        // Unhandled rather than swallowed: the prompt does not act on a press yet, and
        // claiming it here would silently forbid one that later does.
        EventResult::IGNORED
    }

    /// The action menu for whichever message was pressed, or nothing when none was.
    ///
    /// # Only the user's own messages, and the box is the reason that reads correctly
    ///
    /// The owner asked for a menu on "the message you typed", and a framed user turn is now
    /// visibly a distinct object on screen — see
    /// [`TranscriptView::push_boxed`](crate::views::message) — so a box that answers a press
    /// while the prose beside it does not is a distinction the frame already taught. An
    /// assistant reply and a session notice therefore fall through, which keeps a press on
    /// them available to a later surface rather than opening a menu whose only honest row
    /// would be `Copy`.
    ///
    /// # `Revert` is offered on the newest prompt only, and that is a real limit rather than
    /// caution
    ///
    /// Revert is not a new capability: it is `/undo`, which reaches
    /// `zuno_snapshot::Store::restore_turn` through [`HostCommand::Undo`] and restores the
    /// worktree to the boundary before the last completed turn. That boundary is the one the
    /// *newest* prompt opened, so offering it there is exact.
    ///
    /// On an older prompt it would have to mean "undo N turns", and the TUI cannot express
    /// that: `SnapshotHistory` is a stack held by the host (`zuno-cli/src/cmd/tui.rs`), the
    /// screen has no handle on it, and nothing in this crate maps a transcript index to a
    /// checkpoint — a message is not a turn, since one turn appends the prompt, several
    /// assistant steps and any number of notices. So the row is *absent* on older messages
    /// rather than present and inert. A menu entry that silently does nothing is worse than a
    /// shorter menu, and it is the failure mode this codebase has paid for repeatedly.
    ///
    /// # A replayed prompt is never offered it, newest included
    ///
    /// [`Transcript::replay`](crate::views::message::Transcript::replay) puts a resumed
    /// session's persisted history on screen, which introduces prompts this process never
    /// ran. `SnapshotHistory` is rebuilt empty on every launch, so the checkpoint any of
    /// them opened belongs to an exited process — including the newest, which the `newest`
    /// test above would otherwise accept. Offering the row there would be exactly the
    /// defect the paragraph above refuses: a row whose only possible outcome is
    /// `nothing to undo`. The prefix boundary is
    /// [`Transcript::replayed`](crate::views::message::Transcript::replayed).
    ///
    /// Whether the host has a checkpoint to restore is deliberately **not** asked here. It
    /// cannot be — the stack is the host's — and it does not need to be: `restore_snapshot`
    /// answers `nothing to undo` and that refusal is reported. The same fallible-sink
    /// discipline [`Self::commit_selection`] documents.
    fn message_actions(
        &mut self,
        column: u16,
        row: u16,
    ) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let index = self.transcript.message_at(column, row)?;
        let messages = self.transcript.transcript().messages();
        let message = messages.get(index)?;
        if message.role != crate::views::message::Role::User {
            return None;
        }
        let lived_through = index >= self.transcript.transcript().replayed();
        let newest = messages
            .iter()
            .rposition(|held| held.role == crate::views::message::Role::User)
            == Some(index);
        let mut items = vec![
            crate::views::picker::Item::new("Copy message")
                .described("put this prompt on the clipboard")
                .valued(MESSAGE_ACTION_COPY),
        ];
        if newest && lived_through {
            items.push(
                crate::views::picker::Item::new("Revert this turn")
                    .described("restore the worktree to before this prompt ran")
                    .valued(MESSAGE_ACTION_REVERT),
            );
        }
        // Remembered here rather than encoded into the row's value, for the reason
        // `theme_restore` is a field: `DialogHost` pops the dialog and *then* routes its
        // outcome, so by the time the answer arrives the menu that knew which message it was
        // about no longer exists. A value like `copy:7` would work and would also make the
        // index a string parsed at the far end, where a parse failure is a silently dead row.
        self.message_menu = Some(index);
        Some(Box::new(crate::views::picker::SelectDialog::new(
            MESSAGE_ACTIONS_DIALOG_ID,
            "Message",
            self.context.clone(),
            items,
        )))
    }

    /// Act on a row of the message menu.
    ///
    /// `Copy` goes through [`Self::copy`], the same function the editor's own copy key and the
    /// debug panel use, so the clipboard ladder, the OSC 52 preference and all three outcome
    /// toasts are shared rather than reimplemented — see
    /// [`crate::views::external`], whose module note explains why OSC 52 is tried first and is
    /// what makes this work over SSH.
    ///
    /// `Revert` opens [`UNDO_CONFIRM_DIALOG_ID`] rather than submitting, and reuses the very
    /// dialog `/undo` opens. Reaching the driver directly would be the defect that
    /// confirmation was added for: this overwrites files on disk and discards anything edited
    /// since, and a mistyped click is at least as easy as a mistyped `/undo`.
    fn act_on_message(&mut self, index: usize, action: &str) -> EventResult {
        match action {
            MESSAGE_ACTION_COPY => {
                let text = self
                    .transcript
                    .transcript()
                    .messages()
                    .get(index)
                    .map(|message| {
                        message
                            .parts
                            .iter()
                            .filter_map(crate::views::message::MessagePart::text)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                self.copy(text)
            }
            MESSAGE_ACTION_REVERT => {
                self.requested.push(Box::new(
                    crate::views::basics::ConfirmDialog::new(
                        self.context.clone(),
                        UNDO_CONFIRM_DIALOG_ID,
                        "Revert the last turn",
                        "The worktree is restored to the boundary before the last completed \
                         turn. Anything edited since is discarded and cannot be recovered.",
                    )
                    .with_labels("Restore", "Keep"),
                ));
                EventResult::REDRAW
            }
            _ => EventResult::IGNORED,
        }
    }

    /// Scroll the transcript by `notches` observed at `now_ms`.
    ///
    /// The `messages_*` actions in [`Self::handle_view_action`] keep moving whole rows,
    /// unaccelerated, because a line the user asked for by name must not become four just
    /// because they pressed the key quickly — acceleration is a property of a continuous
    /// gesture, not of a deliberate step.
    ///
    /// No hit-testing against the pointer's position: the transcript is the only
    /// scrollable region on this screen, so a notch anywhere means the transcript, the
    /// same way `messages_line_up` does not care where the pointer is.
    fn scroll_transcript(&mut self, notches: f64, now_ms: u64) -> EventResult {
        // Re-stated per notch from the transcript, which measured all three on its last
        // render and is the only thing that owns them. This is what keeps the wheel from
        // drifting away from the view while a live turn grows the content underneath it.
        self.scroller.total = self.transcript.content_height();
        self.scroller.viewport = self.transcript.viewport_height();
        self.scroller.sync_offset(self.transcript.offset());
        if self.scroller.wheel(notches, now_ms) == 0 {
            // A notch whose multiplier has not yet accumulated a whole row moved
            // nothing, so repainting would cost a frame to redraw identical rows.
            return EventResult::IGNORED;
        }
        self.transcript.set_offset(self.scroller.offset());
        EventResult::REDRAW
    }

    /// Route the actions that change what is *shown* rather than what is typed.
    ///
    /// These were the largest class of built-but-unreachable behaviour in this crate.
    /// [`TranscriptView`] has had `toggle_thinking` and a clamped `set_offset` since the
    /// view layer was written, and no key press could reach either, because the composed
    /// screen forwarded keys only to the editor — and the editor answers
    /// [`EditorSignal::None`] for all of them, which the screen then reported as
    /// unhandled. A collapsible reasoning block nothing can collapse is
    /// indistinguishable from one that does not exist.
    fn handle_view_action(&mut self, action: &'static Definition) -> EventResult {
        let viewport = self.transcript.viewport_height().max(1);
        let max = self
            .transcript
            .content_height()
            .saturating_sub(self.transcript.viewport_height());
        let offset = self.transcript.offset();
        let moved = |delta: isize| -> usize {
            let target = isize::try_from(offset)
                .unwrap_or(isize::MAX)
                .saturating_add(delta);
            usize::try_from(target.max(0)).unwrap_or(0).min(max)
        };
        let half = isize::try_from(viewport / 2).unwrap_or(1).max(1);
        let page = isize::try_from(viewport).unwrap_or(1);
        match action.name {
            "session_new" => {
                let (notice, level) = self.commit_selection(Selection::NewSession);
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            "display_thinking" => {
                self.transcript.toggle_thinking();
                EventResult::REDRAW
            }
            "tool_details" => {
                self.transcript.toggle_tool_output();
                EventResult::REDRAW
            }
            "sidebar_toggle" => {
                self.sidebar_visible = !self.sidebar_visible;
                EventResult::REDRAW
            }
            "scrollbar_toggle" => {
                self.scrollbar_visible = !self.scrollbar_visible;
                EventResult::REDRAW
            }
            "tips_toggle" => {
                if self.welcome.tips_visible() {
                    self.welcome.hide_tips();
                } else {
                    self.welcome.next_tip();
                }
                EventResult::REDRAW
            }
            "messages_line_up" => {
                self.transcript.set_offset(moved(-1));
                EventResult::REDRAW
            }
            "messages_line_down" => {
                self.transcript.set_offset(moved(1));
                EventResult::REDRAW
            }
            "messages_page_up" => {
                self.transcript.set_offset(moved(-page));
                EventResult::REDRAW
            }
            "messages_page_down" => {
                self.transcript.set_offset(moved(page));
                EventResult::REDRAW
            }
            "messages_half_page_up" => {
                self.transcript.set_offset(moved(-half));
                EventResult::REDRAW
            }
            "messages_half_page_down" => {
                self.transcript.set_offset(moved(half));
                EventResult::REDRAW
            }
            "messages_first" => {
                self.transcript.set_offset(0);
                EventResult::REDRAW
            }
            "messages_last" => {
                self.transcript.set_offset(max);
                self.transcript.follow();
                EventResult::REDRAW
            }
            "model_list" => self.request(self.model_picker()),
            "agent_list" => self.request(self.agent_picker()),
            "agent_cycle" => self.cycle_agent(1),
            "agent_cycle_reverse" => self.cycle_agent(-1),
            "variant_cycle" => self.cycle_effort(1),
            "messages_transcript" => self.toggle_transcript(),
            "session_list" => self.request(Some(self.session_picker())),
            "session_queued_prompts" => self.request(self.queued_input_view()),
            // Two statements because opening the theme picker also records the theme to
            // put back on escape, which needs `&mut self` while `request` does too.
            "theme_list" => {
                let dialog = self.theme_picker();
                self.request(dialog)
            }
            "session_child_first" => self.request(self.subagent_view()),
            "ps_view" => self.request(self.background_view()),
            "memory_view" => self.request(self.memory_view()),
            "mcp_list" => self.request(self.mcp_list()),
            "status_view" => self.request(self.status_panel()),
            "debug_view" => self.request(self.debug_panel()),
            "prompt_skills" => self.request(self.skill_list()),
            "diff_open" => self.request(self.diff_view()),
            "help_show" => self.request(self.help_view()),
            "command_list" => self.request(self.command_palette()),
            _ => EventResult::IGNORED,
        }
    }

    /// The command palette.
    ///
    /// Always available, and that is the point: forty-three rows of the binding table ship
    /// with `keys: "none"`, faithfully to upstream, and upstream's answer for reaching one
    /// is the palette. Without it a third of the table is unreachable by any means —
    /// `command_list` was itself on the welcome screen's hint list, bound to `ctrl+p`, and
    /// reached nothing, which is why it was removed from that list rather than wired.
    fn command_palette(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        // The keymap rather than the shipped table, so the spelling each row shows is the
        // one the user would actually press. Without a keymap there is nothing honest to
        // print, and `request` says so instead of guessing.
        let keymap = self.keymap.as_ref()?;
        Some(Box::new(crate::views::palette::palette(
            self.context.clone(),
            keymap,
        )))
    }

    /// Run the action a palette row named.
    ///
    /// Guarded against the palette naming itself, which would push a second palette over
    /// the first and leave one behind on every later choice.
    fn dispatch_action(&mut self, action: &str) -> EventResult {
        if action == "command_list" {
            return EventResult::IGNORED;
        }
        let Some(definition) = crate::keybind::definition(action) else {
            return EventResult::IGNORED;
        };
        // A synthetic event with no key: the two readers both fall back to the action name.
        // `handle_action` checks `APP_EXIT` before asking whether the chord is an exit
        // chord, and `typed_character` yields nothing for a null key — correct here,
        // because a palette choice is not a typed character.
        let event = KeyEvent::new(
            crossterm::event::KeyCode::Null,
            crossterm::event::KeyModifiers::NONE,
        );
        self.handle_action(definition, &event)
    }

    /// Ask the host to open `dialog`, or report why it cannot be opened.
    ///
    /// A picker with nothing in it is the failure mode that reads as a broken key: the
    /// dialog opens, says `no matches`, and the user cannot tell an empty catalog from
    /// a surface that did not load. This is transient interface state, not conversation
    /// content, so it must never enter the durable/model-visible transcript.
    fn request(&mut self, dialog: Option<Box<dyn crate::views::dialog::Dialog>>) -> EventResult {
        match dialog {
            Some(dialog) => {
                self.requested.push(dialog);
                EventResult::REDRAW
            }
            None => {
                self.toasts
                    .push(Toast::info("Nothing is available in this view yet."));
                EventResult::REDRAW
            }
        }
    }

    fn model_picker(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.catalog.models.is_empty() {
            return None;
        }
        let mut picker =
            crate::views::picker::model_picker(self.context.clone(), self.catalog.models.clone());
        if let Some(model) = &self.catalog.model {
            picker = picker.selecting(model);
        }
        Some(Box::new(picker))
    }

    fn agent_picker(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.catalog.agents.is_empty() {
            return None;
        }
        let mut picker =
            crate::views::picker::agent_picker(self.context.clone(), self.catalog.agents.clone());
        if let Some(agent) = &self.catalog.agent {
            picker = picker.selecting(agent);
        }
        Some(Box::new(picker))
    }

    /// Build the session picker from the screen's current projection.
    ///
    /// Client hosts use this after a session-deletion remount so the refreshed list is
    /// already open on the replacement composition. Keeping construction here prevents
    /// clients from duplicating the active-row and empty-catalog rules.
    pub fn session_picker(&self) -> Box<dyn crate::views::dialog::Dialog> {
        let mut picker = crate::views::picker::session_picker(
            self.context.clone(),
            self.catalog.sessions.clone(),
        );
        if let Some(session) = &self.catalog.session {
            picker = picker.selecting(session);
        }
        Box::new(picker)
    }

    fn queued_input_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        Some(Box::new(crate::views::picker::queued_input_dialog(
            self.context.clone(),
            self.queued_inputs.clone(),
        )))
    }

    fn mcp_list(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.mcp.is_empty() {
            return None;
        }
        Some(Box::new(crate::views::picker::mcp_list(
            self.context.clone(),
            self.mcp.clone(),
        )))
    }

    /// The delegated-task view, over the delegations this conversation actually made.
    ///
    /// Always present, for the reason the census below is: "this session has delegated
    /// nothing" is itself the answer a user opening it wants, and returning `None` would
    /// replace that answer with `request`'s generic "nothing to choose from here yet",
    /// which reads as a surface that failed rather than as a session with no subagents.
    fn subagent_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        Some(Box::new(
            crate::views::subagent::SubagentView::new(
                self.context.clone(),
                crate::views::subagent::delegations(self.transcript.transcript().messages()),
            )
            .with_work_state(self.work.clone()),
        ))
    }

    fn background_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        Some(Box::new(crate::views::background::BackgroundView::new(
            self.context.clone(),
            Arc::clone(self.background_executions.as_ref()?),
            self.background_session.clone().unwrap_or_default(),
        )))
    }

    fn memory_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        Some(Box::new(crate::views::memory::MemoryView::new(
            self.context.clone(),
            self.work.clone(),
        )))
    }

    fn cancel_background(&mut self, execution_id: &str) -> EventResult {
        let outcome = (|| {
            let service = self
                .background_executions
                .as_ref()
                .ok_or_else(|| "background execution service is unavailable".to_owned())?;
            let id = zuno_pty::BackgroundExecutionId::parse(execution_id.to_owned())
                .map_err(|error| error.to_string())?;
            let info = service.get(&id).map_err(|error| error.to_string())?;
            if self.background_session.as_deref() != Some(info.session_id.as_str()) {
                return Err(format!(
                    "background execution `{execution_id}` is not owned by this session"
                ));
            }
            service.cancel(&id).map_err(|error| error.to_string())
        })();
        self.toasts.push(match outcome {
            Ok(true) => Toast::success(format!("stopping background terminal {execution_id}")),
            Ok(false) => Toast::warning(format!(
                "background terminal {execution_id} is already settled"
            )),
            Err(error) => Toast::error(error),
        });
        EventResult::REDRAW
    }

    /// `§8.7`'s status census, with the MCP group read live at open time.
    ///
    /// Always present, unlike the pickers: a census whose groups are all empty is itself
    /// the answer to "why is nothing happening", so returning `None` here would replace a
    /// useful report with "nothing to choose from here yet".
    fn status_panel(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let mcp = self
            .mcp
            .snapshot()
            .iter()
            .map(crate::views::picker::McpServer::service)
            .collect();
        let mut groups = vec![crate::views::diagnostics::Group::new("MCP servers", mcp)];
        groups.extend(self.census.iter().cloned());
        Some(Box::new(crate::views::diagnostics::StatusPanel::new(
            self.context.clone(),
            groups,
        )))
    }

    /// `§8.7`'s debug report.
    fn debug_panel(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        Some(Box::new(crate::views::diagnostics::DebugPanel::new(
            self.context.clone(),
            self.debug.clone(),
        )))
    }

    fn skill_list(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let skills = self.sidebar.ambient().skills.clone();
        if skills.is_empty() {
            return None;
        }
        Some(Box::new(crate::views::picker::skill_list(
            self.context.clone(),
            skills,
        )))
    }

    /// The most recent patch a tool reported, as a scrollable diff.
    ///
    /// Read back out of the transcript rather than accumulated separately: the transcript
    /// already recognises a unified diff in tool output — see
    /// [`crate::views::message::looks_like_diff`] — so a second collector could disagree
    /// with what is on screen. Absent when no tool has produced one, which the caller
    /// reports rather than opening an empty viewer.
    fn diff_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let patch = self.transcript.transcript().latest_diff()?;
        Some(Box::new(crate::views::diff::DiffDialog::new(
            self.context.clone(),
            &patch,
        )))
    }

    /// The keybinding reference, when the host supplied the keymap to build it from.
    ///
    /// A help view lists what the *user's* keymap resolved, so it cannot be built from
    /// the shipped table alone; without the keymap it would advertise defaults the user
    /// may have rebound. Absent rather than wrong: the key then reports "nothing to show"
    /// instead of printing a table of keys that do not work.
    fn help_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let keymap = self.keymap.as_ref()?;
        Some(Box::new(crate::views::help::HelpView::new(
            self.context.clone(),
            keymap,
        )))
    }

    /// The theme picker, and the restore point escape needs.
    ///
    /// Moving the cursor in this picker re-themes the screen immediately — see
    /// [`crate::views::ViewContext::set_theme`] — so the theme showing when it opened is
    /// recorded here first. Without it, cancelling would leave the user in whichever
    /// theme they happened to be scrolling past, which is the one outcome they did not
    /// ask for.
    fn theme_picker(&mut self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        // The registry is built here rather than held, because the picker resolves every
        // theme once at construction for its preview and then never consults it again.
        let registry = crate::theme::ThemeRegistry::new();
        let active = self.context.theme();
        self.theme_restore = Some(Arc::clone(&active));
        Some(Box::new(crate::views::picker::theme_picker(
            self.context.clone(),
            &registry,
            // The mode the host resolved at startup, carried on the active theme rather
            // than re-decided here. A second mode policy in this crate would preview
            // dark variants on a terminal the CLI had already found to be light.
            active.mode,
        )))
    }

    /// Put back the theme the picker opened over.
    fn restore_theme(&mut self) -> EventResult {
        let Some(previous) = self.theme_restore.take() else {
            return EventResult::IGNORED;
        };
        self.context.set_theme(&previous);
        EventResult::REDRAW
    }

    /// Cancel a running turn, or leave the application when none is running.
    ///
    /// Falling through to shutdown when the sink is missing or refuses is what keeps
    /// this from becoming the trap described in the module docs.
    fn request_exit(&mut self) -> EventResult {
        if self.status.is_running()
            && !self.cancel_requested
            && let Some(cancels) = self.cancels.as_ref()
            && cancels.try_send(()).is_ok()
        {
            self.cancel_requested = true;
            self.interrupt_armed_at_ms = None;
            self.cancellations += 1;
            self.toasts.push(Toast::info(
                "cancelling the turn; press the same exit key again to leave",
            ));
            return EventResult::REDRAW;
        }
        let _requested = self.shutdown.try_send(TerminalEvent::Shutdown);
        EventResult::REDRAW
    }

    /// Arm or confirm cancellation of the running turn.
    ///
    /// `now_ms` is supplied by the screen's monotonic clock so the confirmation window
    /// can be tested without sleeping.
    fn request_interrupt_at(&mut self, now_ms: u64) -> EventResult {
        if !self.status.is_running() {
            self.interrupt_armed_at_ms = None;
            return EventResult::IGNORED;
        }
        if self.cancel_requested {
            return EventResult::REDRAW;
        }
        let confirmed = self
            .interrupt_armed_at_ms
            .is_some_and(|armed| now_ms.saturating_sub(armed) <= INTERRUPT_CONFIRM_WINDOW_MS);
        if !confirmed {
            self.interrupt_armed_at_ms = Some(now_ms);
            return EventResult::REDRAW;
        }

        self.interrupt_armed_at_ms = None;
        if let Some(cancels) = self.cancels.as_ref()
            && cancels.try_send(()).is_ok()
        {
            self.cancel_requested = true;
            self.cancellations += 1;
        } else {
            self.toasts
                .push(Toast::warning("the active turn could not be cancelled"));
        }
        EventResult::REDRAW
    }
}

impl SessionScreen {
    /// Adopt a picker's answer, and forward it to whoever can act on it.
    ///
    /// The reply identity and welcome facts are updated here so the choice is visible
    /// immediately, while the sink carries it to the host that can only apply it to the
    /// *next* turn. Saying so is the point: a model that changed on screen but not in the
    /// running turn, with nothing said, is a surface that lies.
    ///
    /// # Said in a toast, not in the transcript, and that is the reported defect
    ///
    /// Every notice here used to be pushed onto the transcript. On a fresh session the
    /// first thing a user did — press `<leader>m`, pick a model — therefore opened the
    /// conversation with `model set to … for the next turn` as its *first message*, which
    /// the owner reported as a session hint with no reason to be at the top of a first
    /// conversation. Two things were wrong with it, and they are the two
    /// [`Self::copy`] already records about the same choice:
    ///
    /// * It is **permanent.** A confirmation of a switch is true for one moment and is
    ///   then exported, re-read and re-rendered forever as though it were part of the
    ///   conversation. The durable answer already lives on the reply identity, which states
    ///   the agent and the model on every frame.
    /// * It is **invisible when it matters.** A picker's answer is routed while the picker
    ///   is closing, and a transcript row lands behind whatever modal is still up.
    ///   `§11.4` puts a toast over the dialog for exactly this case.
    ///
    /// [`Self::cycle_agent`] already reached this conclusion for the *same* facts by a
    /// different route — "cycling is exploratory and repeated, so walking seven agents
    /// would leave seven permanent rows in a transcript being read for a reply" — so the
    /// two surfaces that switch a model or an agent now report identically instead of one
    /// of them writing history.
    fn adopt(&mut self, dialog: &'static str, value: &str) -> EventResult {
        let selection = match dialog {
            crate::views::picker::MODEL_DIALOG_ID => {
                self.catalog.model = Some(value.to_owned());
                self.status.set_configured_model(value);
                self.sidebar.ambient_mut().model = Some(value.to_owned());
                self.welcome.facts_mut().model = Some(value.to_owned());
                self.adopt_model_reasoning(value);
                Selection::Model(value.to_owned())
            }
            crate::views::picker::AGENT_DIALOG_ID => {
                self.catalog.agent = Some(value.to_owned());
                self.status.set_configured_agent(value);
                self.sidebar.ambient_mut().agent = Some(value.to_owned());
                self.welcome.facts_mut().agent = Some(value.to_owned());
                Selection::Agent(value.to_owned())
            }
            crate::views::picker::SESSION_DIALOG_ID => {
                if self.catalog.session.as_deref() == Some(value) {
                    self.toasts.push(Toast::new(
                        ToastLevel::Info,
                        format!("session {value} is already active"),
                    ));
                    return EventResult::REDRAW;
                }
                Selection::Session(value.to_owned())
            }
            // No [`Selection::Theme`] is sent, and that is the change. The variant stays
            // because the host still matches on it, but a theme is the view layer's own
            // state now: the palette on screen is already the chosen one — the picker's
            // highlight hook applied it as the cursor arrived — so committing only has to
            // drop the restore point. Sending it would put a colour change through the
            // channel that rebuilds the turn host, and would earn the "not applied:
            // nothing is listening" notice from a host that deliberately discards it,
            // which would be a lie about a theme that visibly did apply.
            crate::views::picker::THEME_DIALOG_ID => {
                self.theme_restore = None;
                // The resolved name, not `value`: a theme that fell back is showing the
                // fallback, and the notice should say what the user is looking at.
                let name = self.context.theme().name.clone();
                self.toasts
                    .push(Toast::success(format!("theme set to {name}")));
                return EventResult::REDRAW;
            }
            // The palette resolves to *another action's name*, so it re-enters the same
            // routing a key press takes. That is what makes an unbound action reachable
            // without a second copy of the routing table. Re-entry is bounded because the
            // palette is excluded from what it can dispatch.
            crate::views::palette::DIALOG_ID => return self.dispatch_action(value),
            SKILL_DIALOG_ID => {
                // `Info`: nothing was refused and nothing succeeded — the picker exists to
                // report the name, and this states it.
                self.toasts.push(Toast::new(
                    ToastLevel::Info,
                    format!("skill `{value}` — name it in a prompt to invoke it"),
                ));
                return EventResult::REDRAW;
            }
            _ => return EventResult::IGNORED,
        };
        let (text, level) = self.commit_selection(selection);
        self.toasts.push(Toast::new(level, text));
        EventResult::REDRAW
    }

    /// Send `selection` to the host, and say what happened without choosing where.
    ///
    /// Split out of [`Self::adopt`] so the cycling keys can reuse the *delivery* while
    /// reporting on a different surface. Both callers must keep the refusal branch, which is
    /// the whole reason the level comes back: a selection that reached nothing and said so
    /// nowhere is the defect class the sink was made fallible to expose.
    ///
    /// A [`ToastLevel`] rather than the `bool` this returned before, because the two callers
    /// render on different surfaces — a toast and a transcript notice — and each was mapping
    /// the boolean itself. One of them got it wrong: the picker's notice was drawn at warning
    /// grade whether the selection was delivered or refused, so a model switch that worked
    /// was announced with the same `!` as one that had not. Returning the level is what makes
    /// the two surfaces agree by construction.
    fn commit_selection(&mut self, selection: Selection) -> (String, ToastLevel) {
        let notice = match &selection {
            Selection::Model(model) => format!("model set to {model} for the next turn"),
            Selection::Agent(agent) => format!("agent set to {agent} for the next turn"),
            Selection::Session(id) => format!("switching to session {id}"),
            Selection::NewSession => String::from("starting a new session"),
            Selection::SessionRename { id, title } => {
                format!("renaming session {id} to {title}")
            }
            Selection::SessionDelete(id) => format!("deleting session {id}"),
            Selection::JobCancel(id) => format!("cancelling background job {id}"),
            Selection::MemoryApply(id) => format!("approving memory candidate {id}"),
            Selection::MemoryReject(id) => format!("rejecting memory candidate {id}"),
            Selection::MemoryUndo(id) => format!("undoing memory candidate {id}"),
            Selection::MemoryEditApply { id, .. } => {
                format!("applying edited memory candidate {id}")
            }
            Selection::MemoryRemove { scope, .. } => {
                format!("removing {} resident memory", scope.as_str())
            }
            Selection::Theme(theme) => format!("theme {theme} selected"),
            Selection::Effort(effort) => {
                format!("reasoning set to {effort} for the next turn")
            }
        };
        let delivered = self
            .selections
            .as_ref()
            .is_some_and(|sink| sink.try_send(selection).is_ok());
        if delivered {
            (notice, ToastLevel::Success)
        } else {
            // A refused sink is reported rather than swallowed. The alternative is the
            // defect this whole change is about: a picker that appears to work, a
            // selection that reached nothing, and no way for the user to tell.
            (
                format!("{notice} (not applied: nothing is listening)"),
                ToastLevel::Warning,
            )
        }
    }

    /// Move to the agent `step` places along the catalog, wrapping at both ends.
    ///
    /// Cycles [`SessionCatalog::agents`] in its own order — the same list and sequence
    /// `<leader>a` opens. Deriving a second list here is the failure this codebase keeps
    /// paying for: two surfaces that each decide what "the agents" are will disagree, and the
    /// user cannot tell which is lying. Subagent-only and hidden rows are excluded where the
    /// catalog is built, so both surfaces drop the same rows for the same reason.
    ///
    /// Reports through a toast where the picker pushes a transcript notice: cycling is
    /// exploratory and repeated, so walking seven agents would leave seven permanent rows in
    /// a transcript being read for a reply, and the reply identity already holds the durable
    /// answer. A refused sink still reports, at warning grade — a key that appears to switch
    /// and reaches nothing is worse than a dead one, because the identity agrees with it.
    ///
    /// Mid-turn is not special-cased, deliberately: `drive_turns` reads this channel only
    /// between turns, the same deferral the MCP toggle relies on.
    fn cycle_agent(&mut self, step: isize) -> EventResult {
        let names: Vec<String> = self
            .catalog
            .agents
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        if names.len() < 2 {
            // One agent is not a cycle, and silence would be indistinguishable from the dead
            // key this action shipped as. Naming the count is what tells the user the key
            // works and the catalog is short.
            self.toasts.push(Toast::warning(format!(
                "no other agent to switch to: the catalog has {}",
                match names.len() {
                    0 => String::from("none"),
                    _ => format!("only `{}`", names[0]),
                }
            )));
            return EventResult::REDRAW;
        }
        let current = self
            .catalog
            .agent
            .as_ref()
            .and_then(|active| names.iter().position(|name| name == active));
        // An agent absent from the list — launched with `--agent` naming a subagent, say —
        // has no position to step from, so the first row is where the cycle starts rather
        // than nowhere. `rem_euclid` over the signed sum is what makes one implementation
        // serve both directions, including the wrap from the first row backwards.
        let next = match current {
            Some(index) => {
                let length = isize::try_from(names.len()).unwrap_or(isize::MAX);
                let moved = isize::try_from(index).unwrap_or(0).saturating_add(step);
                usize::try_from(moved.rem_euclid(length)).unwrap_or(0)
            }
            None => 0,
        };
        let name = names[next].clone();
        self.catalog.agent = Some(name.clone());
        self.status.set_configured_agent(&name);
        self.sidebar.ambient_mut().agent = Some(name.clone());
        self.welcome.facts_mut().agent = Some(name.clone());
        let (text, level) = self.commit_selection(Selection::Agent(name));
        self.toasts.push(Toast::new(level, text));
        EventResult::REDRAW
    }

    /// Track whether the newly chosen model reasons, and drop a level it cannot use.
    ///
    /// Both halves matter. Keeping `reasoning` stale leaves the cycling key looking live
    /// on a model that ignores it; keeping the level itself would leave `(high)` on
    /// the reply identity next to a model whose request carries no such control — the exact lie
    /// this feature must not tell. The host reaches the same conclusion independently in
    /// `session_reasoning_options`, so the identity and the wire agree.
    ///
    /// A model absent from the catalog list leaves both untouched: an unknown row is not
    /// evidence that reasoning went away.
    fn adopt_model_reasoning(&mut self, qualified: &str) {
        let Some(reasoning) = self
            .catalog
            .models
            .iter()
            .find(|entry| entry.id == qualified)
            .map(|entry| entry.reasoning)
        else {
            return;
        };
        self.catalog.reasoning = reasoning;
        let supports_active = self.catalog.effort.is_none_or(|active| {
            self.catalog
                .reasoning_efforts
                .get(qualified)
                .filter(|levels| !levels.is_empty())
                .map_or(reasoning, |levels| levels.contains(&active))
        });
        if !reasoning || !supports_active {
            self.catalog.effort = None;
            self.status.set_effort(None);
            self.welcome.facts_mut().reasoning = None;
        }
    }

    /// Step the reasoning level, and say so when the model has none to step.
    ///
    /// # Why this refuses rather than cycling a label
    ///
    /// A level only means something if the request carries it, and the request carries it
    /// only when the catalog says the model reasons — `session_reasoning_options` in
    /// `zuno-cli/src/cmd/turn.rs` returns nothing otherwise. Cycling here anyway would
    /// give a key that changes the identity and nothing else, which is the failure this
    /// project has removed repeatedly. The toast names the model so the refusal is
    /// actionable: the answer is to switch models, not to press harder.
    ///
    /// The cycle runs over the current model's declared canonical variants, weakest to
    /// strongest. A reasoning model with no declared variants falls back to
    /// [`ReasoningEffort::ALL`]. Starting with no level selects the first available one.
    fn cycle_effort(&mut self, step: isize) -> EventResult {
        use zuno_llm::effort::ReasoningEffort;
        if !self.catalog.reasoning {
            self.toasts.push(Toast::warning(match &self.catalog.model {
                Some(model) => format!(
                    "{model} does not support selectable reasoning effort. Choose a \
                     reasoning-capable model to change the effort level."
                ),
                None => String::from(
                    "No model is currently resolved, so reasoning effort cannot be changed. \
                     Choose a model first.",
                ),
            }));
            return EventResult::REDRAW;
        }
        let levels = self
            .catalog
            .model
            .as_ref()
            .and_then(|model| self.catalog.reasoning_efforts.get(model))
            .filter(|levels| !levels.is_empty())
            .map(Vec::as_slice)
            .unwrap_or(&ReasoningEffort::ALL);
        let length = isize::try_from(levels.len()).unwrap_or(isize::MAX);
        let current = self
            .catalog
            .effort
            .and_then(|active| levels.iter().position(|level| *level == active));
        // `rem_euclid` over the signed sum, as `cycle_agent` does, so one expression
        // serves both directions including the wrap backwards off the first level.
        let next = match current {
            Some(index) => {
                let moved = isize::try_from(index).unwrap_or(0).saturating_add(step);
                usize::try_from(moved.rem_euclid(length)).unwrap_or(0)
            }
            None => 0,
        };
        let chosen = levels[next];
        self.catalog.effort = Some(chosen);
        self.status.set_effort(Some(chosen));
        self.welcome.facts_mut().reasoning = Some(chosen.as_str().to_owned());
        let (text, level) = self.commit_selection(Selection::Effort(chosen));
        self.toasts.push(Toast::new(level, text));
        EventResult::REDRAW
    }
    fn send_queue_mutation(&mut self, mutation: QueuedInputMutation) -> EventResult {
        let delivered = self
            .queue_mutations
            .as_ref()
            .is_some_and(|sink| sink.try_send(mutation).is_ok());
        if !delivered {
            self.toasts.push(Toast::warning(
                "queued input was not changed: the queue manager is unavailable",
            ));
        }
        EventResult::REDRAW
    }
}

impl ActionComponent for SessionScreen {
    fn dialog_region(&self, dialog: &'static str, area: Rect) -> Option<Rect> {
        if !matches!(
            dialog,
            crate::views::question::DIALOG_ID | crate::views::permission::DIALOG_ID
        ) {
            return None;
        }
        let empty = self.welcome_active();
        let content = content_bounds(area, sidebar_drawn(self.sidebar_visible, empty, area.width));
        let composer = composer_region(content, empty);
        Some(Rect {
            height: composer.height.saturating_sub(info_rows(content.height)),
            ..composer
        })
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        if self.autocomplete.is_open() {
            vec!["prompt.autocomplete"]
        } else if self.editor.is_empty()
            && self.transcript.content_height() > self.transcript.viewport_height()
        {
            // In native-selection mode, terminal alternate-scroll converts wheel notches
            // to Up/Down keys. Promoting `messages` only for an empty composer makes those
            // keys scroll the transcript without stealing vertical editing or history
            // traversal from a prompt the user is actively composing.
            vec!["messages"]
        } else if self.editor.cursor().line == 0
            || self.editor.cursor().line + 1 == self.editor.height()
        {
            // Scope ordering cannot vary by chord, so both history arrows are promoted at
            // either vertical edge. `InputEditor` then applies the directional half of the
            // rule: an arrow pointing into a multi-line buffer still moves the cursor, while
            // one pointing out past its first/last line walks history. Promoting everywhere
            // would shadow `input_move_up/down` throughout pasted blocks; never promoting is
            // the original bug that made persisted history unreachable.
            vec!["history"]
        } else {
            Vec::new()
        }
    }

    fn drain_dialogs(&mut self) -> Vec<Box<dyn crate::views::dialog::Dialog>> {
        std::mem::take(&mut self.requested)
    }

    fn drain_toasts(&mut self) -> Vec<Toast> {
        std::mem::take(&mut self.toasts)
    }

    fn apply_dialog_outcome(
        &mut self,
        dialog: &'static str,
        outcome: &crate::views::dialog::DialogOutcome,
    ) -> EventResult {
        match outcome {
            crate::views::dialog::DialogOutcome::QueuedInput(
                crate::views::picker::QueuedInputDialogAction::Edit {
                    id,
                    expected_revision,
                    text,
                },
            ) => {
                self.queued_input_edit = Some((id.clone(), *expected_revision, text.clone()));
                self.requested
                    .push(Box::new(crate::views::basics::PromptDialog::new(
                        self.context.clone(),
                        QUEUED_INPUT_EDIT_DIALOG_ID,
                        "Edit queued input",
                        text,
                    )));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::QueuedInput(
                crate::views::picker::QueuedInputDialogAction::Cancel {
                    id,
                    expected_revision,
                },
            ) => self.send_queue_mutation(QueuedInputMutation::Cancel {
                id: id.clone(),
                expected_revision: *expected_revision,
            }),
            crate::views::dialog::DialogOutcome::Submitted { text, .. }
                if dialog == QUEUED_INPUT_EDIT_DIALOG_ID =>
            {
                let Some((id, expected_revision, original)) = self.queued_input_edit.take() else {
                    return EventResult::IGNORED;
                };
                if text.trim().is_empty() {
                    self.queued_input_edit = Some((id, expected_revision, original.clone()));
                    self.requested
                        .push(Box::new(crate::views::basics::PromptDialog::new(
                            self.context.clone(),
                            QUEUED_INPUT_EDIT_DIALOG_ID,
                            "Edit queued input",
                            original,
                        )));
                    self.toasts
                        .push(Toast::warning("queued input cannot be empty"));
                    return EventResult::REDRAW;
                }
                self.send_queue_mutation(QueuedInputMutation::Edit {
                    id,
                    expected_revision,
                    text: text.to_owned(),
                })
            }
            crate::views::dialog::DialogOutcome::Cancelled
                if dialog == QUEUED_INPUT_EDIT_DIALOG_ID =>
            {
                self.queued_input_edit = None;
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Session(
                crate::views::picker::SessionDialogAction::Rename { id, title },
            ) => {
                self.session_rename = Some((id.clone(), title.clone()));
                self.requested
                    .push(Box::new(crate::views::basics::PromptDialog::new(
                        self.context.clone(),
                        SESSION_RENAME_DIALOG_ID,
                        "Rename session",
                        title,
                    )));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Session(
                crate::views::picker::SessionDialogAction::Delete { id, title },
            ) => {
                let (notice, level) = self.commit_selection(Selection::SessionDelete(id.clone()));
                self.toasts.push(Toast::new(
                    level,
                    if level == ToastLevel::Success {
                        format!("deleting session {title}")
                    } else {
                        notice
                    },
                ));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Submitted { text, .. }
                if dialog == SESSION_RENAME_DIALOG_ID =>
            {
                let Some((id, original_title)) = self.session_rename.take() else {
                    return EventResult::IGNORED;
                };
                let title = text.trim();
                if title.is_empty() {
                    self.session_rename = Some((id, original_title.clone()));
                    self.requested
                        .push(Box::new(crate::views::basics::PromptDialog::new(
                            self.context.clone(),
                            SESSION_RENAME_DIALOG_ID,
                            "Rename session",
                            original_title,
                        )));
                    self.toasts
                        .push(Toast::warning("session title cannot be empty"));
                    return EventResult::REDRAW;
                }
                let (notice, level) = self.commit_selection(Selection::SessionRename {
                    id,
                    title: title.to_owned(),
                });
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Cancelled
                if dialog == SESSION_RENAME_DIALOG_ID =>
            {
                self.session_rename = None;
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Selected { value, .. }
                if dialog == PLAN_START_CONFIRM_DIALOG_ID =>
            {
                if value == crate::views::basics::CONFIRM_VALUE {
                    self.select_collaboration_agent("plan");
                }
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Selected { value, .. }
                if dialog == WORK_START_CONFIRM_DIALOG_ID =>
            {
                if value == crate::views::basics::CONFIRM_VALUE {
                    self.select_collaboration_agent("orchestrator");
                }
                EventResult::REDRAW
            }
            // The confirmation is checked before the general `Selected` arm because
            // `adopt` routes on the dialog id and would report this one as unknown.
            crate::views::dialog::DialogOutcome::Selected { value, .. }
                if dialog == UNDO_CONFIRM_DIALOG_ID =>
            {
                if value == crate::views::basics::CONFIRM_VALUE {
                    // The same call the unconfirmed path made, reached only now. The
                    // shown text is `/undo` rather than a sentence about it, so the
                    // transcript records what the user invoked.
                    self.submit_to_driver(
                        String::from("/undo"),
                        PromptSubmission::Host(HostCommand::Undo),
                    );
                }
                EventResult::REDRAW
            }
            // Before the general `Selected` arm for the reason the undo confirmation is:
            // `adopt` routes on the dialog id and would report this one as unknown.
            crate::views::dialog::DialogOutcome::Selected { value, .. }
                if dialog == MESSAGE_ACTIONS_DIALOG_ID =>
            {
                // Taken, not read: the menu is answered once, and an index left behind would
                // let a later outcome act on a message the user is no longer pointing at.
                match self.message_menu.take() {
                    Some(index) => self.act_on_message(index, value),
                    None => EventResult::IGNORED,
                }
            }
            crate::views::dialog::DialogOutcome::Selected { value, .. } => {
                self.adopt(dialog, value)
            }
            // The dialog the external-editor fallback opened answered, so the text goes
            // where the real editor's result goes. Cancelling leaves the buffer alone,
            // which is `Ok(None)`'s behaviour in `drain_editor_results` — the two routes
            // agree on both outcomes, not just the successful one.
            crate::views::dialog::DialogOutcome::Submitted { text, .. }
                if dialog == EDITOR_FALLBACK_DIALOG_ID =>
            {
                self.editor.set_text(text);
                self.refresh_autocomplete();
                EventResult::REDRAW
            }
            // `§8.7`'s "Enter 复制全部并 toast": the report goes through the same `copy` the
            // editor's own copy uses, so it reports success, an empty payload and a
            // clipboard failure the one way this screen already reports them. A panel that
            // wrote to the clipboard itself would need a second set of those messages.
            crate::views::dialog::DialogOutcome::Submitted { text, .. }
                if dialog == crate::views::diagnostics::DEBUG_DIALOG_ID =>
            {
                self.copy(text.clone())
            }
            // Escape arrives as a cancelled outcome through the same routing a selection
            // takes, so no key is named here — which is the discipline this layer keeps.
            crate::views::dialog::DialogOutcome::Cancelled
                if dialog == crate::views::picker::THEME_DIALOG_ID =>
            {
                self.restore_theme()
            }
            crate::views::dialog::DialogOutcome::McpToggle(request) => {
                let delivered = self
                    .mcp_toggles
                    .as_ref()
                    .is_some_and(|sink| sink.try_send(request.clone()).is_ok());
                if !delivered {
                    self.transcript
                        .transcript_mut()
                        .push(Message::notice(format!(
                            "MCP server `{}` was not toggled: lifecycle worker is busy or unavailable",
                            request.server
                        )));
                }
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::JobCancel { job_id } => {
                let (notice, level) = self.commit_selection(Selection::JobCancel(job_id.clone()));
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::MemoryApply { id } => {
                let (notice, level) = self.commit_selection(Selection::MemoryApply(id.clone()));
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::MemoryReject { id } => {
                let (notice, level) = self.commit_selection(Selection::MemoryReject(id.clone()));
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::MemoryUndo { id } => {
                let (notice, level) = self.commit_selection(Selection::MemoryUndo(id.clone()));
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::MemoryEditRequested { id, content } => {
                self.memory_edit = Some(id.clone());
                self.requested
                    .push(Box::new(crate::views::basics::PromptDialog::new(
                        self.context.clone(),
                        crate::views::memory::EDIT_DIALOG_ID,
                        "Edit and approve memory",
                        content,
                    )));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Submitted { text, .. }
                if dialog == crate::views::memory::EDIT_DIALOG_ID =>
            {
                let Some(id) = self.memory_edit.take() else {
                    return EventResult::IGNORED;
                };
                let content = text.trim();
                if content.is_empty() {
                    self.memory_edit = Some(id);
                    self.toasts
                        .push(Toast::warning("memory content cannot be empty"));
                    return EventResult::REDRAW;
                }
                let (notice, level) = self.commit_selection(Selection::MemoryEditApply {
                    id,
                    content: content.to_owned(),
                });
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::Cancelled
                if dialog == crate::views::memory::EDIT_DIALOG_ID =>
            {
                self.memory_edit = None;
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::MemoryRemove { scope, content } => {
                let (notice, level) = self.commit_selection(Selection::MemoryRemove {
                    scope: *scope,
                    content: content.clone(),
                });
                self.toasts.push(Toast::new(level, notice));
                EventResult::REDRAW
            }
            crate::views::dialog::DialogOutcome::BackgroundCancel { execution_id } => {
                self.cancel_background(execution_id)
            }
            _ => EventResult::IGNORED,
        }
    }

    fn observe_modal(&mut self, active: Option<&'static str>) {
        self.modal = active;
        // Only tool-owned human-input prompts make the turn wait on the user. A picker or
        // help view is something the user opened while work continued, so suppressing the
        // pulse behind those would claim the turn had stopped when it had not.
        let awaiting = match active {
            Some(crate::views::permission::DIALOG_ID) => {
                Some(crate::views::message::AwaitingUser::Approval)
            }
            Some(crate::views::question::DIALOG_ID) => {
                Some(crate::views::message::AwaitingUser::Answer)
            }
            _ => None,
        };
        // Both projections, from one answer. The transcript retains the state for activity
        // rendering and the reply identity exposes it to the fixed footer; updating only one
        // would let the two surfaces disagree about who the turn is waiting on.
        self.transcript.transcript_mut().set_awaiting_user(awaiting);
        self.status.set_awaiting_user(awaiting);
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult {
        if action.name == "session_interrupt" && self.modal.is_some() {
            return self.request_interrupt_at(self.now_ms());
        }
        if self.transcript_full {
            if matches!(action.name, "messages_transcript" | "session_interrupt") {
                return self.toggle_transcript();
            }
            if action.name == "messages_copy"
                && let Some(text) = self.transcript.selected_text()
            {
                return self.copy(text);
            }
            return self.handle_view_action(action);
        }
        if self.autocomplete.is_open() {
            let autocomplete_action = match action.name {
                "prompt.autocomplete.prev"
                | "prompt.autocomplete.next"
                | "prompt.autocomplete.hide"
                | "prompt.autocomplete.select"
                | "prompt.autocomplete.complete" => Some(action.name),
                "input_submit" | "input_force_submit" | "prompt_submit" => {
                    Some("prompt.autocomplete.select")
                }
                "input_move_up" | "command_list" => Some("prompt.autocomplete.prev"),
                "input_move_down" => Some("prompt.autocomplete.next"),
                "session_interrupt" => Some("prompt.autocomplete.hide"),
                _ => None,
            };
            if let Some(autocomplete_action) = autocomplete_action {
                let autocomplete = self.autocomplete_step(autocomplete_action);
                if action.name == "session_interrupt" {
                    return autocomplete.merge(self.request_interrupt_at(self.now_ms()));
                }
                return autocomplete;
            }
        }
        if action.name == "session_interrupt" {
            return self.request_interrupt_at(self.now_ms());
        }
        // `ctrl+c` and `ctrl+d` are each claimed by the `input` scope before `app`,
        // so a screen that only watched for `app_exit` could never be left. Asking
        // the keymap whether the *chord* is an exit chord — rather than matching the
        // action names the resolution happened to produce — is what makes this
        // independent of which scope won, and it is why `delete`, the other spelling
        // of `input_delete`, no longer quits an application it was never bound to.
        let editor_owns_chord =
            !self.editor.text().is_empty() && matches!(action.name, "input_clear" | "input_delete");
        if action.name == APP_EXIT || (is_exit_request(event) && !editor_owns_chord) {
            return self.request_exit();
        }
        if action.name == "messages_copy"
            && let Some(text) = self.transcript.selected_text()
        {
            return self.copy(text);
        }
        if self.handle_view_action(action).handled {
            return EventResult::REDRAW;
        }
        match self.editor.handle_action(action) {
            EditorSignal::None => EventResult::IGNORED,
            EditorSignal::Submit(text) => {
                self.submit(text, action.name == "input_force_submit");
                self.autocomplete.hide();
                EventResult::REDRAW
            }
            EditorSignal::Copy(text) => self.copy(text),
            EditorSignal::OpenExternalEditor => self.request_external_editor(),
            EditorSignal::Changed => {
                self.refresh_autocomplete();
                EventResult::REDRAW
            }
            EditorSignal::Paste => self.paste_from_clipboard(),
        }
    }
}

/// The scope chain a session screen resolves keys in, outermost last.
///
/// `input` and `prompt` before `app` so a binding the editor claims wins over an
/// application-wide one on the same chord, and `app` last so `app_exit` still
/// resolves while the prompt has focus.
/// Every scope whose actions this screen can act on, plus `app` last.
///
/// A scope missing from this list is the quietest possible dead key: the binding table
/// has the row, the chord is spelled, [`SessionScreen::handle_action`] has an arm for
/// it — and [`crate::keybind::KeyDispatcher`] never resolves the press, because
/// resolution is scoped. The four pickers were unreachable for two independent reasons
/// at once, and this was the second one; a screen that handles an action must therefore
/// list the scope that action lives in.
#[must_use]
pub fn scopes() -> Vec<String> {
    [
        // `input` and `prompt` first, so a chord the editor claims wins over an
        // application-wide one on the same keys.
        "input",
        "prompt",
        // `history` stays after `input` in the static chain, preserving the rule above that
        // editor bindings win ordinary collisions. Its complete scope is only `up` and
        // `down`, so registering it cannot consume a typeable character. At a buffer's
        // vertical edge `focused_scopes` temporarily promotes it; the editor then decides
        // from direction whether that arrow moves inward or crosses into history.
        "history",
        // `editor` with them, because `editor_open` *is* a prompt action — its command is
        // `prompt.editor` and it opens `$EDITOR` on the buffer the prompt owns — so it
        // belongs beside the family above rather than among the surfaces below.
        //
        // Safe at any position, which is the part worth stating. The scope carries exactly
        // one row, `editor_open` on `<leader>e`, and no other row in the table spells
        // `<leader>e`, so this cannot take a chord from a scope before or after it. It also
        // cannot do what `diff` below does: a leader sequence opens with `ctrl+x`, which no
        // text entry produces, so registering this scope costs no typeable character.
        //
        // Unregistered it was the quietest possible dead key: `ctrl+x` resolved to
        // `Pending`, the `e` then matched nothing, fell through to the editor and was
        // inserted — `ctrl+x e` left `beforee` in the prompt, and the contained-editor
        // stack behind it could not be opened by any means.
        "editor",
        "messages",
        "model",
        "agent",
        "session",
        "theme",
        "sidebar",
        "mcp",
        "tool",
        "display",
        "tips",
        "command",
        "help",
        "background",
        "memory",
        // `status` and `debug` cost no typeable character, which is the question this list
        // exists to answer. Each is a one-row scope — `status_view` on `<leader>s` and an
        // unbound `debug_view` — so neither can claim a bare letter the way `diff` below
        // does, and no other row in the table spells `<leader>s`.
        "status",
        "debug",
        // `variant` costs no typeable character either: the complete scope is
        // `variant_cycle` on `alt+t` plus an unbound `variant_list`, and a modified chord
        // is not something text entry produces. Unregistered, `ctrl+t` was the same dead
        // key `editor` above describes — the table advertised "Cycle model variants" and
        // no scope claimed the chord, so it resolved to `Unmatched` and fell through to
        // the editor, which inserts nothing for it. `views/slash.rs:267` recorded the
        // scope's shape while nothing was reaching it. `ctrl+t` belongs to `transcript`.
        "variant",
        // `diff` after `input` and `messages`, and only for `diff_open`'s sake. The scope
        // also carries the viewer's own bare characters — `q`, `n`, `p`, `d`, `v`, `s`,
        // `b`, `[`, `]`, `?`, `E` — which resolve here whether or not the viewer is open.
        // That list is derived and asserted by
        // `exposing_the_diff_scope_did_not_stop_its_bare_letters_being_typed`, because
        // this comment named only nine of the eleven for as long as it existed. That is
        // survivable, and only because of two facts together: this screen returns
        // `IGNORED` for every diff action except `diff_open`, and an unhandled action
        // falls through to the editor, which inserts the character. Give this screen an
        // arm for one of those letters and the letter stops being typeable.
        "diff", // `app` last, so `app_exit` still resolves while the prompt has focus.
        "app",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
