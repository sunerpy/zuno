//! Background terminal list and bounded output viewer.

use crate::keybind::Definition;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::{ViewContext, truncate};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use std::sync::Arc;
use zuno_pty::{
    BackgroundExecutionId, BackgroundExecutionInfo, BackgroundExecutionService,
    BackgroundExecutionStatus, ReplayCursor,
};

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;

pub const DIALOG_ID: &str = "background_list";
pub const EMPTY: &str = "no background terminals for this session";

pub struct BackgroundView {
    context: ViewContext,
    service: Arc<BackgroundExecutionService>,
    session_id: String,
    executions: Vec<BackgroundExecutionInfo>,
    cursor: usize,
    expanded: bool,
    confirm_cancel: Option<BackgroundExecutionId>,
}

impl BackgroundView {
    #[must_use]
    pub fn new(
        context: ViewContext,
        service: Arc<BackgroundExecutionService>,
        session_id: impl Into<String>,
    ) -> Self {
        let session_id = session_id.into();
        let executions = service.list_for_session(&session_id);
        Self {
            context,
            service,
            session_id,
            executions,
            cursor: 0,
            expanded: false,
            confirm_cancel: None,
        }
    }

    fn refresh(&mut self) {
        let selected = self
            .executions
            .get(self.cursor)
            .map(|execution| execution.id.clone());
        self.executions = self.service.list_for_session(&self.session_id);
        self.cursor = selected
            .and_then(|id| {
                self.executions
                    .iter()
                    .position(|execution| execution.id == id)
            })
            .unwrap_or_else(|| self.cursor.min(self.executions.len().saturating_sub(1)));
    }

    fn selected(&self) -> Option<&BackgroundExecutionInfo> {
        self.executions.get(self.cursor)
    }

    fn step(&mut self, delta: isize) -> DialogStep {
        self.refresh();
        self.confirm_cancel = None;
        if self.executions.len() > 1 {
            let len = isize::try_from(self.executions.len()).unwrap_or(isize::MAX);
            let current = isize::try_from(self.cursor).unwrap_or_default();
            self.cursor =
                usize::try_from(current.saturating_add(delta).rem_euclid(len)).unwrap_or_default();
        }
        DialogStep::Redraw
    }

    fn cancel_step(&mut self) -> DialogStep {
        self.refresh();
        let Some(execution) = self
            .selected()
            .filter(|execution| execution.status == BackgroundExecutionStatus::Running)
        else {
            self.confirm_cancel = None;
            return DialogStep::Redraw;
        };
        let id = execution.id.clone();
        if self.confirm_cancel.as_ref() != Some(&id) {
            self.confirm_cancel = Some(id);
            self.expanded = true;
            return DialogStep::Redraw;
        }
        self.confirm_cancel = None;
        DialogStep::Emitted(DialogOutcome::BackgroundCancel {
            execution_id: id.to_string(),
        })
    }

    fn status_style(&self, status: BackgroundExecutionStatus) -> ratatui::style::Style {
        match status {
            BackgroundExecutionStatus::Running => self.context.accent(),
            BackgroundExecutionStatus::Completed => self.context.text(),
            BackgroundExecutionStatus::Failed => self.context.error(),
            BackgroundExecutionStatus::Cancelled => self.context.muted(),
            BackgroundExecutionStatus::Uncertain => self.context.warning(),
        }
    }

    fn detail(&self, execution: &BackgroundExecutionInfo, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        detail_row(
            &mut lines,
            &self.context,
            width,
            "id",
            execution.id.to_string(),
        );
        detail_row(
            &mut lines,
            &self.context,
            width,
            "status",
            execution.status.as_str().to_owned(),
        );
        detail_row(
            &mut lines,
            &self.context,
            width,
            "command",
            execution.command.clone(),
        );
        detail_row(
            &mut lines,
            &self.context,
            width,
            "cwd",
            execution.cwd.display().to_string(),
        );
        if let Some(pid) = execution.pid {
            detail_row(&mut lines, &self.context, width, "pid", pid.to_string());
        }
        if let Some(code) = execution.exit_code {
            detail_row(&mut lines, &self.context, width, "exit", code.to_string());
        }
        if let Some(error) = &execution.error {
            detail_row(&mut lines, &self.context, width, "error", error.clone());
        }
        if let Ok(replay) = self.service.output(&execution.id, ReplayCursor::Full) {
            let text = crate::attention::strip_ansi(&String::from_utf8_lossy(&replay.bytes));
            let tail = text.lines().rev().take(12).collect::<Vec<_>>();
            if !tail.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  output".to_owned(),
                    self.context.title(),
                )));
                for line in tail.into_iter().rev() {
                    lines.push(Line::from(Span::styled(
                        truncate(&format!("  {line}"), width),
                        self.context.text(),
                    )));
                }
            }
            if replay.discarded > 0 {
                detail_row(
                    &mut lines,
                    &self.context,
                    width,
                    "retention",
                    format!("{} bytes discarded before this tail", replay.discarded),
                );
            }
        }
        lines
    }
}

fn detail_row(
    lines: &mut Vec<Line<'static>>,
    context: &ViewContext,
    width: usize,
    label: &str,
    value: String,
) {
    lines.push(Line::from(Span::styled(
        truncate(&format!("  {label} {value}"), width),
        context.muted(),
    )));
}

impl Dialog for BackgroundView {
    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn title(&self) -> String {
        if self.executions.is_empty() {
            "Background terminals".to_owned()
        } else {
            format!(
                "Background terminals  {}/{}",
                self.cursor.saturating_add(1),
                self.executions.len()
            )
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.refresh();
        let body = usize::from(width.saturating_sub(2)).max(1);
        if self.executions.is_empty() {
            return vec![Line::from(Span::styled(
                EMPTY.to_owned(),
                self.context.muted(),
            ))];
        }
        let mut lines = self
            .executions
            .iter()
            .enumerate()
            .map(|(index, execution)| {
                let marker = if index == self.cursor { "›" } else { " " };
                let pid = execution
                    .pid
                    .map_or_else(|| "—".to_owned(), |pid| pid.to_string());
                let text = truncate(
                    &format!(
                        "{marker} {} · pid {pid} · {}",
                        execution.status.as_str(),
                        execution.title
                    ),
                    body,
                );
                Line::from(Span::styled(
                    text,
                    if index == self.cursor {
                        self.context.element()
                    } else {
                        self.status_style(execution.status)
                    },
                ))
            })
            .collect::<Vec<_>>();
        if self.expanded
            && let Some(execution) = self.selected()
        {
            lines.push(Line::default());
            lines.extend(self.detail(execution, body));
        }
        if let Some(id) = &self.confirm_cancel {
            lines.push(Line::from(Span::styled(
                truncate(
                    &format!("press x again to stop {id}; the command will never be replayed"),
                    body,
                ),
                self.context.warning(),
            )));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("←→", "terminal"),
            ("enter", "output"),
            ("x x", "stop"),
            ("esc", "close"),
        ]
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::XLarge
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["dialog.background", "session"]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        match action.name {
            "session_child_cycle" | "dialog.select.next" => self.step(1),
            "session_child_cycle_reverse" | "dialog.select.prev" => self.step(-1),
            "dialog.select.home" => {
                self.cursor = 0;
                self.confirm_cancel = None;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.refresh();
                self.cursor = self.executions.len().saturating_sub(1);
                self.confirm_cancel = None;
                DialogStep::Redraw
            }
            "dialog.select.submit" => {
                self.expanded = !self.expanded;
                self.confirm_cancel = None;
                DialogStep::Redraw
            }
            "background_cancel" => self.cancel_step(),
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
            MouseEventKind::ScrollUp => return self.step(-1),
            MouseEventKind::ScrollDown => return self.step(1),
            MouseEventKind::Up(MouseButton::Left) => {}
            _ => return DialogStep::Ignored,
        }
        self.refresh();
        let index = usize::from(event.row.saturating_sub(body.top()));
        if index >= self.executions.len() {
            return DialogStep::Ignored;
        }
        self.cursor = index;
        self.expanded = true;
        self.confirm_cancel = None;
        DialogStep::Redraw
    }
}
