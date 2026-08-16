//! The permission prompt: `once`, `always`, or `reject`.
//!
//! # The three replies are a contract, not a UI choice
//!
//! [`zuno_permission::ReplyKind`] is `Reject | Once | Always`, and the engine's
//! pending-approval lifecycle accepts exactly those. So this view does not invent a
//! vocabulary: it produces a [`PermissionDecision`] carrying that enum, and a
//! consumer hands it to [`zuno_permission::PermissionEngine`] unchanged. The labels
//! are the oracle's — "Allow once", "Allow always", "Reject"
//! (`packages/tui/src/routes/session/permission.tsx:432`).
//!
//! # Two escalation stages, both for the same reason
//!
//! `always` and `reject` are the two replies a user can regret, so each gets a
//! second surface (`permission.tsx:127-186`):
//!
//! - **always** shows what it is about to grant — either "until OpenCode is
//!   restarted" for the `*` pattern, or the concrete pattern list — and asks for a
//!   confirmation. The pattern list comes from the request's `always` field, which
//!   is what the engine will install as rules.
//! - **reject** offers a message box, but only in a child session, where the
//!   rejection is being reported back to a parent agent that can act on it. In a
//!   top-level session upstream replies immediately, because there is nobody to
//!   tell.
//!
//! Escape is `reject`, never `once`: the failure mode of a mis-keyed prompt has to
//! be refusal.
//!
//! # Per-permission titles
//!
//! `permission.tsx:189-357` reads the tool's own arguments to say what is about to
//! happen — the path being edited, the command being run, the URL being fetched —
//! rather than showing the permission's internal name. That is ported, driven by
//! the request's `metadata` and the tool input, because "Permission required:
//! bash" tells a user nothing they can decide on.

use crate::keybind::Definition;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep};
use crate::views::diff::DiffView;
use crate::views::{ViewContext, padded};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use serde_json::Value;
use zuno_permission::{PermissionRequest, ReplyKind};

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;

/// The dialog id [`DialogOutcome`] carries for this prompt.
pub const DIALOG_ID: &str = "permission";

/// What the user decided about one permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    /// The request being answered.
    pub request_id: String,
    /// The reply, in the engine's own vocabulary.
    pub reply: ReplyKind,
    /// The rejection message, when the user wrote one.
    pub message: Option<String>,
}

impl PermissionDecision {
    /// The reply the engine's [`zuno_permission::PermissionReply`] needs.
    #[must_use]
    pub fn into_reply(self) -> zuno_permission::PermissionReply {
        zuno_permission::PermissionReply {
            request_id: self.request_id,
            reply: self.reply,
            message: self.message,
        }
    }
}

/// Which surface the prompt is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The three-option choice.
    Choose,
    /// The "always allow" confirmation.
    ConfirmAlways,
    /// The rejection message box.
    RejectMessage,
}

/// One offered reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Option_ {
    reply: ReplyKind,
    label: &'static str,
}

/// The oracle's option order and labels (`permission.tsx:432`).
const OPTIONS: [Option_; 3] = [
    Option_ {
        reply: ReplyKind::Once,
        label: "Allow once",
    },
    Option_ {
        reply: ReplyKind::Always,
        label: "Allow always",
    },
    Option_ {
        reply: ReplyKind::Reject,
        label: "Reject",
    },
];

/// What the request is asking for, in words a user can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// The one-glyph marker.
    pub icon: &'static str,
    /// The headline.
    pub title: String,
    /// Detail rows.
    pub detail: Vec<String>,
}

/// Describe a request the way the oracle's `info()` does (`permission.tsx:189-357`).
///
/// `input` is the tool call's decoded arguments when the transcript has them; the
/// fallback path is deliberate rather than an error, because a permission ask can
/// arrive before the arguments finish streaming.
#[must_use]
pub fn describe(request: &PermissionRequest, input: &Value) -> Subject {
    let string =
        |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or_default().to_owned();
    let arg = |key: &str| string(input.get(key));
    let meta = |key: &str| string(request.metadata.get(key));

    match request.permission.as_str() {
        "edit" => {
            let path = edit_path(request, input);
            Subject {
                icon: "→",
                title: format!("Edit {path}"),
                detail: detail("Path", &path),
            }
        }
        "read" => {
            let path = arg("filePath");
            Subject {
                icon: "→",
                title: format!("Read {path}"),
                detail: detail("Path", &path),
            }
        }
        "glob" => {
            let pattern = arg("pattern");
            Subject {
                icon: "✱",
                title: format!("Glob \"{pattern}\""),
                detail: detail("Pattern", &pattern),
            }
        }
        "grep" => {
            let pattern = arg("pattern");
            Subject {
                icon: "✱",
                title: format!("Grep \"{pattern}\""),
                detail: detail("Pattern", &pattern),
            }
        }
        "list" => {
            let path = arg("path");
            Subject {
                icon: "→",
                title: format!("List {path}"),
                detail: detail("Path", &path),
            }
        }
        "bash" => {
            let command = arg("command");
            Subject {
                icon: "#",
                title: String::from("Shell command"),
                detail: if command.is_empty() {
                    Vec::new()
                } else {
                    vec![format!("$ {command}")]
                },
            }
        }
        "task" => {
            let kind = input
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .to_owned();
            let description = arg("description");
            Subject {
                icon: "#",
                title: format!("{} Task", titlecase(&kind)),
                detail: if description.is_empty() {
                    Vec::new()
                } else {
                    vec![format!("◉ {description}")]
                },
            }
        }
        "webfetch" => {
            let url = arg("url");
            Subject {
                icon: "%",
                title: format!("WebFetch {url}"),
                detail: detail("URL", &url),
            }
        }
        "websearch" => {
            let query = arg("query");
            Subject {
                icon: "◈",
                title: format!("Web search \"{query}\""),
                detail: detail("Query", &query),
            }
        }
        "external_directory" => {
            // `permission.tsx:322-338`: the directory is whichever of these the
            // request actually carried, and a wildcard pattern is reduced to its
            // parent because a user judges a directory, not a glob.
            let parent = meta("parentDir");
            let filepath = meta("filepath");
            let derived = request
                .patterns
                .first()
                .map_or_else(String::new, |pattern| {
                    if pattern.contains('*') {
                        parent_of(pattern)
                    } else {
                        pattern.clone()
                    }
                });
            let directory = [parent, filepath, derived]
                .into_iter()
                .find(|candidate| !candidate.is_empty())
                .unwrap_or_default();
            Subject {
                icon: "←",
                title: format!("Access external directory {directory}"),
                detail: request
                    .patterns
                    .iter()
                    .map(|pattern| format!("- {pattern}"))
                    .collect(),
            }
        }
        "doom_loop" => Subject {
            icon: "⟳",
            title: String::from("Continue after repeated failures"),
            detail: vec![String::from(
                "This keeps the session running despite repeated failures.",
            )],
        },
        other => Subject {
            icon: "⚙",
            title: format!("Call tool {other}"),
            detail: vec![format!("Tool: {other}")],
        },
    }
}

fn edit_path(request: &PermissionRequest, input: &Value) -> String {
    ["filePath", "file_path", "path"]
        .into_iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .or_else(|| request.metadata.get("filepath").and_then(Value::as_str))
        .or_else(|| request.patterns.first().map(String::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn edit_patch(request: &PermissionRequest, input: &Value) -> Option<String> {
    request
        .metadata
        .get("diff")
        .and_then(Value::as_str)
        .or_else(|| {
            ["patchText", "patch_text", "patch"]
                .into_iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
        })
        .filter(|patch| !patch.is_empty())
        .map(str::to_owned)
        .or_else(|| replacement_patch(input))
}

fn replacement_patch(input: &Value) -> Option<String> {
    let old = input
        .get("oldString")
        .or_else(|| input.get("old_string"))
        .and_then(Value::as_str)?;
    let new = input
        .get("newString")
        .or_else(|| input.get("new_string"))
        .and_then(Value::as_str)?;
    if old == new {
        return None;
    }

    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let old_count = old_lines.len().max(1);
    let new_count = new_lines.len().max(1);
    let mut patch = format!("@@ -1,{old_count} +1,{new_count} @@\n");
    if old_lines.is_empty() {
        patch.push_str("-\n");
    } else {
        for line in old_lines {
            patch.push('-');
            patch.push_str(line);
            patch.push('\n');
        }
    }
    if new_lines.is_empty() {
        patch.push_str("+\n");
    } else {
        for line in new_lines {
            patch.push('+');
            patch.push_str(line);
            patch.push('\n');
        }
    }
    Some(patch)
}

fn detail(label: &str, value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        vec![format!("{label}: {value}")]
    }
}

fn parent_of(pattern: &str) -> String {
    match pattern.rfind('/') {
        Some(0) => String::from("/"),
        Some(index) => pattern[..index].to_owned(),
        None => String::from("."),
    }
}

/// Upstream's `Locale.titlecase` for a single word.
fn titlecase(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// The permission prompt.
pub struct PermissionPrompt {
    context: ViewContext,
    request: PermissionRequest,
    subject: Subject,
    stage: Stage,
    selected: usize,
    /// Confirmation cursor in [`Stage::ConfirmAlways`]: `true` is "Confirm".
    confirm: bool,
    reject_message: String,
    /// Whether rejecting offers a message box (`permission.tsx:479` — a child
    /// session only).
    reject_message_offered: bool,
    /// Whether the body is expanded to the full frame
    /// (`permission.prompt.fullscreen`).
    expanded: bool,
    diff: Option<DiffView>,
}

impl PermissionPrompt {
    /// A prompt for `request`, with the tool call's arguments when they are known.
    #[must_use]
    pub fn new(context: ViewContext, request: PermissionRequest, input: &Value) -> Self {
        let subject = describe(&request, input);
        let diff = (request.permission == "edit")
            .then(|| edit_patch(&request, input))
            .flatten()
            .map(|patch| DiffView::new(context.clone(), &patch));
        Self {
            context,
            request,
            subject,
            stage: Stage::Choose,
            selected: 0,
            confirm: true,
            reject_message: String::new(),
            reject_message_offered: false,
            expanded: false,
            diff,
        }
    }

    /// Offer a message box when rejecting, for a child session.
    #[must_use]
    pub const fn with_reject_message(mut self, offered: bool) -> Self {
        self.reject_message_offered = offered;
        self
    }

    /// The stage being shown.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        self.stage
    }

    /// The currently highlighted reply.
    #[must_use]
    pub fn highlighted(&self) -> ReplyKind {
        OPTIONS[self.selected].reply
    }

    /// Whether the body is expanded.
    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// The request being decided.
    #[must_use]
    pub const fn request(&self) -> &PermissionRequest {
        &self.request
    }

    fn decide(&self, reply: ReplyKind, message: Option<String>) -> DialogStep {
        DialogStep::Resolved(DialogOutcome::Permission(PermissionDecision {
            request_id: self.request.id.clone(),
            reply,
            message,
        }))
    }

    fn choose_lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(vec![
                Span::styled(String::from("△ "), self.context.warning()),
                Span::styled(String::from("Permission required"), self.context.title()),
            ]),
            Line::from(vec![
                Span::styled(format!("  {} ", self.subject.icon), self.context.muted()),
                Span::styled(self.subject.title.clone(), self.context.text()),
            ]),
        ];
        for row in &self.subject.detail {
            lines.push(padded(&format!("  {row}"), width, self.context.muted()));
        }
        if let Some(diff) = self.diff.as_mut() {
            lines.push(padded("", width, self.context.surface()));
            lines.extend(diff.lines(width));
        }
        lines.push(padded("", width, self.context.surface()));
        let mut spans = Vec::new();
        for (index, option) in OPTIONS.iter().enumerate() {
            let style = if index == self.selected {
                self.context.selected()
            } else {
                self.context.muted()
            };
            spans.push(Span::styled(format!(" {} ", option.label), style));
            spans.push(Span::styled(String::from(" "), self.context.surface()));
        }
        lines.push(Line::from(spans));
        lines
    }

    fn always_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled(String::from("△ "), self.context.warning()),
            Span::styled(String::from("Always allow"), self.context.title()),
        ])];
        // `permission.tsx:129-131`: a lone `*` is a blanket grant, and saying so is
        // clearer than printing an asterisk and hoping.
        if self.request.always.len() == 1 && self.request.always[0] == "*" {
            lines.push(padded(
                &format!(
                    "  This will allow {} until OpenCode is restarted.",
                    self.request.permission
                ),
                width,
                self.context.muted(),
            ));
        } else {
            lines.push(padded(
                "  This will allow the following patterns until OpenCode is restarted",
                width,
                self.context.muted(),
            ));
            for pattern in &self.request.always {
                lines.push(padded(
                    &format!("  - {pattern}"),
                    width,
                    self.context.text(),
                ));
            }
        }
        lines.push(padded("", width, self.context.surface()));
        let confirm_style = if self.confirm {
            self.context.selected()
        } else {
            self.context.muted()
        };
        let cancel_style = if self.confirm {
            self.context.muted()
        } else {
            self.context.selected()
        };
        lines.push(Line::from(vec![
            Span::styled(String::from(" Confirm "), confirm_style),
            Span::styled(String::from(" "), self.context.surface()),
            Span::styled(String::from(" Cancel "), cancel_style),
        ]));
        lines
    }

    fn reject_lines(&self, width: u16) -> Vec<Line<'static>> {
        vec![
            Line::from(vec![
                Span::styled(String::from("△ "), self.context.error()),
                Span::styled(String::from("Reject permission"), self.context.title()),
            ]),
            padded(
                "  Tell OpenCode what to do differently",
                width,
                self.context.muted(),
            ),
            padded("", width, self.context.surface()),
            padded(
                &format!("  {}▏", self.reject_message),
                width,
                self.context.element(),
            ),
        ]
    }
}

impl Dialog for PermissionPrompt {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        match self.stage {
            Stage::Choose => String::from("Permission required"),
            Stage::ConfirmAlways => String::from("Always allow"),
            Stage::RejectMessage => String::from("Reject permission"),
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        match self.stage {
            Stage::Choose => self.choose_lines(width),
            Stage::ConfirmAlways => self.always_lines(width),
            Stage::RejectMessage => self.reject_lines(width),
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match self.stage {
            Stage::Choose => vec![
                ("↑↓", "select"),
                ("enter", "confirm"),
                (
                    "ctrl+f",
                    if self.expanded {
                        "minimize"
                    } else {
                        "fullscreen"
                    },
                ),
            ],
            Stage::ConfirmAlways => vec![("⇆", "select"), ("enter", "confirm"), ("esc", "cancel")],
            Stage::RejectMessage => vec![("enter", "confirm"), ("esc", "cancel")],
        }
    }

    fn desired_height(&self, content_rows: u16, available: u16) -> u16 {
        if self.expanded {
            return available;
        }
        // `permission.tsx:626` caps a collapsed prompt at 15 rows so the transcript
        // behind it stays readable — which is what makes the prompt decidable.
        content_rows.saturating_add(2).min(available).min(15)
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match self.stage {
            Stage::Choose => match action.name {
                "dialog.select.prev" => {
                    self.selected = (self.selected + OPTIONS.len() - 1) % OPTIONS.len();
                    DialogStep::Redraw
                }
                "dialog.select.next" => {
                    self.selected = (self.selected + 1) % OPTIONS.len();
                    DialogStep::Redraw
                }
                "dialog.select.home" => {
                    self.selected = 0;
                    DialogStep::Redraw
                }
                "dialog.select.end" => {
                    self.selected = OPTIONS.len() - 1;
                    DialogStep::Redraw
                }
                "permission.prompt.fullscreen" => {
                    self.expanded = !self.expanded;
                    DialogStep::Redraw
                }
                "dialog.select.submit" | "dialog.prompt.submit" => {
                    match OPTIONS[self.selected].reply {
                        ReplyKind::Always => {
                            self.stage = Stage::ConfirmAlways;
                            self.confirm = true;
                            DialogStep::Redraw
                        }
                        ReplyKind::Reject if self.reject_message_offered => {
                            self.stage = Stage::RejectMessage;
                            DialogStep::Redraw
                        }
                        reply => self.decide(reply, None),
                    }
                }
                // Escape resolves to `reject`, never to the highlighted option: a
                // prompt dismissed by accident must not have granted anything.
                "app_exit" | "session_interrupt" => self.decide(ReplyKind::Reject, None),
                _ => DialogStep::Ignored,
            },
            Stage::ConfirmAlways => match action.name {
                "dialog.select.prev" | "dialog.select.next" => {
                    self.confirm = !self.confirm;
                    DialogStep::Redraw
                }
                "dialog.select.submit" | "dialog.prompt.submit" => {
                    if self.confirm {
                        self.decide(ReplyKind::Always, None)
                    } else {
                        self.stage = Stage::Choose;
                        DialogStep::Redraw
                    }
                }
                "app_exit" | "session_interrupt" => {
                    // Cancelling the escalation returns to the choice rather than
                    // resolving: the user has not decided yet.
                    self.stage = Stage::Choose;
                    DialogStep::Redraw
                }
                _ => DialogStep::Ignored,
            },
            Stage::RejectMessage => match action.name {
                "dialog.prompt.submit" | "dialog.select.submit" => {
                    let message = self.reject_message.trim();
                    let message = if message.is_empty() {
                        None
                    } else {
                        Some(message.to_owned())
                    };
                    self.decide(ReplyKind::Reject, message)
                }
                "app_exit" | "session_interrupt" => {
                    self.stage = Stage::Choose;
                    self.reject_message.clear();
                    DialogStep::Redraw
                }
                "input_backspace" => {
                    self.reject_message.pop();
                    DialogStep::Redraw
                }
                _ => {
                    if let KeyCode::Char(character) = event.code
                        && !event
                            .modifiers
                            .intersects(crossterm::event::KeyModifiers::CONTROL)
                    {
                        self.reject_message.push(character);
                        return DialogStep::Redraw;
                    }
                    DialogStep::Ignored
                }
            },
        }
    }
}

/// Typed text a prompt in [`Stage::RejectMessage`] should receive.
///
/// A dialog only ever sees *resolved actions*, and a printable character resolves
/// to no action — so without this the reject box could not be typed into. The
/// dispatcher forwards unmatched keys to the component tree, and this is the seam
/// the host uses to route them into the active dialog.
pub fn typed_character(event: &KeyEvent) -> Option<char> {
    match event.code {
        KeyCode::Char(character)
            if !event
                .modifiers
                .intersects(crossterm::event::KeyModifiers::CONTROL) =>
        {
            Some(character)
        }
        _ => None,
    }
}
