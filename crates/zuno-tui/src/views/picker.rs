//! The picker family: session, model, agent, and theme.
//!
//! # One list, four callers
//!
//! Upstream ships a separate component per picker
//! (`component/dialog-session-list.tsx`, `dialog-model.tsx`, `dialog-agent.tsx`,
//! `dialog-theme-list.tsx`) over one shared `ui/dialog-select.tsx`. The shared part
//! is the whole behaviour — filter, cursor, paging, submit — so here there is one
//! [`SelectDialog`] and four constructors. Four copies of a list widget is four
//! places for the paging arithmetic to be wrong.
//!
//! # Filtering is the same ranking autocomplete uses
//!
//! [`crate::views::autocomplete::score`] rather than a second scoring rule, because a
//! user who learns that typing `sess` finds `session_list` in one surface should not
//! have to learn something different in the other.
//!
//! # The theme picker previews, and that is not cosmetic
//!
//! It renders todo 75's [`crate::theme::PaletteSampleView`] beside the list. A theme
//! name means nothing; a preview is the only way to choose one. Reusing that view
//! also means the picker is covered by the 33 committed palette snapshots.

use crate::keybind::Definition;
use crate::theme::{Mode, Resolved, ThemeRegistry};
use crate::views::autocomplete::score;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep};
use crate::views::{ViewContext, padded};
use crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};

#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;

/// The dialog id for the session picker.
pub const SESSION_DIALOG_ID: &str = "session_list";
/// The dialog id for the model picker.
pub const MODEL_DIALOG_ID: &str = "model_list";
/// The dialog id for the agent picker.
pub const AGENT_DIALOG_ID: &str = "agent_list";
/// The dialog id for the theme picker.
pub const THEME_DIALOG_ID: &str = "theme_list";
/// The dialog id for the MCP server list.
pub const MCP_DIALOG_ID: &str = "mcp_list";

/// The MCP servers, as a filterable list.
///
/// A list and not a picker, strictly speaking: selecting a row does nothing, because an
/// MCP server is an ambient fact rather than a choice. It exists because the sidebar
/// shows a *summary* — `2 up, 1 failed` — and a failure's reason is what a user acts on,
/// which does not fit in the panel's remaining columns. The same [`SelectDialog`] is
/// reused rather than a bespoke view so that filtering by name behaves the way it does
/// everywhere else.
#[must_use]
pub fn mcp_list(
    context: ViewContext,
    servers: Vec<crate::views::ambient::Service>,
) -> SelectDialog {
    let items = servers
        .into_iter()
        .map(|service| {
            let detail = if service.detail.is_empty() {
                String::new()
            } else {
                format!("{} · {}", service.health.glyph(), service.detail)
            };
            Item::new(service.name).described(detail)
        })
        .collect();
    SelectDialog::new(MCP_DIALOG_ID, "MCP servers", context, items)
}

/// One row of a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What the user sees.
    pub label: String,
    /// Secondary text.
    pub description: String,
    /// The opaque value reported in [`DialogOutcome::Selected`].
    pub value: String,
}

impl Item {
    /// A row whose label is also its value.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            value: label.clone(),
            label,
            description: String::new(),
        }
    }

    /// Attach a description.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Override the reported value.
    #[must_use]
    pub fn valued(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }
}

/// A per-row preview renderer.
///
/// A named alias because it appears in a field, a builder parameter, and a boxed
/// value; spelled out three times it drifts.
pub type PreviewFn = dyn Fn(&Item, &ViewContext) -> Vec<Line<'static>> + Send;

/// A filterable list dialog.
pub struct SelectDialog {
    id: &'static str,
    heading: String,
    context: ViewContext,
    items: Vec<Item>,
    /// Indices into `items`, in ranked order.
    filtered: Vec<usize>,
    filter: String,
    cursor: usize,
    rows: usize,
    /// A per-row preview, drawn under the list. The theme picker's reason to exist.
    preview: Option<Box<PreviewFn>>,
}

impl SelectDialog {
    /// A picker over `items`.
    #[must_use]
    pub fn new(
        id: &'static str,
        heading: impl Into<String>,
        context: ViewContext,
        items: Vec<Item>,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            id,
            heading: heading.into(),
            context,
            items,
            filtered,
            filter: String::new(),
            cursor: 0,
            rows: 10,
            preview: None,
        }
    }

    /// Attach a preview renderer for the highlighted row.
    #[must_use]
    pub fn with_preview(
        mut self,
        preview: impl Fn(&Item, &ViewContext) -> Vec<Line<'static>> + Send + 'static,
    ) -> Self {
        self.preview = Some(Box::new(preview));
        self
    }

    /// Show at most `rows` list rows at once.
    #[must_use]
    pub const fn with_rows(mut self, rows: usize) -> Self {
        self.rows = rows;
        self
    }

    /// Start with the cursor on the item whose value is `value`.
    #[must_use]
    pub fn selecting(mut self, value: &str) -> Self {
        if let Some(position) = self
            .filtered
            .iter()
            .position(|index| self.items[*index].value == value)
        {
            self.cursor = position;
        }
        self
    }

    /// The current filter text.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The highlighted row.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The rows that pass the filter.
    #[must_use]
    pub fn visible(&self) -> Vec<&Item> {
        self.filtered
            .iter()
            .map(|index| &self.items[*index])
            .collect()
    }

    /// The highlighted item.
    #[must_use]
    pub fn selected(&self) -> Option<&Item> {
        self.filtered
            .get(self.cursor)
            .map(|index| &self.items[*index])
    }

    /// Set the filter and re-rank.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        let mut ranked = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                // The description is searched too, matching upstream's behaviour for
                // slash commands (`autocomplete.tsx:506-507`): a user looking for
                // "the one that forks" does not know it is called `session_fork`.
                //
                // And the value, at the same weight as the description. A model's label is
                // its display name (`Claude Haiku 4.5`) while its value is the id the
                // engine takes (`…claude-haiku-4-5-20251001-v1:0`), and a user who knows
                // the id — because that is what `--model` and the config file spell —
                // otherwise types it and is told there are no matches. "No results" and
                // "searching the wrong field" look identical from the outside.
                let best = score(&item.label, filter)
                    .into_iter()
                    .chain(score(&item.description, filter).map(|value| value / 2))
                    .chain(score(&item.value, filter).map(|value| value / 2))
                    .max()?;
                Some((best, index))
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.filtered = ranked.into_iter().map(|(_, index)| index).collect();
        self.cursor = self.cursor.min(self.filtered.len().saturating_sub(1));
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let length = self.filtered.len() as isize;
        // Wrapping rather than clamping: a list of five options is faster to reach
        // the end of by going up, and upstream's select wraps.
        self.cursor = ((self.cursor as isize + delta).rem_euclid(length)) as usize;
    }
}

impl Dialog for SelectDialog {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        if self.filter.is_empty() {
            format!("{} ({})", self.heading, self.filtered.len())
        } else {
            format!(
                "{} ({}) — {}",
                self.heading,
                self.filtered.len(),
                self.filter
            )
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.filtered.is_empty() {
            lines.push(padded(" no matches", width, self.context.muted()));
            return lines;
        }
        // Keep the cursor in view by scrolling the window, not the cursor.
        let first = self.cursor.saturating_sub(self.rows.saturating_sub(1));
        for (position, index) in self.filtered.iter().enumerate().skip(first).take(self.rows) {
            let item = &self.items[*index];
            let style = if position == self.cursor {
                self.context.selected()
            } else {
                self.context.text()
            };
            let marker = if position == self.cursor { ">" } else { " " };
            let body = if item.description.is_empty() {
                format!(" {marker} {}", item.label)
            } else {
                format!(" {marker} {}  {}", item.label, item.description)
            };
            lines.push(padded(&body, width, style));
        }
        if let (Some(preview), Some(item)) = (self.preview.as_ref(), self.selected()) {
            lines.push(padded("", width, self.context.surface()));
            lines.extend(preview(item, &self.context));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "move"),
            ("pgup/pgdn", "page"),
            ("enter", "select"),
            ("esc", "cancel"),
        ]
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.select.prev" => {
                self.move_cursor(-1);
                DialogStep::Redraw
            }
            "dialog.select.next" => {
                self.move_cursor(1);
                DialogStep::Redraw
            }
            "dialog.select.page_up" => {
                self.move_cursor(-(self.rows as isize));
                DialogStep::Redraw
            }
            "dialog.select.page_down" => {
                self.move_cursor(self.rows as isize);
                DialogStep::Redraw
            }
            "dialog.select.home" => {
                self.cursor = 0;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.filtered.len().saturating_sub(1);
                DialogStep::Redraw
            }
            "dialog.select.submit" | "dialog.prompt.submit" => match self.selected() {
                Some(item) => DialogStep::Resolved(DialogOutcome::Selected {
                    dialog: self.id,
                    value: item.value.clone(),
                }),
                None => DialogStep::Ignored,
            },
            // `session_interrupt` is the action the table binds to escape, and every
            // dialog footer here advertises `esc cancel`. Without this arm the dialog
            // ignored it, `DialogHost` absorbed it as an unrecognised action, and a
            // picker could only be left by choosing something — a hint that lies, and
            // the worse kind, because it names a way out that does not exist.
            "app_exit" | "session_interrupt" => DialogStep::Resolved(DialogOutcome::Cancelled),
            "input_backspace" => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.set_filter(&filter);
                DialogStep::Redraw
            }
            _ => self.handle_typed(event),
        }
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        if let Some(character) = crate::views::permission::typed_character(key) {
            let filter = format!("{}{character}", self.filter);
            self.set_filter(&filter);
            return DialogStep::Redraw;
        }
        DialogStep::Ignored
    }
}

/// A session, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// The session id.
    pub id: String,
    /// Its title.
    pub title: String,
    /// A human-readable age or timestamp.
    pub when: String,
}

/// The session picker.
#[must_use]
pub fn session_picker(context: ViewContext, sessions: Vec<SessionEntry>) -> SelectDialog {
    let items = sessions
        .into_iter()
        .map(|session| {
            Item::new(session.title)
                .described(session.when)
                .valued(session.id)
        })
        .collect();
    SelectDialog::new(SESSION_DIALOG_ID, "Sessions", context, items)
}

/// A model, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// `provider/model`, the value the engine accepts.
    pub id: String,
    /// The model's display name.
    pub name: String,
    /// The provider's display name.
    pub provider: String,
}

/// The model picker.
///
/// The value is `provider/model` rather than a bare model id, because a bare id is
/// exactly the unqualified form the model policy treats as unavailable
/// (`zuno-agent/src/model_policy.rs`).
#[must_use]
pub fn model_picker(context: ViewContext, models: Vec<ModelEntry>) -> SelectDialog {
    let items = models
        .into_iter()
        .map(|model| {
            Item::new(model.name)
                .described(model.provider)
                .valued(model.id)
        })
        .collect();
    SelectDialog::new(MODEL_DIALOG_ID, "Models", context, items)
}

/// An agent, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    /// The agent's name.
    pub name: String,
    /// Its one-line description.
    pub description: String,
}

/// The agent picker.
#[must_use]
pub fn agent_picker(context: ViewContext, agents: Vec<AgentEntry>) -> SelectDialog {
    let items = agents
        .into_iter()
        .map(|agent| Item::new(agent.name).described(agent.description))
        .collect();
    SelectDialog::new(AGENT_DIALOG_ID, "Agents", context, items)
}

/// The theme picker, previewing each theme's resolved palette.
///
/// `mode` is the light/dark mode the preview resolves in, so a user picking in dark
/// mode sees the dark variant of a theme that declares both.
#[must_use]
pub fn theme_picker(context: ViewContext, registry: &ThemeRegistry, mode: Mode) -> SelectDialog {
    let names = registry.names();
    let items = names
        .iter()
        .map(|name| {
            let layer = registry
                .layer_of(name)
                .map_or_else(String::new, |layer| format!("{layer:?}").to_lowercase());
            Item::new(name.clone()).described(layer)
        })
        .collect::<Vec<_>>();
    // Every theme is resolved once here rather than on each frame: resolution walks
    // colour references and a picker redraws on every keystroke.
    let resolved = names
        .iter()
        .map(|name| (name.clone(), registry.resolve(name, mode)))
        .collect::<Vec<(String, Resolved)>>();
    let selected = context
        .config
        .keybinds
        .is_empty()
        .then(|| crate::theme::DEFAULT_THEME.to_owned());
    let dialog = SelectDialog::new(THEME_DIALOG_ID, "Themes", context, items)
        .with_rows(8)
        .with_preview(move |item, context| {
            let Some((_, resolved)) = resolved.iter().find(|(name, _)| *name == item.value) else {
                return Vec::new();
            };
            preview_lines(resolved, context)
        });
    match selected {
        Some(name) => dialog.selecting(&name),
        None => dialog,
    }
}

/// Six swatch rows summarising a palette, the theme picker's preview.
///
/// A subset of [`crate::theme::PaletteSampleView`]'s fifty-odd rows: a picker has
/// eight rows to spare, and these six are the ones a user judges a theme by.
#[must_use]
pub fn preview_lines(resolved: &Resolved, context: &ViewContext) -> Vec<Line<'static>> {
    let palette = &resolved.palette;
    let swatch = |label: &str, color: crate::theme::Rgba| {
        Span::styled(
            format!(" {label} "),
            ratatui::style::Style::new()
                .fg(crate::theme::selected_foreground(palette, Some(color)).into())
                .bg(color.into()),
        )
    };
    vec![
        Line::from(vec![Span::styled(
            format!(" {} ({:?})", resolved.name, resolved.mode),
            context.title(),
        )]),
        Line::from(vec![
            swatch("primary", palette.primary),
            swatch("accent", palette.accent),
            swatch("error", palette.error),
            swatch("warning", palette.warning),
            swatch("success", palette.success),
            swatch("info", palette.info),
        ]),
    ]
}
