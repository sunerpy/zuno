//! Auditable resident-memory candidates and current entries.

use crate::keybind::Definition;
use crate::views::ambient::WorkState;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::{ViewContext, truncate};
use crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};
use zuno_types::{
    MemoryAction, MemoryCandidateProjection, MemoryCandidateStatus, MemoryEntryProjection,
};

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;

/// Stable dialog identifier used by slash routing and outcome dispatch.
pub const DIALOG_ID: &str = "memory_view";
/// Prompt opened to edit a pending candidate before applying it.
pub const EDIT_DIALOG_ID: &str = "memory_edit";
/// Honest empty-state copy.
pub const EMPTY: &str = "no memory candidates or resident entries";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Candidate(MemoryCandidateProjection),
    Entry(MemoryEntryProjection),
}

fn status_glyph(status: MemoryCandidateStatus) -> &'static str {
    match status {
        MemoryCandidateStatus::Pending => "○",
        MemoryCandidateStatus::Applying => "…",
        MemoryCandidateStatus::Undoing => "…",
        MemoryCandidateStatus::Applied => "✓",
        MemoryCandidateStatus::Rejected => "×",
        MemoryCandidateStatus::Undone => "↶",
        MemoryCandidateStatus::Failed => "!",
        MemoryCandidateStatus::Uncertain => "?",
    }
}

impl Item {
    fn headline(&self, width: usize) -> String {
        match self {
            Self::Candidate(candidate) => {
                let content = candidate
                    .content
                    .as_deref()
                    .or(candidate.old_text.as_deref())
                    .unwrap_or(candidate.reason.as_str());
                truncate(
                    &format!(
                        "{} {} {} · {}",
                        status_glyph(candidate.status),
                        candidate.scope.as_str(),
                        candidate.action.as_str(),
                        content
                    ),
                    width,
                )
            }
            Self::Entry(entry) => truncate(
                &format!("✓ {} saved · {}", entry.scope.as_str(), entry.content),
                width,
            ),
        }
    }
}

/// Live memory manager over the shared durable work-state projection.
pub struct MemoryView {
    context: ViewContext,
    state: WorkState,
    cursor: usize,
    expanded: bool,
    confirm_remove: Option<(zuno_types::MemoryScope, String)>,
}

impl MemoryView {
    #[must_use]
    pub fn new(context: ViewContext, state: WorkState) -> Self {
        Self {
            context,
            state,
            cursor: 0,
            expanded: false,
            confirm_remove: None,
        }
    }

    fn items(&self) -> Vec<Item> {
        let state = self.state.snapshot();
        state
            .memory_candidates
            .into_iter()
            .map(Item::Candidate)
            .chain(state.memory_entries.into_iter().map(Item::Entry))
            .collect()
    }

    fn selected(&self) -> Option<Item> {
        self.items().into_iter().nth(self.cursor)
    }

    fn clamp_cursor(&mut self, length: usize) {
        self.cursor = self.cursor.min(length.saturating_sub(1));
    }

    fn step(&mut self, delta: isize) -> DialogStep {
        let length = self.items().len();
        self.confirm_remove = None;
        if length < 2 {
            return DialogStep::Redraw;
        }
        let length = isize::try_from(length).unwrap_or(isize::MAX);
        let moved = isize::try_from(self.cursor)
            .unwrap_or_default()
            .saturating_add(delta);
        self.cursor = usize::try_from(moved.rem_euclid(length)).unwrap_or_default();
        DialogStep::Redraw
    }

    fn apply(&mut self) -> DialogStep {
        let Some(Item::Candidate(candidate)) = self.selected() else {
            return DialogStep::Redraw;
        };
        if !matches!(
            candidate.status,
            MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
        ) {
            return DialogStep::Redraw;
        }
        DialogStep::Emitted(DialogOutcome::MemoryApply { id: candidate.id })
    }

    fn reject(&mut self) -> DialogStep {
        let Some(Item::Candidate(candidate)) = self.selected() else {
            return DialogStep::Redraw;
        };
        if !matches!(
            candidate.status,
            MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
        ) {
            return DialogStep::Redraw;
        }
        DialogStep::Emitted(DialogOutcome::MemoryReject { id: candidate.id })
    }

    fn undo(&mut self) -> DialogStep {
        let Some(Item::Candidate(candidate)) = self.selected() else {
            return DialogStep::Redraw;
        };
        if candidate.status != MemoryCandidateStatus::Applied {
            return DialogStep::Redraw;
        }
        DialogStep::Emitted(DialogOutcome::MemoryUndo { id: candidate.id })
    }

    fn edit(&mut self) -> DialogStep {
        let Some(Item::Candidate(candidate)) = self.selected() else {
            return DialogStep::Redraw;
        };
        if !matches!(candidate.action, MemoryAction::Add | MemoryAction::Replace)
            || !matches!(
                candidate.status,
                MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
            )
        {
            return DialogStep::Redraw;
        }
        DialogStep::Emitted(DialogOutcome::MemoryEditRequested {
            id: candidate.id,
            content: candidate.content.unwrap_or_default(),
        })
    }

    fn remove(&mut self) -> DialogStep {
        let Some(Item::Entry(entry)) = self.selected() else {
            self.confirm_remove = None;
            return DialogStep::Redraw;
        };
        let target = (entry.scope, entry.content);
        if self.confirm_remove.as_ref() != Some(&target) {
            self.confirm_remove = Some(target);
            self.expanded = true;
            return DialogStep::Redraw;
        }
        self.confirm_remove = None;
        DialogStep::Emitted(DialogOutcome::MemoryRemove {
            scope: target.0,
            content: target.1,
        })
    }

    fn details(&self, item: &Item, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut row = |label: &str, value: String| {
            lines.push(Line::from(Span::styled(
                truncate(&format!("  {label} {value}"), width),
                self.context.muted(),
            )));
        };
        match item {
            Item::Candidate(candidate) => {
                row("id", candidate.id.clone());
                row("scope", candidate.scope.as_str().to_owned());
                row("action", candidate.action.as_str().to_owned());
                row("status", candidate.status.as_str().to_owned());
                row(
                    "confidence",
                    format!("{:.0}%", f64::from(candidate.confidence) / 100.0),
                );
                row("source", candidate.source.as_str().to_owned());
                row("reason", candidate.reason.clone());
                if let Some(content) = &candidate.content {
                    row("content", content.clone());
                }
                if let Some(old_text) = &candidate.old_text {
                    row("locator", old_text.clone());
                }
                if let Some(session) = &candidate.source_session_id {
                    row("session", session.clone());
                }
                if let Some(message) = &candidate.source_message_id {
                    row("message", message.clone());
                }
                if let Some(error) = &candidate.error {
                    row("diagnostic", error.clone());
                }
            }
            Item::Entry(entry) => {
                row("scope", entry.scope.as_str().to_owned());
                row("content", entry.content.clone());
                row("source", "resident memory file".to_owned());
            }
        }
        lines
    }
}

impl Dialog for MemoryView {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        let length = self.items().len();
        if length == 0 {
            "Memory".to_owned()
        } else {
            format!("Memory  {}/{}", self.cursor.saturating_add(1), length)
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let items = self.items();
        self.clamp_cursor(items.len());
        let body = usize::from(width.saturating_sub(2)).max(1);
        if items.is_empty() {
            return vec![Line::from(Span::styled(
                EMPTY.to_owned(),
                self.context.muted(),
            ))];
        }
        let mut lines = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let marker = if index == self.cursor { "›" } else { " " };
                Line::from(Span::styled(
                    format!("{marker} {}", item.headline(body.saturating_sub(2))),
                    if index == self.cursor {
                        self.context.element()
                    } else {
                        self.context.muted()
                    },
                ))
            })
            .collect::<Vec<_>>();
        if self.expanded
            && let Some(item) = items.get(self.cursor)
        {
            lines.push(Line::default());
            lines.extend(self.details(item, body));
        }
        if let Some((scope, content)) = &self.confirm_remove {
            lines.push(Line::from(Span::styled(
                truncate(
                    &format!(
                        "press x again to remove {} memory: {}",
                        scope.as_str(),
                        content
                    ),
                    body,
                ),
                self.context.warning(),
            )));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("←→", "item"),
            ("enter", "details"),
            ("a", "approve"),
            ("e", "edit+approve"),
            ("r", "reject"),
            ("u", "undo"),
            ("x x", "remove saved"),
            ("esc", "close"),
        ]
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::XLarge
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["dialog.memory", "session"]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        match action.name {
            "session_child_cycle" | "dialog.select.next" => self.step(1),
            "session_child_cycle_reverse" | "dialog.select.prev" => self.step(-1),
            "dialog.select.home" => {
                self.cursor = 0;
                self.confirm_remove = None;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.items().len().saturating_sub(1);
                self.confirm_remove = None;
                DialogStep::Redraw
            }
            "dialog.select.submit" => {
                self.expanded = !self.expanded;
                self.confirm_remove = None;
                DialogStep::Redraw
            }
            "memory_apply" => self.apply(),
            "memory_reject" => self.reject(),
            "memory_edit" => self.edit(),
            "memory_undo" => self.undo(),
            "memory_remove" => self.remove(),
            "session_parent" | "session_interrupt" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
        }
    }
}
