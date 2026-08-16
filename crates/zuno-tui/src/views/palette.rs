//! The command palette: every action, findable by name, reachable without a key.
//!
//! # Why this is the load-bearing surface and not a convenience
//!
//! Forty-three of the binding table's rows ship with `keys: "none"`, faithfully to
//! upstream (`packages/tui/src/config/keybind.ts`) — `help_show`, `mcp_list`,
//! `prompt_skills`, `display_thinking` and `tool_details` among them. Those are not
//! oversights: upstream's answer is that the palette is how an unbound action is
//! invoked. Without a palette, a Rust port that copies the table faithfully ships a
//! third of its capabilities with no way to reach them at all — which is the
//! built-but-unreachable defect, in bulk.
//!
//! So this dialog lists every action, bound or not, and resolving it emits the
//! action's name for the host to dispatch. That is what makes the welcome screen's
//! "via the palette" route honest.
//!
//! # It is a [`SelectDialog`], not a fourth list implementation
//!
//! Filtering, cursor movement, paging and submission are already correct in
//! [`crate::views::picker::SelectDialog`], and its ranking is the same one
//! autocomplete uses. A palette with its own list widget would be a fifth place for
//! the paging arithmetic to be wrong.
//!
//! # Ordering puts what a user can press first
//!
//! Bound actions before unbound ones, and within each group by scope then name. A
//! user scanning the palette for the first time is looking for what the keyboard can
//! already do; an unbound action is something they will invoke from here every time,
//! so it does not need to be at the top.

use crate::keybind::{DEFINITIONS, Keymap};
use crate::views::ViewContext;
use crate::views::picker::{Item, SelectDialog};

#[cfg(test)]
#[path = "palette_tests.rs"]
mod tests;

/// The dialog id [`crate::views::dialog::DialogOutcome`] carries for the palette.
pub const DIALOG_ID: &str = "command_list";

/// How the palette describes an action nobody has bound.
///
/// Named rather than blank so a row's description says *why* it has no key, which is
/// the question a user reading a keyless row actually has.
pub const NO_KEY: &str = "no key · run from here";

/// Rows the palette shows at once.
///
/// Larger than a picker's ten because the palette is the one list a user browses
/// rather than targets, and the binding table has over a hundred rows.
pub const ROWS: usize = 16;

/// One palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The action's name, and the value the outcome carries.
    pub action: &'static str,
    /// Its description, as the binding table states it.
    pub description: &'static str,
    /// The scope the key resolves in.
    pub scope: &'static str,
    /// The key spellings, comma-joined, or empty when unbound.
    pub keys: String,
}

impl Entry {
    /// Whether a key press can reach this action without the palette.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        !self.keys.is_empty()
    }
}

/// Every invocable action, bound ones first.
///
/// Leader rows are excluded because a leader is a prefix, not an action: dispatching
/// `leader` would do nothing and a row that does nothing is worse than an absent one.
#[must_use]
pub fn entries(keymap: &Keymap) -> Vec<Entry> {
    let mut rows = DEFINITIONS
        .iter()
        .filter(|definition| !definition.is_leader())
        .map(|definition| Entry {
            action: definition.name,
            description: definition.description,
            scope: definition.scope,
            keys: keymap.sequences(definition.name).join(", "),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .is_bound()
            .cmp(&left.is_bound())
            .then_with(|| left.scope.cmp(right.scope))
            .then_with(|| left.action.cmp(right.action))
    });
    rows
}

/// The command palette.
#[must_use]
pub fn palette(context: ViewContext, keymap: &Keymap) -> SelectDialog {
    let items = entries(keymap)
        .into_iter()
        .map(|entry| {
            let keys = if entry.is_bound() {
                entry.keys.clone()
            } else {
                NO_KEY.to_owned()
            };
            // The description carries the scope and the key because a palette row has
            // one line: a user choosing between `messages_last` and `session_list`
            // needs to know which surface each belongs to.
            Item::new(entry.description)
                .described(format!("{}  ·  {keys}", entry.scope))
                .valued(entry.action)
        })
        .collect();
    SelectDialog::new(DIALOG_ID, "Commands", context, items).with_rows(ROWS)
}
