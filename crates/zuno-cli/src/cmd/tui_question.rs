//! Session-owned presentation of durable questions.
//!
//! Only the service writes answers and admits input. A dialog sends a
//! revision-checked command and observes its receipt; it never returns answers
//! to a tool, resumes a Goal, or starts Work. Disconnect drops presentation
//! state without resolving outstanding requests.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use async_trait::async_trait;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use zuno_session_control::QuestionService;
use zuno_tool::InterruptHandle;
use zuno_tool::question::{QuestionError, QuestionPort, QuestionResult};
use zuno_tui::app::{AppEvent, Component, EventResult, TerminalEvent};
use zuno_tui::crossterm::event::{KeyEvent, MouseEvent};
use zuno_tui::keybind::{ActionComponent, Definition, PendingPrefix};
use zuno_tui::ratatui::Frame;
use zuno_tui::ratatui::layout::Rect;
use zuno_tui::ratatui::text::Line;
use zuno_tui::views::ViewContext;
use zuno_tui::views::basics::PromptDialog;
use zuno_tui::views::dialog::{
    BodyAnchor, Dialog, DialogHost, DialogOutcome, DialogPlacement, DialogStep, DialogWidth,
};
use zuno_tui::views::picker::{Item, SelectDialog};
use zuno_tui::views::question::{
    QuestionOption as TuiQuestionOption, QuestionPrompt, QuestionRequest as TuiQuestionRequest,
};
use zuno_tui::views::session::SessionScreen;
use zuno_tui::views::toast::Toast;
use zuno_types::question::{
    PlanQuestionDecision, QuestionAction, QuestionAnswers, QuestionCommand, QuestionMode,
    QuestionPurpose, QuestionReceipt, QuestionRequest, QuestionSpec, QuestionState, QuestionView,
};

const QUESTION_CHANNEL_CAPACITY: usize = 32;
const QUESTIONS_DIALOG_ID: &str = "questions";
const PLAN_RISK_DIALOG_ID: &str = "question.plan_risk";

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct ServiceBinding {
    service: Arc<QuestionService>,
    session_id: String,
}

enum PresenterCommand {
    List,
    Open(String),
    Apply {
        question: Box<QuestionView>,
        command: QuestionCommand,
    },
}

enum PresenterUpdate {
    Pending(Vec<QuestionView>),
    List,
    Open(Box<QuestionView>),
    Applied(Box<QuestionReceipt>),
    Error(String),
}

/// Replacement hosts and interactive children inherit this same port.
///
/// Binding happens once, immediately after the first host opens its database.
/// Tool composition can hold the port before then; invoking it unbound fails
/// explicitly. No tool or turn owns the lifetime of a displayed request.
pub(crate) struct QuestionBroker {
    binding: OnceLock<ServiceBinding>,
    commands: mpsc::Sender<PresenterCommand>,
    command_source: Mutex<Option<mpsc::Receiver<PresenterCommand>>>,
    updates: mpsc::Sender<PresenterUpdate>,
    pending: Mutex<mpsc::Receiver<PresenterUpdate>>,
    wake: mpsc::Sender<TerminalEvent>,
    presentation_blocks_turn: AtomicBool,
    work_ready: Arc<Notify>,
    replaced_identity: Mutex<Option<zuno_types::execution::TurnExecutionIdentity>>,
}

impl QuestionBroker {
    pub(crate) fn new(wake: mpsc::Sender<TerminalEvent>) -> Self {
        let (commands, command_source) = mpsc::channel(QUESTION_CHANNEL_CAPACITY);
        let (updates, pending) = mpsc::channel(QUESTION_CHANNEL_CAPACITY);
        Self {
            binding: OnceLock::new(),
            commands,
            command_source: Mutex::new(Some(command_source)),
            updates,
            pending: Mutex::new(pending),
            wake,
            presentation_blocks_turn: AtomicBool::new(false),
            work_ready: Arc::new(Notify::new()),
            replaced_identity: Mutex::new(None),
        }
    }

    pub(crate) fn attach_service(
        &self,
        service: Arc<QuestionService>,
        session_id: impl Into<String>,
    ) -> Result<(), String> {
        self.binding
            .set(ServiceBinding {
                service,
                session_id: session_id.into(),
            })
            .map_err(|_| "the question presenter is already bound to a session".to_owned())
    }

    fn service(&self) -> QuestionResult<&Arc<QuestionService>> {
        self.binding
            .get()
            .map(|binding| &binding.service)
            .ok_or_else(|| QuestionError::Unavailable("the TUI session is not open".to_owned()))
    }

    pub(crate) fn work_notifications(&self) -> Arc<Notify> {
        Arc::clone(&self.work_ready)
    }

    pub(crate) fn host_replaced(&self, identity: zuno_types::execution::TurnExecutionIdentity) {
        *locked(&self.replaced_identity) = Some(identity);
        let _ = self.wake.try_send(TerminalEvent::Wake);
    }

    fn notify_work(&self, receipt: &QuestionReceipt) {
        if receipt.input_id.is_some() {
            // A permit survives until the driver returns to its idle select.
            // Early Plan approval has no input ID and cannot wake the Plan host.
            self.work_ready.notify_one();
        }
    }

    /// Native `/questions [list | open <request-id> | <request-id>]` handler.
    ///
    /// Route this presentation control before admitting model input, including
    /// while the current turn is still streaming.
    pub(crate) fn show_questions(&self, arguments: &str) -> Result<(), String> {
        let fields = arguments.split_whitespace().collect::<Vec<_>>();
        let command = match fields.as_slice() {
            [] | ["list"] => PresenterCommand::List,
            [id] => PresenterCommand::Open((*id).to_owned()),
            ["open", id] => PresenterCommand::Open((*id).to_owned()),
            _ => {
                return Err(
                    "usage: /questions [list | open <request-id> | <request-id>]".to_owned(),
                );
            }
        };
        self.send(command)
    }

    fn send(&self, command: PresenterCommand) -> Result<(), String> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    "question actions are still being saved; try again".to_owned()
                }
                mpsc::error::TrySendError::Closed(_) => {
                    "the question presenter has stopped; pending requests were retained".to_owned()
                }
            })
    }

    fn apply(&self, question: QuestionView, action: QuestionAction) -> Result<(), String> {
        let command = question_command(&question, action)?;
        self.send(PresenterCommand::Apply {
            question: Box::new(question),
            command,
        })
    }

    async fn publish(&self, update: PresenterUpdate) {
        if let PresenterUpdate::Applied(receipt) = &update {
            self.notify_work(receipt);
        }
        if self.updates.send(update).await.is_ok() {
            let _ = self.wake.try_send(TerminalEvent::Wake);
        }
    }

    /// Receipts notify immediately; reopening reads authoritative state again.
    ///
    /// Subscribe before reading to avoid a recovery race. Shutdown never
    /// resolves requests; an action already submitted may finish its commit.
    pub(crate) async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let stopping = shutdown.clone();
        tokio::select! {
            biased;
            _ = async {
                while !*shutdown.borrow() {
                    if shutdown.changed().await.is_err() {
                        break;
                    }
                }
            } => {}
            _ = self.run_presenter(stopping) => {}
        }
    }

    async fn run_presenter(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let Some(binding) = self.binding.get() else {
            self.publish(PresenterUpdate::Error(
                "the question presenter has no service".to_owned(),
            ))
            .await;
            return;
        };
        let Some(mut commands) = locked(&self.command_source).take() else {
            return;
        };
        let mut changes = binding.service.subscribe();
        let mut sessions = BTreeSet::from([binding.session_id.clone()]);
        let mut previous = None;
        self.refresh(&sessions, &mut previous).await;
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        PresenterCommand::List => {
                            self.refresh(&sessions, &mut previous).await;
                            self.publish(PresenterUpdate::List).await;
                        }
                        PresenterCommand::Open(id) => {
                            let mut found = None;
                            for session in &sessions {
                                match binding.service.get(session, &id).await {
                                    Ok(view) => { found = Some(Ok(view)); break; }
                                    Err(QuestionError::NotFound { .. }) => {}
                                    Err(error) => { found = Some(Err(error)); break; }
                                }
                            }
                            match found {
                                Some(Ok(view)) => self.publish(PresenterUpdate::Open(Box::new(view))).await,
                                Some(Err(error)) => self.publish(PresenterUpdate::Error(error.to_string())).await,
                                None => self.publish(PresenterUpdate::Error(format!(
                                    "question `{id}` was not found in this session"
                                ))).await,
                            }
                        }
                        PresenterCommand::Apply { question, command } => {
                            match binding.service.apply(
                                &question.origin.session_id, &question.id, command,
                            ).await {
                                Ok(receipt) => self.publish(PresenterUpdate::Applied(Box::new(receipt))).await,
                                Err(error) => self.publish(PresenterUpdate::Error(format!(
                                    "question action was not acknowledged: {error}; /questions reloads durable state"
                                ))).await,
                            }
                            self.refresh(&sessions, &mut previous).await;
                        }
                    }
                }
                change = changes.recv() => {
                    match change {
                        Ok(receipt) => {
                            self.notify_work(&receipt);
                            // Children inherit this exact service and retain their
                            // own session identity when presented here.
                            sessions.insert(receipt.question.origin.session_id);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                    self.refresh(&sessions, &mut previous).await;
                }
            }
        }
    }

    async fn refresh(&self, sessions: &BTreeSet<String>, previous: &mut Option<Vec<QuestionView>>) {
        let Ok(service) = self.service() else {
            return;
        };
        let mut pending = Vec::new();
        for session in sessions {
            match service.pending(session).await {
                Ok(questions) => pending.extend(questions),
                Err(error) => {
                    self.publish(PresenterUpdate::Error(format!(
                        "pending questions could not be loaded: {error}"
                    )))
                    .await;
                    return;
                }
            }
        }
        pending.sort_by(|left, right| {
            (left.time_created, &left.id).cmp(&(right.time_created, &right.id))
        });
        if previous.as_ref() != Some(&pending) {
            *previous = Some(pending.clone());
            self.publish(PresenterUpdate::Pending(pending)).await;
        }
    }
}

#[async_trait]
impl QuestionPort for QuestionBroker {
    async fn open(&self, spec: QuestionSpec) -> QuestionResult<QuestionReceipt> {
        self.service()?.open(spec).await
    }

    async fn apply(
        &self,
        session_id: &str,
        request_id: &str,
        command: QuestionCommand,
    ) -> QuestionResult<QuestionReceipt> {
        self.service()?.apply(session_id, request_id, command).await
    }

    async fn get(&self, session_id: &str, request_id: &str) -> QuestionResult<QuestionView> {
        self.service()?.get(session_id, request_id).await
    }

    async fn pending(&self, session_id: &str) -> QuestionResult<Vec<QuestionView>> {
        self.service()?.pending(session_id).await
    }

    async fn wait_for_change(
        &self,
        session_id: &str,
        request_id: &str,
        after_revision: i64,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> QuestionResult<QuestionView> {
        self.service()?
            .wait_for_change(session_id, request_id, after_revision, interrupt)
            .await
    }
}

fn question_command(
    question: &QuestionView,
    action: QuestionAction,
) -> Result<QuestionCommand, String> {
    let value = serde_json::to_value((
        &question.origin.session_id,
        &question.id,
        question.revision,
        &action,
    ))
    .map_err(|error| error.to_string())?;
    Ok(QuestionCommand {
        // Retrying an exact action after a lost acknowledgement reuses its key.
        // Changed input or a newer revision is a different command.
        command_id: format!("tui-question:{}", zuno_orchestration::sha256_json(&value)),
        expected_revision: question.revision,
        action,
    })
}

fn to_tui_request(request: &QuestionRequest) -> TuiQuestionRequest {
    TuiQuestionRequest {
        question: request.question.clone(),
        header: request.header.clone(),
        options: request
            .options
            .iter()
            .map(|option| TuiQuestionOption {
                label: option.label.clone(),
                description: option.description.clone(),
            })
            .collect(),
        multiple: request.multiple,
        custom: request.custom,
    }
}

fn positional_answers(question: &QuestionView) -> Vec<Vec<String>> {
    question
        .questions
        .iter()
        .map(|item| {
            question
                .draft_answers
                .get(&item.id)
                .or_else(|| question.answers.get(&item.id))
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn keyed_answers(
    question: &QuestionView,
    answers: Vec<Vec<String>>,
) -> Result<QuestionAnswers, String> {
    if answers.len() != question.questions.len() {
        return Err("the answer does not match the displayed question items".to_owned());
    }
    Ok(question
        .questions
        .iter()
        .zip(answers)
        .map(|(item, answer)| (item.id.clone(), answer))
        .collect())
}

fn plan_decision(answers: &[Vec<String>]) -> Result<PlanQuestionDecision, String> {
    match answers {
        [answer] => match answer.as_slice() {
            [choice] if choice == "approve" => Ok(PlanQuestionDecision::Approve),
            [choice] if choice == "decline" => Ok(PlanQuestionDecision::Decline),
            _ => Err("choose exactly `approve` or `decline`; no decision was submitted".to_owned()),
        },
        _ => Err("choose exactly `approve` or `decline`; no decision was submitted".to_owned()),
    }
}

fn plan_review_gate(question: &QuestionView) -> Result<zuno_review::PlanReviewGate, String> {
    let binding = question
        .plan
        .as_ref()
        .ok_or_else(|| "the Plan question has no stored authorization binding".to_owned())?;
    serde_json::from_value(binding.review_gate.clone())
        .map_err(|error| format!("the stored Plan review gate is invalid: {error}"))
}

struct ActiveQuestion {
    view: QuestionView,
    risk_prompt: bool,
    invalidated: bool,
}

/// The existing PermissionBridge pumps this adapter on every terminal event.
pub(crate) struct QuestionBridge {
    context: ViewContext,
    broker: Arc<QuestionBroker>,
    pending: Vec<QuestionView>,
    seen: BTreeSet<String>,
    active: Option<ActiveQuestion>,
    requested: VecDeque<Box<dyn Dialog>>,
    captured: Arc<Mutex<VecDeque<(String, DialogOutcome)>>>,
    toasts: Vec<Toast>,
}

impl QuestionBridge {
    pub(crate) fn new(context: ViewContext, broker: Arc<QuestionBroker>) -> Self {
        Self {
            context,
            broker,
            pending: Vec::new(),
            seen: BTreeSet::new(),
            active: None,
            requested: VecDeque::new(),
            captured: Arc::new(Mutex::new(VecDeque::new())),
            toasts: Vec::new(),
        }
    }

    pub(crate) fn resolve(&mut self, answers: Vec<Vec<String>>) -> EventResult {
        let Some(active) = self.active.take() else {
            return EventResult::IGNORED;
        };
        let view = active.view;
        if view.purpose == QuestionPurpose::PlanAuthorization {
            match plan_decision(&answers)
                .and_then(|decision| Ok((decision, plan_review_gate(&view)?)))
            {
                Ok((PlanQuestionDecision::Approve, zuno_review::PlanReviewGate::Draft { .. })) => {
                    self.request_risk_reason(view);
                }
                Ok((decision, _)) => self.submit(
                    view,
                    QuestionAction::PlanDecision {
                        decision,
                        risk_reason: None,
                    },
                ),
                Err(error) => self.refuse(view, error),
            }
        } else {
            match keyed_answers(&view, answers).and_then(|answers| {
                view.validate_answers(&answers)
                    .map_err(|error| error.to_string())?;
                Ok(answers)
            }) {
                Ok(answers) if answers.values().all(|answer| answer.is_empty()) => {
                    self.submit(
                        view,
                        QuestionAction::Defer {
                            draft_answers: answers,
                        },
                    );
                }
                Ok(answers) => self.submit(view, QuestionAction::Answer { answers }),
                Err(error) => self.refuse(view, error),
            }
        }
        EventResult::REDRAW
    }

    pub(crate) fn cancel(&mut self) -> EventResult {
        let Some(active) = self.active.take() else {
            return EventResult::IGNORED;
        };
        self.submit(active.view, QuestionAction::Cancel);
        EventResult::REDRAW
    }

    fn defer(&mut self, answers: Vec<Vec<String>>) {
        let Some(active) = self.active.take() else {
            return;
        };
        let view = active.view;
        match keyed_answers(&view, answers).and_then(|draft_answers| {
            view.validate_draft_answers(&draft_answers)
                .map_err(|error| error.to_string())?;
            Ok(draft_answers)
        }) {
            // Even a complete Plan choice is only saved form data here.
            Ok(draft_answers) => self.submit(view, QuestionAction::Defer { draft_answers }),
            Err(error) => self.refuse(view, error),
        }
    }

    fn submit(&mut self, view: QuestionView, action: QuestionAction) {
        if let Err(error) = self.broker.apply(view.clone(), action) {
            self.refuse(view, error);
        }
    }

    fn refuse(&mut self, view: QuestionView, error: String) {
        self.toasts.push(Toast::error(error));
        self.active = None;
        self.open_question(view);
    }

    fn request_risk_reason(&mut self, view: QuestionView) {
        let dialog = PromptDialog::new(
            self.context.clone(),
            PLAN_RISK_DIALOG_ID,
            "Draft review: explain the risk you accept",
            "",
        );
        self.active = Some(ActiveQuestion {
            view,
            risk_prompt: true,
            invalidated: false,
        });
        self.requested
            .push_back(self.capture(Box::new(dialog), None));
    }

    fn risk_reason(&mut self, outcome: DialogOutcome) {
        let Some(active) = self.active.take().filter(|active| active.risk_prompt) else {
            return;
        };
        match outcome {
            DialogOutcome::Submitted { text, .. } if !text.trim().is_empty() => self.submit(
                active.view,
                QuestionAction::PlanDecision {
                    decision: PlanQuestionDecision::Approve,
                    risk_reason: Some(text),
                },
            ),
            DialogOutcome::Submitted { .. } => {
                self.toasts.push(Toast::warning(
                    "a Draft review requires your risk reason; no approval was submitted",
                ));
                self.request_risk_reason(active.view);
            }
            DialogOutcome::Cancelled => self.submit(active.view, QuestionAction::Cancel),
            _ => self.active = Some(active),
        }
    }

    fn capture(&self, dialog: Box<dyn Dialog>, identity: Option<String>) -> Box<dyn Dialog> {
        Box::new(CapturedDialog {
            inner: dialog,
            captured: Arc::clone(&self.captured),
            identity,
            wake: self.broker.wake.clone(),
        })
    }

    fn open_question(&mut self, view: QuestionView) {
        if view.state != QuestionState::Pending {
            self.toasts.push(Toast::info(format!(
                "question {} is already {}",
                view.id,
                view.state.as_str(),
            )));
            return;
        }
        if self.active.is_some() {
            self.toasts.push(Toast::info(
                "finish or defer the current question before opening another",
            ));
            return;
        }
        if view.questions.is_empty() {
            self.toasts.push(Toast::error(
                "the stored request has no question items; no answer was submitted",
            ));
            return;
        }
        let answers = positional_answers(&view);
        let mut questions = view
            .questions
            .iter()
            .map(|item| to_tui_request(&item.question))
            .collect::<Vec<_>>();
        if view.purpose == QuestionPurpose::PlanAuthorization {
            let Some(binding) = view.plan.as_ref() else {
                self.toasts.push(Toast::error(
                    "the Plan question has no stored authorization binding",
                ));
                return;
            };
            if let Err(error) = plan_review_gate(&view) {
                self.toasts.push(Toast::error(error));
                return;
            }
            questions[0].question = format!(
                "{}\nPlan: {} · revision {}\nWork: {} · {}/{}{}\nReview: {}",
                questions[0].question,
                binding.plan_id,
                binding.plan_revision,
                binding.work_identity.agent,
                binding.work_identity.provider_id,
                binding.work_identity.model_id,
                binding
                    .work_identity
                    .reasoning
                    .as_deref()
                    .map_or_else(String::new, |reasoning| format!(" · {reasoning}")),
                binding.review_gate,
            );
        }
        let dialog = QuestionPrompt::new(self.context.clone(), questions).with_answers(answers);
        let identity = format!("{} · revision {}", view.id, view.revision);
        self.broker.presentation_blocks_turn.store(
            view.mode == QuestionMode::Blocking || view.purpose == QuestionPurpose::RequiredInput,
            Ordering::Release,
        );
        self.seen.insert(view.id.clone());
        self.active = Some(ActiveQuestion {
            view,
            risk_prompt: false,
            invalidated: false,
        });
        self.requested
            .push_back(self.capture(Box::new(dialog), Some(identity)));
    }

    fn open_list(&mut self) {
        let items = self
            .pending
            .iter()
            .map(|question| {
                let answered = question.answers.values().filter(|a| !a.is_empty()).count();
                let drafted = question
                    .draft_answers
                    .values()
                    .filter(|a| !a.is_empty())
                    .count();
                Item::new(format!(
                    "{} · {}",
                    question.id,
                    question
                        .questions
                        .first()
                        .map_or("Question", |item| { item.question.header.as_str() }),
                ))
                .described(format!(
                    "{} · {} · {answered}/{} answered · {drafted} draft · revision {} · {}",
                    question.purpose.as_str(),
                    question.mode.as_str(),
                    question.questions.len(),
                    question.revision,
                    question.origin.session_id,
                ))
                .valued(question.id.clone())
            })
            .collect();
        let dialog = SelectDialog::new(
            QUESTIONS_DIALOG_ID,
            format!("Questions ({} pending)", self.pending.len()),
            self.context.clone(),
            items,
        );
        self.requested
            .push_back(self.capture(Box::new(dialog), None));
    }

    pub(crate) fn open_next(&mut self, host: &mut DialogHost) -> EventResult {
        let mut changed = false;
        let captured = std::mem::take(&mut *locked(&self.captured));
        for (dialog, outcome) in captured {
            changed = true;
            match outcome {
                DialogOutcome::QuestionDeferred(answers) => self.defer(answers),
                DialogOutcome::Selected { value, .. } if dialog == QUESTIONS_DIALOG_ID => {
                    if let Err(error) = self.broker.show_questions(&value) {
                        self.toasts.push(Toast::error(error));
                    }
                }
                outcome if dialog == PLAN_RISK_DIALOG_ID => self.risk_reason(outcome),
                _ => {}
            }
        }
        loop {
            let update = locked(&self.broker.pending).try_recv().ok();
            let Some(update) = update else { break };
            changed = true;
            match update {
                PresenterUpdate::Pending(pending) => {
                    if pending.len() != self.pending.len() {
                        self.toasts.push(Toast::info(format!(
                            "{} pending question(s) · /questions to reopen",
                            pending.len(),
                        )));
                    }
                    self.pending = pending;
                    if let Some(active) = self.active.as_mut() {
                        active.invalidated = !self.pending.iter().any(|view| {
                            view.id == active.view.id && view.revision == active.view.revision
                        });
                    }
                }
                PresenterUpdate::List => self.open_list(),
                PresenterUpdate::Open(view) => self.open_question(*view),
                PresenterUpdate::Applied(receipt) => {
                    self.toasts.push(Toast::success(format!(
                        "question {}: {}{}",
                        receipt.question.id,
                        receipt.question.state.as_str(),
                        if receipt.input_id.is_some() {
                            " · response saved to the session inbox"
                        } else {
                            ""
                        },
                    )));
                }
                PresenterUpdate::Error(error) => self.toasts.push(Toast::error(error)),
            }
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.invalidated)
            && matches!(
                host.active(),
                Some(zuno_tui::views::question::DIALOG_ID | PLAN_RISK_DIALOG_ID)
            )
        {
            host.dismiss();
            self.active = None;
            self.requested.clear();
            self.toasts.push(Toast::info(
                "this question changed in durable storage; /questions shows its current state",
            ));
        }
        for toast in self.toasts.drain(..) {
            host.toasts_mut().show(toast);
            changed = true;
        }
        if !host.is_open() {
            if let Some(dialog) = self.requested.pop_front() {
                host.open(dialog);
                changed = true;
            } else if self.active.is_none()
                && let Some(view) = self
                    .pending
                    .iter()
                    .find(|view| {
                        view.mode == QuestionMode::Blocking && !self.seen.contains(&view.id)
                    })
                    .cloned()
            {
                self.open_question(view);
                if let Some(dialog) = self.requested.pop_front() {
                    host.open(dialog);
                    changed = true;
                }
            }
        }
        if changed {
            EventResult::REDRAW
        } else {
            EventResult::IGNORED
        }
    }
}

/// Preserve the existing composer/modal geometry while showing the actual wait
/// state: opening an optional question or Plan approval does not stop a turn.
pub(crate) struct QuestionScreen {
    inner: SessionScreen,
    broker: Arc<QuestionBroker>,
}

impl QuestionScreen {
    pub(crate) fn new(inner: SessionScreen, broker: Arc<QuestionBroker>) -> Self {
        Self { inner, broker }
    }
}

impl Component for QuestionScreen {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inner.render(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        let identity = locked(&self.broker.replaced_identity).take();
        let changed = identity.is_some();
        if let Some(identity) = identity {
            let model = format!("{}/{}", identity.provider_id, identity.model_id);
            let effort = identity
                .reasoning
                .as_deref()
                .and_then(|value| value.parse().ok());
            let catalog = self.inner.catalog_mut();
            catalog.agent = Some(identity.agent.clone());
            catalog.model = Some(model.clone());
            catalog.effort = effort;
            if let Some(entry) = catalog.models.iter().find(|entry| entry.id == model) {
                catalog.reasoning = entry.reasoning;
            } else if effort.is_some() {
                catalog.reasoning = true;
            }
            self.inner
                .status_mut()
                .set_configured_agent(&identity.agent);
            self.inner.status_mut().set_configured_model(&model);
            self.inner.status_mut().set_effort(effort);
            self.inner.sidebar_mut().ambient_mut().agent = Some(identity.agent);
            self.inner.sidebar_mut().ambient_mut().model = Some(model);
        }
        let result = self.inner.handle_event(event);
        if changed {
            EventResult::REDRAW.merge(result)
        } else {
            result
        }
    }
}

impl ActionComponent for QuestionScreen {
    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult {
        self.inner.handle_action(action, event)
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        self.inner.focused_scopes()
    }

    fn pending_changed(&mut self, pending: &PendingPrefix) -> EventResult {
        self.inner.pending_changed(pending)
    }

    fn drain_dialogs(&mut self) -> Vec<Box<dyn Dialog>> {
        self.inner.drain_dialogs()
    }

    fn drain_toasts(&mut self) -> Vec<Toast> {
        self.inner.drain_toasts()
    }

    fn dialog_region(&self, dialog: &'static str, area: Rect) -> Option<Rect> {
        self.inner.dialog_region(dialog, area)
    }

    fn apply_dialog_outcome(
        &mut self,
        dialog: &'static str,
        outcome: &DialogOutcome,
    ) -> EventResult {
        self.inner.apply_dialog_outcome(dialog, outcome)
    }

    fn observe_modal(&mut self, active: Option<&'static str>) {
        self.inner.observe_modal(active);
        if active == Some(zuno_tui::views::question::DIALOG_ID)
            && !self.broker.presentation_blocks_turn.load(Ordering::Acquire)
        {
            self.inner.status_mut().set_awaiting_user(None);
            self.inner
                .transcript_mut()
                .transcript_mut()
                .set_awaiting_user(None);
        }
    }
}

/// Capture additional native outcomes through the existing host seam.
/// Answers and Escape keep PermissionBridge's resolve/cancel hooks.
struct CapturedDialog {
    inner: Box<dyn Dialog>,
    captured: Arc<Mutex<VecDeque<(String, DialogOutcome)>>>,
    identity: Option<String>,
    wake: mpsc::Sender<TerminalEvent>,
}

impl CapturedDialog {
    fn capture_step(&self, step: DialogStep) -> DialogStep {
        if matches!(step, DialogStep::Resolved(_)) {
            // Pointer and unbound-key outcomes are pumped on the next event.
            let _ = self.wake.try_send(TerminalEvent::Wake);
        }
        if let DialogStep::Resolved(outcome) = &step
            && (matches!(outcome, DialogOutcome::QuestionDeferred(_))
                || matches!(self.inner.id(), QUESTIONS_DIALOG_ID | PLAN_RISK_DIALOG_ID))
        {
            locked(&self.captured).push_back((self.inner.id().to_owned(), outcome.clone()));
        }
        step
    }
}

impl Dialog for CapturedDialog {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn title(&self) -> String {
        self.identity.as_ref().map_or_else(
            || self.inner.title(),
            |identity| format!("{} · {identity}", self.inner.title()),
        )
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.inner.lines(width)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        self.inner.hints()
    }

    fn anchor(&self) -> BodyAnchor {
        self.inner.anchor()
    }

    fn width(&self) -> DialogWidth {
        self.inner.width()
    }

    fn placement(&self) -> DialogPlacement {
        self.inner.placement()
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        self.inner.focused_scopes()
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        let step = self.inner.handle_action(action, event);
        self.capture_step(step)
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        let step = self.inner.handle_typed(key);
        self.capture_step(step)
    }

    fn handle_mouse(&mut self, event: &MouseEvent, body: Rect) -> DialogStep {
        let step = self.inner.handle_mouse(event, body);
        self.capture_step(step)
    }

    fn desired_height(&self, content_rows: u16, available: u16) -> u16 {
        self.inner.desired_height(content_rows, available)
    }
}

#[cfg(test)]
#[path = "tui_question_tests.rs"]
mod tests;
