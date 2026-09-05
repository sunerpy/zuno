//! Per-session controls for using memories and generating new learning.

use crate::keybind::Definition;
use crate::views::ambient::WorkState;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::{ViewContext, truncate};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use zuno_types::{SessionMemoryGeneration, SessionMemoryPolicyProjection};

#[cfg(test)]
#[path = "memories_tests.rs"]
mod tests;

/// Stable dialog identifier used by slash routing.
pub const DIALOG_ID: &str = "memory_policy_view";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Use,
    Generate,
}

/// Live session-memory policy editor over the shared durable work projection.
pub struct MemoryPolicyView {
    context: ViewContext,
    state: WorkState,
    cursor: usize,
}

impl MemoryPolicyView {
    #[must_use]
    pub fn new(context: ViewContext, state: WorkState) -> Self {
        Self {
            context,
            state,
            cursor: 0,
        }
    }

    const fn selected(&self) -> Item {
        if self.cursor == 0 {
            Item::Use
        } else {
            Item::Generate
        }
    }

    fn policy(&self) -> SessionMemoryPolicyProjection {
        self.state.snapshot().memory_policy
    }

    fn step(&mut self, delta: isize) -> DialogStep {
        let moved = isize::try_from(self.cursor)
            .unwrap_or_default()
            .saturating_add(delta);
        self.cursor = usize::try_from(moved.rem_euclid(2)).unwrap_or_default();
        DialogStep::Redraw
    }

    fn toggle(&self) -> DialogStep {
        let policy = self.policy();
        match self.selected() {
            Item::Use => DialogStep::Emitted(DialogOutcome::MemoryUseSet {
                enabled: !policy.use_memories,
            }),
            Item::Generate if policy.generation == SessionMemoryGeneration::Excluded => {
                DialogStep::Redraw
            }
            Item::Generate => DialogStep::Emitted(DialogOutcome::MemoryGenerationSet {
                enabled: policy.generation != SessionMemoryGeneration::Enabled,
            }),
        }
    }

    fn headline(item: Item, policy: &SessionMemoryPolicyProjection, width: usize) -> String {
        let text = match item {
            Item::Use => format!(
                "{} Use resident Memory and retrieved Experience",
                if policy.use_memories { "[x]" } else { "[ ]" }
            ),
            Item::Generate => match policy.generation {
                SessionMemoryGeneration::Enabled => {
                    "[x] Generate learning from this session".to_owned()
                }
                SessionMemoryGeneration::Disabled => {
                    "[ ] Generate learning from this session".to_owned()
                }
                SessionMemoryGeneration::Excluded => {
                    "[!] Learning generation excluded for this session".to_owned()
                }
            },
        };
        truncate(&text, width)
    }
}

impl Dialog for MemoryPolicyView {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        "Session memories".to_owned()
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let policy = self.policy();
        let body = usize::from(width.saturating_sub(2)).max(1);
        let mut lines = [Item::Use, Item::Generate]
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let marker = if index == self.cursor { "›" } else { " " };
                Line::from(Span::styled(
                    format!(
                        "{marker} {}",
                        Self::headline(item, &policy, body.saturating_sub(2))
                    ),
                    if index == self.cursor {
                        self.context.element()
                    } else {
                        self.context.muted()
                    },
                ))
            })
            .collect::<Vec<_>>();
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            truncate(
                &format!(
                    "revision {} · source {}",
                    policy.revision,
                    policy.source.as_deref().unwrap_or("configuration default")
                ),
                body,
            ),
            self.context.muted(),
        )));
        if let Some(reason) = policy.reason {
            lines.push(Line::from(Span::styled(
                truncate(&format!("reason {reason}"), body),
                if policy.generation == SessionMemoryGeneration::Excluded {
                    self.context.warning()
                } else {
                    self.context.muted()
                },
            )));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("←→", "setting"), ("enter", "toggle"), ("esc", "close")]
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::Large
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["dialog.select", "session"]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        match action.name {
            "session_child_cycle" | "dialog.select.next" => self.step(1),
            "session_child_cycle_reverse" | "dialog.select.prev" => self.step(-1),
            "dialog.select.home" => {
                self.cursor = 0;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = 1;
                DialogStep::Redraw
            }
            "dialog.select.submit" => self.toggle(),
            "session_parent" | "session_interrupt" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
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
            MouseEventKind::ScrollUp => self.step(-1),
            MouseEventKind::ScrollDown => self.step(1),
            MouseEventKind::Up(MouseButton::Left) => {
                let index = usize::from(event.row.saturating_sub(body.top()));
                if index >= 2 {
                    return DialogStep::Ignored;
                }
                self.cursor = index;
                self.toggle()
            }
            _ => DialogStep::Ignored,
        }
    }
}
