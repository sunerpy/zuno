//! The two troubleshooting panels: `§8.7`'s status census (D15) and debug report (D16).
//!
//! Colours come from the owning [`ViewContext`]'s palette; nothing here names a colour.
//!
//! # Every row states a fact the runtime actually has
//!
//! `§8.7` asks for generous fields and then constrains them: "每个字段都必须来自真实运行时
//! 状态，不接数据源的字段不显示" — every field must come from real runtime state, and a
//! field with no data source is not shown. A troubleshooting panel is the one surface
//! where an invented row is worse than a missing one, because its whole purpose is to be
//! believed when something is already wrong.
//!
//! Two of the plan's fields are therefore absent, each for a reason established by
//! reading the workspace rather than by preference:
//!
//! * **The enabled-formatter group.** `zuno_tools::format::Formatters` has no production
//!   construction site — every one is a test. Nothing formats anything in this build, so
//!   a group listing "enabled formatters" would assert a subsystem that never runs. The
//!   configured set is real configuration, but printing it under a *status* heading is
//!   the same defect as the `tool_affordance` arm that matched `"patch"` while the
//!   registry spelled it `apply_patch`: a claim with nothing behind it. The group returns
//!   when a formatter runtime is assembled.
//! * **A plugin's version.** [`zuno_plugin`]'s loaded manifest carries an id and a hook
//!   list and no version field, so plugins are listed by id with the hooks they claim —
//!   which is also the fact a person debugging a plugin wants. An npm spec may encode a
//!   version in its install string, but that is the requested spec, not the loaded
//!   plugin's identity, and the two diverge exactly when it matters.
//!
//! # Why both panels are built from `Service`
//!
//! [`crate::views::ambient::Service`] is already the host-neutral `{name, health,
//! detail}` triple the sidebar reads and the one [`crate::views::picker::McpServer`]
//! projects onto. Reusing it keeps one health vocabulary in the crate: a second would let
//! the sidebar and this panel disagree about whether a server is up, and a person
//! comparing them would have no way to tell which was right.
//!
//! # Neither panel selects anything
//!
//! D15 is read-only and closes on escape. D16's only action is to copy itself, which it
//! reports through [`DialogOutcome::Submitted`] so the screen's existing clipboard seam —
//! and therefore its existing toast — does the work. A panel that wrote to the
//! clipboard itself would need its own failure reporting, and the copy already has some.

use crate::keybind::Definition;
use crate::views::ambient::{Health, Service};
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::{ViewContext, display_width, padded, truncate};
use crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;

/// The dialog id [`DialogOutcome`] carries for the status census.
pub const STATUS_DIALOG_ID: &str = "status_view";

/// The dialog id [`DialogOutcome`] carries for the debug report.
pub const DEBUG_DIALOG_ID: &str = "debug_view";

/// Columns the name column occupies before the detail begins.
///
/// Wide enough for the built-in language-server ids and for a configured MCP name; a
/// longer name is truncated rather than allowed to run into its own detail, which is the
/// failure the help view measured on a real terminal and fixed the same way.
pub const NAME_COLUMN: usize = 24;

/// What a group with no members says.
///
/// A heading with nothing under it reads as a panel that failed to load. Saying `none`
/// distinguishes "this build has no MCP servers configured" from "the census broke".
pub const EMPTY: &str = "none";

/// One titled census group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The group heading.
    pub title: String,
    /// Its members, in the order the host resolved them.
    pub services: Vec<Service>,
}

impl Group {
    /// A group of `services` under `title`.
    #[must_use]
    pub fn new(title: impl Into<String>, services: Vec<Service>) -> Self {
        Self {
            title: title.into(),
            services,
        }
    }
}

/// One rendered row of either panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A group heading.
    Heading(String),
    /// A member of the preceding group.
    Entry(Service),
    /// A group heading with nothing under it.
    Empty,
}

/// The status census (D15): read-only groups of live runtime state.
pub struct StatusPanel {
    context: ViewContext,
    groups: Vec<Group>,
    /// First rendered row of the flattened list.
    offset: usize,
    rows: usize,
}

impl StatusPanel {
    /// A census over `groups`, which the host resolved from live state.
    #[must_use]
    pub fn new(context: ViewContext, groups: Vec<Group>) -> Self {
        Self {
            context,
            groups,
            offset: 0,
            rows: 16,
        }
    }

    /// Every row, flattened in group order with a heading before each group.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for group in &self.groups {
            rows.push(Row::Heading(group.title.clone()));
            if group.services.is_empty() {
                rows.push(Row::Empty);
                continue;
            }
            for service in &group.services {
                rows.push(Row::Entry(service.clone()));
            }
        }
        rows
    }
}

/// Render one `{name, health, detail}` row into `width` columns.
///
/// Shared by both panels so a debug field and a census member cannot drift into two
/// column layouts. The name is truncated at [`NAME_COLUMN`]; the detail takes what is
/// left and is truncated too, because a detail that overflows is redrawn by the terminal
/// on the next line without its gutter glyph and reads as a row of its own.
fn service_row(context: &ViewContext, service: &Service, width: u16) -> Line<'static> {
    let style = match service.health {
        Health::Ready => context.success(),
        Health::Pending => context.warning(),
        Health::Faulted => context.error(),
        Health::Disabled => context.muted(),
    };
    let total = usize::from(width);
    let gutter = format!(" {} ", service.health.glyph());
    let gutter_width = display_width(&gutter);
    if total <= gutter_width {
        // The glyph still carries the health, which is the one fact that survives at this
        // width. Dropping the row instead would remove the member from the census.
        return Line::from(Span::styled(truncate(&gutter, total), style));
    }

    // The name column yields to the frame rather than holding [`NAME_COLUMN`] into a
    // narrower one — padding to a fixed column inside a 20-column frame is how a row comes
    // to be 29 columns wide, which ratatui then clips without saying which half it took.
    // This is the same "decoration gives way to content" rule the markdown renderer's
    // fences and list markers follow.
    let body = total - gutter_width;
    let column = NAME_COLUMN.min(body);
    let name = if display_width(&service.name) > column {
        // The ellipsis only earns its column when at least one glyph precedes it.
        if column >= 2 {
            format!("{}…", truncate(&service.name, column - 1))
        } else {
            truncate(&service.name, column)
        }
    } else {
        service.name.clone()
    };
    // `{:<width$}` pads by `char` count, which is wrong for a CJK name: two columns of
    // glyph counted as one character leaves the detail column short by the difference.
    // Padding is therefore measured in terminal columns.
    let pad = column.saturating_sub(display_width(&name));

    let head = format!("{gutter}{name}{}", " ".repeat(pad));
    let used = display_width(&head);
    let mut spans = vec![Span::styled(head, style)];
    // Two columns separate the name from its detail. Below that there is no detail
    // column at all, and the remainder is padding rather than a one-character sliver of a
    // failure reason.
    let room = total.saturating_sub(used);
    if room > 2 && !service.detail.is_empty() {
        let available = room - 2;
        let detail = if display_width(&service.detail) > available {
            truncate(&service.detail, available)
        } else {
            service.detail.clone()
        };
        let filled = used + 2 + display_width(&detail);
        spans.push(Span::styled(format!("  {detail}"), context.muted()));
        if filled < total {
            spans.push(Span::styled(" ".repeat(total - filled), context.muted()));
        }
    } else if room > 0 {
        spans.push(Span::styled(" ".repeat(room), style));
    }
    Line::from(spans)
}

/// Window `rows` at `offset`, correcting an offset left past the end.
///
/// Both panels shrink when a filterless list changes under them — the MCP group is live —
/// so the clamp lives here rather than being repeated with two chances to differ.
fn window(rows: Vec<Row>, offset: &mut usize, height: usize) -> Vec<Row> {
    let total = rows.len();
    // Clamped to the last full screenful rather than to the last row. Stopping at
    // `total - 1` leaves a page-down at the end showing a single entry with its group
    // heading scrolled off, which is the one row that cannot be read without the heading
    // above it — a bare `rust` says nothing about whether it is a language server or a
    // plugin.
    let last = total.saturating_sub(height);
    if *offset > last {
        *offset = last;
    }
    rows.into_iter().skip(*offset).take(height).collect()
}

impl Dialog for StatusPanel {
    fn id(&self) -> &'static str {
        STATUS_DIALOG_ID
    }

    fn title(&self) -> String {
        String::from("Status")
    }

    /// `§11.4` names status at the widest tier, and each row carries a name plus a
    /// sentence — at 88 columns a failure reason is what gets cut, which is the half a
    /// person opening this panel came for.
    fn width(&self) -> DialogWidth {
        DialogWidth::XLarge
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        window(self.rows(), &mut self.offset, self.rows)
            .into_iter()
            .map(|row| match row {
                Row::Heading(title) => padded(&format!(" {title}"), width, self.context.title()),
                Row::Entry(service) => service_row(&self.context, &service, width),
                Row::Empty => padded(&format!("   {EMPTY}"), width, self.context.muted()),
            })
            .collect()
    }

    /// No `enter`: `§6.2` gives D15 no selection, so advertising one would invite a press
    /// that does nothing.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "scroll"), ("esc", "close")]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
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
            // `dialog.select.submit` closes rather than being ignored: a read-only panel
            // that swallowed `enter` would look wedged to anyone who pressed it first.
            "app_exit" | "session_interrupt" | "status_view" | "dialog.select.submit" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
        }
    }
}

/// The runtime facts D16 reports, each resolved by the host from a real source.
///
/// Every field is `Option` or already-formatted text: this crate reads no environment and
/// no build metadata itself, for the same reason [`crate::views::picker::McpServer`] is
/// plain data — rendering stays above execution, and a test can state a fact without a
/// process to read it from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugFacts {
    /// The build identity operators see.
    pub build: Option<String>,
    /// The Rust package version.
    pub version: Option<String>,
    /// The channel this binary was built for.
    pub channel: Option<String>,
    /// Operating system and architecture.
    pub os: Option<String>,
    /// The terminal program, when it identifies itself.
    pub terminal: Option<String>,
    /// The session this screen is talking in.
    pub session: Option<String>,
    /// The qualified `provider/model` in use.
    pub model: Option<String>,
    /// The current agent.
    pub agent: Option<String>,
    /// The working directory, already abbreviated by the host.
    pub directory: Option<String>,
}

impl DebugFacts {
    /// The labelled fields, omitting every one the host could not resolve.
    ///
    /// Order is fixed and runs from build identity outward to session state, because a
    /// person pasting this into a report is answering "which build, on what, doing what".
    #[must_use]
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        [
            ("build", self.build.as_ref()),
            ("version", self.version.as_ref()),
            ("channel", self.channel.as_ref()),
            ("os", self.os.as_ref()),
            ("terminal", self.terminal.as_ref()),
            ("directory", self.directory.as_ref()),
            ("session", self.session.as_ref()),
            ("agent", self.agent.as_ref()),
            ("model", self.model.as_ref()),
        ]
        .into_iter()
        .filter_map(|(label, value)| value.map(|value| (label, value.clone())))
        .collect()
    }

    /// The plain-text form `enter` puts on the clipboard.
    ///
    /// `label: value` per line rather than the rendered rows: the panel's rows are padded
    /// to a column width and carry a health glyph, and pasting that into an issue gives
    /// somebody a block of trailing spaces to clean up.
    #[must_use]
    pub fn report(&self) -> String {
        self.fields()
            .into_iter()
            .map(|(label, value)| format!("{label}: {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The debug report (D16): runtime environment, copied on `enter`.
pub struct DebugPanel {
    context: ViewContext,
    facts: DebugFacts,
    offset: usize,
    rows: usize,
}

impl DebugPanel {
    /// A report over `facts`.
    #[must_use]
    pub fn new(context: ViewContext, facts: DebugFacts) -> Self {
        Self {
            context,
            facts,
            offset: 0,
            rows: 16,
        }
    }

    /// The facts being reported.
    #[must_use]
    pub const fn facts(&self) -> &DebugFacts {
        &self.facts
    }

    /// Every row: one group heading and a field per resolved fact.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let fields = self.facts.fields();
        let mut rows = vec![Row::Heading(String::from("Runtime"))];
        if fields.is_empty() {
            rows.push(Row::Empty);
            return rows;
        }
        rows.extend(fields.into_iter().map(|(label, value)| {
            // `Health::Ready` for every field, because a fact the host resolved is a fact:
            // there is no third state between present and omitted here, and colouring
            // them differently would imply one.
            Row::Entry(Service::new(label, Health::Ready).detailed(value))
        }));
        rows
    }
}

impl Dialog for DebugPanel {
    fn id(&self) -> &'static str {
        DEBUG_DIALOG_ID
    }

    fn title(&self) -> String {
        String::from("Debug")
    }

    /// The widest tier for the same reason as the census: a path or a qualified model is
    /// the field most likely to be the one truncated.
    fn width(&self) -> DialogWidth {
        DialogWidth::XLarge
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        window(self.rows(), &mut self.offset, self.rows)
            .into_iter()
            .map(|row| match row {
                Row::Heading(title) => padded(&format!(" {title}"), width, self.context.title()),
                Row::Entry(service) => service_row(&self.context, &service, width),
                Row::Empty => padded(&format!("   {EMPTY}"), width, self.context.muted()),
            })
            .collect()
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "scroll"), ("enter", "copy"), ("esc", "close")]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
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
            // `Emitted`, not `Resolved`: copying is not finishing. The panel stays up so a
            // person can read the fields they just copied, and a second press is a second
            // copy rather than a press at a dialog that vanished.
            "dialog.select.submit" => DialogStep::Emitted(DialogOutcome::Submitted {
                dialog: DEBUG_DIALOG_ID,
                text: self.facts.report(),
            }),
            "app_exit" | "session_interrupt" | "debug_view" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
        }
    }
}
