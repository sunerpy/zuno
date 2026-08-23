//! Durable native and product-subagent activity projected from the transcript.
//!
//! Tool names are configuration, not presentation contracts. A call enters this view
//! only when its persisted [`zuno_tool::ToolUiIntent`] is `Subagent`; the stable task
//! and product envelopes then provide subject details. Later `job` output and durable
//! next-step reports refine the same row to completed, failed, cancelled, or uncertain.

use crate::keybind::Definition;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep, DialogWidth};
use crate::views::message::{Message, MessagePart, ToolStatus};
use crate::views::{ViewContext, truncate};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_tool::ToolUiIntent;

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

/// The dialog id [`DialogOutcome`] carries for this view.
pub const SUBAGENT_DIALOG_ID: &str = "session_child_first";

/// What an empty view says instead of looking broken.
pub const EMPTY: &str = "no native or product subagent jobs yet";

/// Where a native child's internal transcript remains available.
pub const CHILD_TRANSCRIPT_NOTE: &str = "the subagent's own messages are in that session";

/// Stable facts parsed from a native `<task …>` result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskEnvelope {
    pub session_id: Option<String>,
    pub job_id: Option<String>,
    pub state: Option<String>,
    pub report_delivery: Option<String>,
    pub result: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProductEnvelope {
    product: Option<String>,
    instance: Option<String>,
    run_id: Option<String>,
    job_id: Option<String>,
    state: Option<String>,
    report_delivery: Option<String>,
    result: String,
}

/// Compact envelope projection used by the inline transcript renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputEnvelope {
    pub detail: String,
    pub result: String,
}

/// One native or external delegation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    pub call_id: String,
    pub tool: String,
    pub product: String,
    pub target: Option<String>,
    pub objective: Option<String>,
    pub dispatch_status: ToolStatus,
    pub state: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub job_id: Option<String>,
    pub report_delivery: Option<String>,
    pub result: Option<String>,
    pub diagnostic: Option<String>,
    pub time_created: Option<i64>,
    pub time_completed: Option<i64>,
}

impl Delegation {
    #[must_use]
    pub fn headline(&self, width: usize) -> String {
        let target = self.target.as_deref().unwrap_or("subagent");
        let objective = self.objective.as_deref().unwrap_or("(no description)");
        truncate(
            &format!(
                "{} {} {target}: {objective}",
                state_glyph(&self.state),
                self.product
            ),
            width,
        )
    }

    fn cancellable(&self) -> bool {
        self.job_id.is_some() && self.state == "running"
    }

    fn elapsed(&self) -> String {
        let Some(started) = self.time_created else {
            return "not reported".to_owned();
        };
        let ended = self.time_completed.unwrap_or_else(now_millis);
        format_duration(ended.saturating_sub(started))
    }

    fn safety(&self) -> &'static str {
        if self.product == "zuno" {
            "Zuno child session; durable transcript and normal Zuno permissions"
        } else {
            "native login/config/model; credentials stay outside Zuno; uncertain calls are not replayed"
        }
    }
}

/// Project every persisted subagent-intent call, then refine it with job observations.
#[must_use]
pub fn delegations(messages: &[Message]) -> Vec<Delegation> {
    let mut found = Vec::new();
    for message in messages {
        for part in &message.parts {
            let MessagePart::Tool {
                call_id,
                name,
                ui_intent,
                arguments,
                status,
                output,
                ..
            } = part
            else {
                continue;
            };
            if *ui_intent != ToolUiIntent::Subagent {
                continue;
            }
            found.push(project_call(
                call_id,
                name,
                arguments,
                *status,
                output.as_deref(),
            ));
        }
    }

    for message in messages {
        for part in &message.parts {
            match part {
                MessagePart::Tool {
                    ui_intent: ToolUiIntent::Generic,
                    output: Some(output),
                    ..
                } => refine_from_job_output(&mut found, output),
                MessagePart::Text { text } => refine_from_report(&mut found, text),
                _ => {}
            }
        }
    }
    found
}

/// Merge the durable job projection into transcript-derived delegations.
///
/// A background job changes after its original tool result was persisted. Reading the
/// current SQLite-backed projection here keeps `/subagents` authoritative without
/// requiring the user to call `job` merely to refresh the dialog.
#[must_use]
pub fn delegations_with_jobs(
    messages: &[Message],
    jobs: &[zuno_types::JobProjection],
) -> Vec<Delegation> {
    let mut found = delegations(messages);
    merge_job_projections(&mut found, jobs);
    found
}

fn project_call(
    call_id: &str,
    name: &str,
    arguments: &str,
    status: ToolStatus,
    output: Option<&str>,
) -> Delegation {
    let arguments = serde_json::from_str::<Value>(arguments).ok();
    let field = |key: &str| {
        arguments
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.is_empty())
    };
    let objective = field("description").or_else(|| field("prompt"));
    let requested_delivery = field("reportDelivery").or_else(|| {
        arguments
            .as_ref()
            .and_then(|value| value.get("background"))
            .and_then(Value::as_bool)
            .filter(|background| *background)
            .map(|_| "nextStep".to_owned())
    });

    if let Some(task) = output.and_then(task_envelope) {
        return Delegation {
            call_id: call_id.to_owned(),
            tool: name.to_owned(),
            product: "zuno".to_owned(),
            target: field("subagent_type").or_else(|| field("category")),
            objective,
            dispatch_status: status,
            state: task
                .state
                .unwrap_or_else(|| dispatch_state(status).to_owned()),
            session_id: task.session_id,
            run_id: None,
            job_id: task.job_id,
            report_delivery: task.report_delivery.or(requested_delivery),
            result: nonempty(task.result),
            diagnostic: (status == ToolStatus::Error)
                .then(|| output.unwrap_or_default().to_owned()),
            time_created: None,
            time_completed: None,
        };
    }
    if let Some(product) = output.and_then(product_envelope) {
        return Delegation {
            call_id: call_id.to_owned(),
            tool: name.to_owned(),
            product: product.product.unwrap_or_else(|| name.to_owned()),
            target: product.instance,
            objective,
            dispatch_status: status,
            state: product
                .state
                .unwrap_or_else(|| dispatch_state(status).to_owned()),
            session_id: None,
            run_id: product.run_id,
            job_id: product.job_id,
            report_delivery: product.report_delivery.or(requested_delivery),
            result: nonempty(product.result),
            diagnostic: (status == ToolStatus::Error)
                .then(|| output.unwrap_or_default().to_owned()),
            time_created: None,
            time_completed: None,
        };
    }

    Delegation {
        call_id: call_id.to_owned(),
        tool: name.to_owned(),
        product: name.to_owned(),
        target: field("subagent_type").or_else(|| field("category")),
        objective,
        dispatch_status: status,
        state: dispatch_state(status).to_owned(),
        session_id: None,
        run_id: None,
        job_id: None,
        report_delivery: requested_delivery,
        result: output.and_then(|value| nonempty(value.to_owned())),
        diagnostic: (status == ToolStatus::Error)
            .then(|| output.unwrap_or("subagent dispatch failed").to_owned()),
        time_created: None,
        time_completed: None,
    }
}

/// Parse a native task result without depending on the tool's wire name.
#[must_use]
pub fn task_envelope(output: &str) -> Option<TaskEnvelope> {
    let tag = output.lines().find(|line| line.starts_with("<task "))?;
    Some(TaskEnvelope {
        session_id: attribute(tag, "id"),
        job_id: attribute(tag, "job"),
        state: attribute(tag, "state"),
        report_delivery: attribute(tag, "reportDelivery"),
        result: enclosed(output, "task_result"),
    })
}

/// Parse either supported subagent envelope without consulting a wire tool name.
#[must_use]
pub fn output_envelope(output: &str) -> Option<OutputEnvelope> {
    if let Some(task) = task_envelope(output) {
        let mut detail = task.session_id.as_deref().map_or_else(
            || "no child session".to_owned(),
            |id| format!("session {id}"),
        );
        if let Some(state) = task.state {
            detail.push_str(" · ");
            detail.push_str(&state);
        }
        if let Some(job) = task.job_id {
            detail.push_str(" · job ");
            detail.push_str(&job);
        }
        return Some(OutputEnvelope {
            detail,
            result: task.result,
        });
    }
    let product = product_envelope(output)?;
    let mut detail = product
        .product
        .unwrap_or_else(|| "product agent".to_owned());
    if let Some(instance) = product.instance {
        detail.push(' ');
        detail.push_str(&instance);
    }
    if let Some(state) = product.state {
        detail.push_str(" · ");
        detail.push_str(&state);
    }
    if let Some(job) = product.job_id {
        detail.push_str(" · job ");
        detail.push_str(&job);
    }
    Some(OutputEnvelope {
        detail,
        result: product.result,
    })
}

fn product_envelope(output: &str) -> Option<ProductEnvelope> {
    let tag = output
        .lines()
        .find(|line| line.starts_with("<product-agent "))?;
    Some(ProductEnvelope {
        product: attribute(tag, "product"),
        instance: attribute(tag, "instance"),
        run_id: attribute(tag, "run"),
        job_id: attribute(tag, "job"),
        state: attribute(tag, "state"),
        report_delivery: attribute(tag, "reportDelivery"),
        result: enclosed(output, "product_agent_result"),
    })
}

fn enclosed(output: &str, element: &str) -> String {
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    output
        .split_once(&open)
        .and_then(|(_, rest)| rest.split_once(&close))
        .map_or("", |(result, _)| result)
        .trim_matches('\n')
        .to_owned()
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned())
}

fn refine_from_job_output(tasks: &mut [Delegation], output: &str) {
    let Ok(job) = serde_json::from_str::<Value>(output) else {
        return;
    };
    let Some(job_id) = job.get("jobID").and_then(Value::as_str) else {
        return;
    };
    let Some(task) = tasks
        .iter_mut()
        .find(|task| task.job_id.as_deref() == Some(job_id))
    else {
        return;
    };
    if let Some(status) = job.get("status").and_then(Value::as_str) {
        task.state = status.to_owned();
    }
    if job.get("cancellationRequested").and_then(Value::as_bool) == Some(true)
        && task.state == "running"
    {
        task.state = "cancelling".to_owned();
    }
    task.report_delivery = job
        .get("reportDelivery")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| task.report_delivery.clone());
    task.result = job
        .get("result")
        .and_then(result_text)
        .or_else(|| task.result.clone());
    task.diagnostic = job
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| task.diagnostic.clone());
    task.time_created = job
        .get("timeCreated")
        .and_then(Value::as_i64)
        .or(task.time_created);
    task.time_completed = job
        .get("timeCompleted")
        .and_then(Value::as_i64)
        .or(task.time_completed);
    if let Some(subject) = job.get("subject") {
        match subject.get("kind").and_then(Value::as_str) {
            Some("childSession") => {
                task.product = "zuno".to_owned();
                task.session_id = subject
                    .get("sessionID")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| task.session_id.clone());
            }
            Some("productAgent") => {
                task.product = subject
                    .get("product")
                    .and_then(Value::as_str)
                    .unwrap_or(&task.product)
                    .to_owned();
                task.target = subject
                    .get("instance")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| task.target.clone());
                task.run_id = subject
                    .get("runID")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| task.run_id.clone());
            }
            _ => {}
        }
    }
}

fn merge_job_projections(tasks: &mut Vec<Delegation>, jobs: &[zuno_types::JobProjection]) {
    for job in jobs {
        let (tool, product, target, session_id, run_id) = match &job.subject {
            zuno_types::JobSubjectProjection::ChildSession { session_id } => (
                "task".to_owned(),
                "zuno".to_owned(),
                None,
                Some(session_id.clone()),
                None,
            ),
            zuno_types::JobSubjectProjection::ProductAgent {
                run_id,
                product,
                instance,
                tool,
            } => (
                tool.clone(),
                product.clone(),
                Some(instance.clone()),
                None,
                Some(run_id.clone()),
            ),
        };
        if let Some(task) = tasks
            .iter_mut()
            .find(|task| task.job_id.as_deref() == Some(job.id.as_str()))
        {
            task.tool = tool;
            task.product = product;
            task.target = target.or_else(|| task.target.clone());
            task.session_id = session_id.or_else(|| task.session_id.clone());
            task.run_id = run_id.or_else(|| task.run_id.clone());
            task.state = job.status.clone();
            task.report_delivery = Some(job.report_delivery.clone());
            task.result = job.result.clone().or_else(|| task.result.clone());
            task.diagnostic = job.error.clone().or_else(|| task.diagnostic.clone());
            task.time_created = Some(job.time_created);
            task.time_completed = job.time_completed;
            continue;
        }
        tasks.push(Delegation {
            call_id: format!("job:{}", job.id),
            tool,
            product,
            target,
            objective: None,
            dispatch_status: ToolStatus::Completed,
            state: job.status.clone(),
            session_id,
            run_id,
            job_id: Some(job.id.clone()),
            report_delivery: Some(job.report_delivery.clone()),
            result: job.result.clone(),
            diagnostic: job.error.clone(),
            time_created: Some(job.time_created),
            time_completed: job.time_completed,
        });
    }
}

fn result_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("text").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| (!value.is_null()).then(|| value.to_string()))
}

fn refine_from_report(tasks: &mut [Delegation], text: &str) {
    for task in tasks {
        let Some(job) = task.job_id.as_deref() else {
            continue;
        };
        let completed = format!("completed job `{job}`");
        let failed = format!("failed job `{job}`");
        let cancelled = format!("cancelled job `{job}`");
        let uncertain = format!("uncertain outcome for job `{job}`");
        if text.contains(&uncertain) {
            task.state = "uncertain".to_owned();
            task.diagnostic = Some(text.to_owned());
        } else if text.contains(&cancelled) {
            task.state = "cancelled".to_owned();
            task.diagnostic = Some(text.to_owned());
        } else if text.contains(&failed) {
            task.state = "failed".to_owned();
            task.diagnostic = Some(text.to_owned());
        } else if text.contains(&completed) {
            task.state = "completed".to_owned();
            task.result = text
                .split_once("\n\n")
                .and_then(|(_, result)| nonempty(result.to_owned()))
                .or_else(|| task.result.clone());
        }
    }
}

fn dispatch_state(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::Running => "running",
        ToolStatus::Completed => "completed",
        ToolStatus::Blocked => "blocked",
        ToolStatus::Error => "failed",
    }
}

fn state_glyph(state: &str) -> &'static str {
    match state {
        "completed" => "✓",
        "failed" => "✗",
        "cancelled" => "■",
        "uncertain" => "?",
        "running" | "cancelling" => "…",
        _ => "~",
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn format_duration(milliseconds: i64) -> String {
    if milliseconds < 1_000 {
        return format!("{}ms", milliseconds.max(0));
    }
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// Cursor, detail disclosure, and in-place cancellation confirmation.
pub struct SubagentView {
    context: ViewContext,
    base_tasks: Vec<Delegation>,
    tasks: Vec<Delegation>,
    work: Option<crate::views::ambient::WorkState>,
    work_generation: u64,
    cursor: usize,
    expanded: bool,
    confirm_cancel: Option<String>,
}

impl SubagentView {
    #[must_use]
    pub fn new(context: ViewContext, tasks: Vec<Delegation>) -> Self {
        Self {
            context,
            base_tasks: tasks.clone(),
            tasks,
            work: None,
            work_generation: 0,
            cursor: 0,
            expanded: false,
            confirm_cancel: None,
        }
    }

    #[must_use]
    pub fn with_work_state(mut self, work: crate::views::ambient::WorkState) -> Self {
        let (generation, state) = work.observe();
        merge_job_projections(&mut self.tasks, &state.jobs);
        self.work_generation = generation;
        self.work = Some(work);
        self
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.tasks.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn selected(&self) -> Option<&Delegation> {
        self.tasks.get(self.cursor)
    }

    fn refresh(&mut self) {
        let Some(work) = &self.work else {
            return;
        };
        let (generation, state) = work.observe();
        if generation == self.work_generation {
            return;
        }
        let selected_job = self.selected().and_then(|task| task.job_id.clone());
        self.tasks = self.base_tasks.clone();
        merge_job_projections(&mut self.tasks, &state.jobs);
        self.work_generation = generation;
        self.cursor = selected_job
            .and_then(|job| {
                self.tasks
                    .iter()
                    .position(|task| task.job_id.as_deref() == Some(job.as_str()))
            })
            .unwrap_or_else(|| self.cursor.min(self.tasks.len().saturating_sub(1)));
        self.confirm_cancel = None;
    }

    fn step(&mut self, step: isize) -> DialogStep {
        self.confirm_cancel = None;
        if self.tasks.len() < 2 {
            return DialogStep::Redraw;
        }
        let length = isize::try_from(self.tasks.len()).unwrap_or(isize::MAX);
        let moved = isize::try_from(self.cursor)
            .unwrap_or(0)
            .saturating_add(step);
        self.cursor = usize::try_from(moved.rem_euclid(length)).unwrap_or(0);
        DialogStep::Redraw
    }

    fn cancel_step(&mut self) -> DialogStep {
        let Some(job_id) = self
            .selected()
            .filter(|task| task.cancellable())
            .and_then(|task| task.job_id.clone())
        else {
            self.confirm_cancel = None;
            return DialogStep::Redraw;
        };
        if self.confirm_cancel.as_deref() != Some(job_id.as_str()) {
            self.confirm_cancel = Some(job_id);
            self.expanded = true;
            return DialogStep::Redraw;
        }
        self.confirm_cancel = None;
        if let Some(task) = self.tasks.get_mut(self.cursor) {
            task.state = "cancelling".to_owned();
        }
        DialogStep::Emitted(DialogOutcome::JobCancel { job_id })
    }

    fn detail(&self, width: usize) -> Vec<Line<'static>> {
        let Some(task) = self.selected() else {
            return vec![Line::from(Span::styled(
                EMPTY.to_owned(),
                self.context.muted(),
            ))];
        };
        let mut lines = Vec::new();
        let mut row = |label: &str, value: String| {
            lines.push(Line::from(Span::styled(
                truncate(&format!("  {label} {value}"), width),
                self.context.muted(),
            )));
        };
        row("product", task.product.clone());
        row(
            "target",
            task.target
                .clone()
                .unwrap_or_else(|| "not reported".to_owned()),
        );
        row("status", task.state.clone());
        row("elapsed", task.elapsed());
        row(
            "job",
            task.job_id
                .clone()
                .unwrap_or_else(|| "foreground".to_owned()),
        );
        row(
            "report",
            task.report_delivery
                .clone()
                .unwrap_or_else(|| "foreground".to_owned()),
        );
        if let Some(session) = &task.session_id {
            row("session", session.clone());
            row("note", CHILD_TRANSCRIPT_NOTE.to_owned());
        }
        if let Some(run) = &task.run_id {
            row("run", run.clone());
        }
        if let Some(result) = &task.result {
            row("result", result.clone());
        }
        if let Some(diagnostic) = &task.diagnostic {
            row("diagnostic", diagnostic.clone());
        }
        row("safety", task.safety().to_owned());
        lines
    }
}

impl Dialog for SubagentView {
    fn id(&self) -> &'static str {
        SUBAGENT_DIALOG_ID
    }

    fn title(&self) -> String {
        if self.tasks.is_empty() {
            "Subagents".to_owned()
        } else {
            format!(
                "Subagents  {}/{}",
                self.cursor.saturating_add(1),
                self.tasks.len()
            )
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.refresh();
        let body = usize::from(width.saturating_sub(2)).max(1);
        let mut lines = Vec::new();
        for (index, task) in self.tasks.iter().enumerate() {
            let marker = if index == self.cursor { "›" } else { " " };
            lines.push(Line::from(Span::styled(
                truncate(&format!("{marker} {}", task.headline(body)), body),
                if index == self.cursor {
                    self.context.element()
                } else {
                    self.context.muted()
                },
            )));
        }
        if self.tasks.is_empty() {
            lines.push(Line::from(Span::styled(
                EMPTY.to_owned(),
                self.context.muted(),
            )));
            return lines;
        }
        if self.expanded {
            lines.push(Line::from(Span::raw(String::new())));
            lines.extend(self.detail(body));
        }
        if let Some(job) = &self.confirm_cancel {
            lines.push(Line::from(Span::styled(
                truncate(
                    &format!("press x again to cancel job {job}; the call will not be replayed"),
                    body,
                ),
                self.context.warning(),
            )));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("←→", "job"),
            ("enter", "details"),
            ("x x", "cancel"),
            ("esc", "close"),
        ]
    }

    fn width(&self) -> DialogWidth {
        DialogWidth::Large
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["dialog.subagent", "session"]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        self.refresh();
        match action.name {
            "session_child_cycle" | "dialog.select.next" => self.step(1),
            "session_child_cycle_reverse" | "dialog.select.prev" => self.step(-1),
            "session_child_first" | "dialog.select.home" => {
                self.cursor = 0;
                self.confirm_cancel = None;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.tasks.len().saturating_sub(1);
                self.confirm_cancel = None;
                DialogStep::Redraw
            }
            "dialog.select.submit" => {
                self.expanded = !self.expanded;
                self.confirm_cancel = None;
                DialogStep::Redraw
            }
            "subagent_cancel" => self.cancel_step(),
            "session_parent" | "session_interrupt" => {
                DialogStep::Resolved(DialogOutcome::Cancelled)
            }
            _ => DialogStep::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: &MouseEvent, body: Rect) -> DialogStep {
        self.refresh();
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
        let index = usize::from(event.row.saturating_sub(body.top()));
        if index >= self.tasks.len() {
            return DialogStep::Ignored;
        }
        self.cursor = index;
        self.expanded = true;
        self.confirm_cancel = None;
        DialogStep::Redraw
    }
}
