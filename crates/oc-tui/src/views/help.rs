//! The help view: every action, its keys, and its description.
//!
//! # It is generated from the keymap, never from a written list
//!
//! `packages/tui/src/ui/dialog-help.tsx` renders the live bindings. A hand-written
//! help text is wrong the moment a user rebinds anything, and wrong *silently*, which
//! is the worst kind. So every row here comes from [`crate::keybind::Keymap`]:
//! [`crate::keybind::Keymap::sequences`] for the keys and
//! [`crate::keybind::Definition::description`] for the text.
//!
//! # Grouping is by scope, because scope is what decides whether a key works
//!
//! The binding table's `scope` column is not a category label — it is the condition
//! under which the key resolves at all (`keybind.ts`). Grouping by it means the help
//! answers the question a user actually has: "what can I press *here*".
//!
//! # An unbound action still gets a row
//!
//! A user who unbound something needs to see that it exists and has no key, or they
//! will conclude the feature is gone.

use crate::keybind::{DEFINITIONS, Definition, Keymap};
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep};
use crate::views::{ViewContext, padded};
use crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "help_tests.rs"]
mod tests;

/// The dialog id [`DialogOutcome`] carries for the help view.
pub const DIALOG_ID: &str = "help_show";

/// What an action with no key renders as.
pub const UNBOUND: &str = "(unbound)";

/// One help row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The action's name.
    pub action: &'static str,
    /// Its key spellings, comma-joined, or [`UNBOUND`].
    pub keys: String,
    /// Its description.
    pub description: &'static str,
}

/// Every action grouped by scope, with the keys the keymap actually resolved.
#[must_use]
pub fn entries(keymap: &Keymap) -> BTreeMap<&'static str, Vec<Entry>> {
    let mut grouped: BTreeMap<&'static str, Vec<Entry>> = BTreeMap::new();
    for definition in DEFINITIONS.iter().filter(|row| !row.is_leader()) {
        let sequences = keymap.sequences(definition.name);
        let keys = if sequences.is_empty() {
            UNBOUND.to_owned()
        } else {
            sequences.join(", ")
        };
        grouped.entry(definition.scope).or_default().push(Entry {
            action: definition.name,
            keys,
            description: definition.description,
        });
    }
    grouped
}

/// The help dialog.
pub struct HelpView {
    context: ViewContext,
    grouped: BTreeMap<&'static str, Vec<Entry>>,
    /// First rendered row of the flattened list.
    offset: usize,
    rows: usize,
    filter: String,
}

impl HelpView {
    /// A help view over `keymap`.
    #[must_use]
    pub fn new(context: ViewContext, keymap: &Keymap) -> Self {
        Self {
            context,
            grouped: entries(keymap),
            offset: 0,
            rows: 16,
            filter: String::new(),
        }
    }

    /// The scopes present.
    #[must_use]
    pub fn scopes(&self) -> Vec<&'static str> {
        self.grouped.keys().copied().collect()
    }

    /// Every row, flattened in scope order with a heading before each group.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (scope, group) in &self.grouped {
            let matched = group
                .iter()
                .filter(|entry| self.matches(entry))
                .collect::<Vec<_>>();
            if matched.is_empty() {
                continue;
            }
            rows.push(Row::Heading(scope));
            for entry in matched {
                rows.push(Row::Entry(entry.clone()));
            }
        }
        rows
    }

    fn matches(&self, entry: &Entry) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let needle = self.filter.to_lowercase();
        entry.action.to_lowercase().contains(&needle)
            || entry.description.to_lowercase().contains(&needle)
            || entry.keys.to_lowercase().contains(&needle)
    }

    /// The current filter.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Set the filter and reset the scroll.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        self.offset = 0;
    }
}

/// A help row: a scope heading or an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A scope heading.
    Heading(&'static str),
    /// One action.
    Entry(Entry),
}

impl Dialog for HelpView {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        if self.filter.is_empty() {
            String::from("Keybindings")
        } else {
            format!("Keybindings — {}", self.filter)
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let rows = self.rows();
        let total = rows.len();
        if self.offset >= total {
            self.offset = total.saturating_sub(1);
        }
        rows.into_iter()
            .skip(self.offset)
            .take(self.rows)
            .map(|row| match row {
                Row::Heading(scope) => padded(&format!(" {scope}"), width, self.context.title()),
                Row::Entry(entry) => {
                    let style = if entry.keys == UNBOUND {
                        self.context.muted()
                    } else {
                        self.context.text()
                    };
                    Line::from(vec![
                        Span::styled(format!("   {:<22}", entry.keys), self.context.accent()),
                        Span::styled(entry.description.to_owned(), style),
                        Span::styled(
                            " ".repeat(
                                usize::from(width)
                                    .saturating_sub(25 + entry.description.chars().count()),
                            ),
                            style,
                        ),
                    ])
                }
            })
            .collect()
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "scroll"), ("esc", "close")]
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.select.prev" => {
                self.offset = self.offset.saturating_sub(1);
                DialogStep::Redraw
            }
            "dialog.select.next" => {
                self.offset += 1;
                DialogStep::Redraw
            }
            "dialog.select.page_up" => {
                self.offset = self.offset.saturating_sub(self.rows);
                DialogStep::Redraw
            }
            "dialog.select.page_down" => {
                self.offset += self.rows;
                DialogStep::Redraw
            }
            "dialog.select.home" => {
                self.offset = 0;
                DialogStep::Redraw
            }
            "app_exit" | "help_show" | "dialog.select.submit" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            "input_backspace" => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.set_filter(&filter);
                DialogStep::Redraw
            }
            _ => {
                if let Some(character) = crate::views::permission::typed_character(event) {
                    let filter = format!("{}{character}", self.filter);
                    self.set_filter(&filter);
                    return DialogStep::Redraw;
                }
                DialogStep::Ignored
            }
        }
    }
}
