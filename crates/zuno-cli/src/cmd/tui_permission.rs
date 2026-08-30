//! The interactive surface's permission collaborator, and how it reaches a human.
//!
//! `run` fails closed because it has nobody to ask; the TUI has somebody, so it must
//! actually ask. The obstacle is that the two ends live on different threads and
//! neither may wait on the other: the turn driver calls
//! [`PermissionAsker::ask`] and needs an answer before it can continue, while the
//! render loop is the only thing that can obtain one and is forbidden from blocking
//! inside a component (see `zuno_tui::views::dialog`). A direct call in either
//! direction deadlocks.
//!
//! So the two halves never call each other:
//!
//! - [`PermissionBroker::ask`] parks a [`PermissionRequest`] and awaits a
//!   [`tokio::sync::oneshot`]. Nothing about the terminal is touched.
//! - [`PermissionBridge`], a component in the tree, notices the parked request while
//!   handling an ordinary event, opens the prompt todo 76 built, and sends the
//!   [`ReplyKind`] back down that oneshot when the user resolves it.
//!
//! # A dropped answer is a refusal
//!
//! If the TUI exits with a request outstanding, the oneshot's sender is dropped and
//! [`PermissionBroker::ask`] returns [`ToolError::Denied`]. That direction is not a
//! choice: a tool call whose authorization can no longer be obtained must not run.
//!
//! # `always` is remembered here, for the process
//!
//! The prompt's own copy promises "until Zuno is restarted", and the dispatcher's
//! per-call asker cannot carry a grant between calls. The broker therefore keeps the
//! approved `(permission, patterns)` pairs and answers a matching later ask without
//! prompting again — which is what makes `always` different from `once` rather than a
//! second spelling of it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zuno_db::human_request::{
    HumanRequestKind, HumanRequestState, HumanRequestStore, NewHumanRequest,
};
use zuno_error::ToolError;
use zuno_goal::GoalStore;
use zuno_permission::{PermissionRequest, ReplyKind};
use zuno_tool::{PermissionAsk, PermissionAsker, PermissionOrigin};
use zuno_tui::app::{AppEvent, Component, EventResult, TerminalEvent};
use zuno_tui::keybind::{ActionComponent, Definition, PendingPrefix};
use zuno_tui::ratatui::Frame;
use zuno_tui::ratatui::layout::Rect;
use zuno_tui::views::ViewContext;
use zuno_tui::views::dialog::{DialogHost, DialogOutcome};
use zuno_tui::views::permission::PermissionPrompt;

use super::tui_question::QuestionBridge;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A session-scoped `(permission, patterns)` grant.
type Grant = (String, String, Vec<String>);
type PendingKey = (String, String);

struct Pending {
    answer: oneshot::Sender<ReplyKind>,
    grant: Option<Grant>,
}

#[derive(Default)]
struct Parked {
    waiting: VecDeque<PermissionRequest>,
    pending: HashMap<PendingKey, Pending>,
    presented: HashSet<PendingKey>,
    standing: Vec<Grant>,
    surfaces: usize,
}

#[derive(Clone)]
struct DurablePermissions {
    store: HumanRequestStore,
    goals: Arc<GoalStore>,
    recovery_session_id: String,
}

/// The asker a surface with a human attached hands to the dispatcher.
pub(crate) struct PermissionBroker {
    parked: Mutex<Parked>,
    wake: mpsc::Sender<TerminalEvent>,
    durable: Mutex<Option<DurablePermissions>>,
}

pub(crate) struct PermissionSurfaceLease {
    broker: Option<Arc<PermissionBroker>>,
}

impl PermissionSurfaceLease {
    #[cfg(test)]
    pub(crate) fn close(mut self) {
        if let Some(broker) = self.broker.take() {
            broker.release_surface();
        }
    }
}

impl Drop for PermissionSurfaceLease {
    fn drop(&mut self) {
        if let Some(broker) = self.broker.take() {
            broker.release_surface();
        }
    }
}

impl PermissionBroker {
    /// A broker that nudges the render loop through `wake`.
    pub(crate) fn new(wake: mpsc::Sender<TerminalEvent>) -> Self {
        Self {
            parked: Mutex::new(Parked::default()),
            wake,
            durable: Mutex::new(None),
        }
    }

    pub(crate) fn attach_durable(
        &self,
        store: HumanRequestStore,
        goals: Arc<GoalStore>,
        recovery_session_id: impl Into<String>,
    ) {
        *locked(&self.durable) = Some(DurablePermissions {
            store,
            goals,
            recovery_session_id: recovery_session_id.into(),
        });
    }

    /// Lease the single attached TUI surface. Dropping it refuses every pending ask.
    pub(crate) fn surface_lease(self: &Arc<Self>) -> PermissionSurfaceLease {
        let mut parked = locked(&self.parked);
        parked.surfaces = parked.surfaces.saturating_add(1);
        drop(parked);
        PermissionSurfaceLease {
            broker: Some(Arc::clone(self)),
        }
    }

    fn release_surface(&self) {
        let answers = {
            let mut parked = locked(&self.parked);
            if parked.surfaces == 0 {
                return;
            }
            parked.surfaces -= 1;
            if parked.surfaces != 0 {
                return;
            }
            parked.waiting.clear();
            parked.presented.clear();
            parked
                .pending
                .drain()
                .map(|(_, pending)| pending.answer)
                .collect::<Vec<_>>()
        };
        for answer in answers {
            let _delivered = answer.send(ReplyKind::Reject);
        }
    }

    fn close_surfaces(&self) {
        let answers = {
            let mut parked = locked(&self.parked);
            parked.surfaces = 0;
            parked.waiting.clear();
            parked.presented.clear();
            parked
                .pending
                .drain()
                .map(|(_, pending)| pending.answer)
                .collect::<Vec<_>>()
        };
        for answer in answers {
            let _delivered = answer.send(ReplyKind::Reject);
        }
    }

    /// Take the next request that has not been shown yet.
    fn next_request(&self) -> Option<PermissionRequest> {
        let mut parked = locked(&self.parked);
        while let Some(request) = parked.waiting.pop_front() {
            let key = (request.session_id.clone(), request.id.clone());
            if parked.pending.contains_key(&key) {
                parked.presented.insert(key);
                return Some(request);
            }
        }
        let durable = locked(&self.durable).clone()?;
        let request = durable
            .store
            .pending(Some(&durable.recovery_session_id))
            .ok()?
            .into_iter()
            .find_map(|request| {
                if request.kind != HumanRequestKind::Permission {
                    return None;
                }
                let key = (request.session_id.clone(), request.id.clone());
                if parked.pending.contains_key(&key) || parked.presented.contains(&key) {
                    return None;
                }
                serde_json::from_value::<PermissionRequest>(request.payload).ok()
            })?;
        parked
            .presented
            .insert((request.session_id.clone(), request.id.clone()));
        Some(request)
    }

    /// Answer a resolved request, and remember an `always` for this session.
    fn resolve(&self, session_id: &str, request_id: &str, reply: ReplyKind) -> bool {
        let pending = {
            let mut parked = locked(&self.parked);
            let key = (session_id.to_owned(), request_id.to_owned());
            parked.presented.remove(&key);
            parked.pending.remove(&key)
        };
        let durable = locked(&self.durable).clone();
        let recovered = pending.is_none();
        let request = durable
            .as_ref()
            .and_then(|durable| durable.store.get(request_id).ok().flatten());
        if recovered
            && !request.as_ref().is_some_and(|request| {
                request.session_id == session_id
                    && request.kind == HumanRequestKind::Permission
                    && request.state == HumanRequestState::Pending
            })
        {
            return false;
        }
        if pending
            .as_ref()
            .is_some_and(|pending| pending.answer.is_closed())
        {
            if let Some(durable) = durable.as_ref() {
                let _settled = durable.store.resolve(
                    request_id,
                    HumanRequestState::Failed,
                    None,
                    zuno_db::message::now_millis(),
                );
            }
            return false;
        }
        let persisted = durable.as_ref().is_none_or(|durable| {
            let response = json!({"reply": reply});
            let settled = if recovered {
                durable
                    .store
                    .answer_with_input(request_id, response, zuno_db::message::now_millis())
                    .ok()
                    .flatten()
                    .is_some()
            } else {
                durable
                    .store
                    .resolve(
                        request_id,
                        HumanRequestState::Answered,
                        Some(&response),
                        zuno_db::message::now_millis(),
                    )
                    .ok()
                    .flatten()
                    .is_some()
            };
            if settled
                && request
                    .as_ref()
                    .is_some_and(|request| request.goal_id.is_some())
            {
                return durable.goals.resume_for_work(session_id).is_ok();
            }
            settled
        });
        if !persisted {
            if let Some(pending) = pending {
                let _delivered = pending.answer.send(ReplyKind::Reject);
            }
            return false;
        }
        let grant = pending
            .as_ref()
            .and_then(|pending| pending.grant.clone())
            .or_else(|| {
                request.as_ref().and_then(|request| {
                    let request =
                        serde_json::from_value::<PermissionRequest>(request.payload.clone())
                            .ok()?;
                    (!request.always.is_empty()).then_some((
                        request.session_id,
                        request.permission,
                        request.patterns,
                    ))
                })
            });
        let delivered = pending.map_or(recovered, |pending| pending.answer.send(reply).is_ok());
        if delivered
            && reply == ReplyKind::Always
            && let Some(grant) = grant
        {
            locked(&self.parked).standing.push(grant);
        }
        delivered
    }

    fn persist(&self, request: &PermissionRequest) -> Result<(), ToolError> {
        let Some(durable) = locked(&self.durable).clone() else {
            return Ok(());
        };
        let payload = serde_json::to_value(request).map_err(|source| ToolError::Failed {
            tool: String::from("permission"),
            source: Box::new(source),
        })?;
        if durable
            .goals
            .request_permission(
                &request.session_id,
                request.id.clone(),
                payload.clone(),
                request.tool.as_ref().map(|tool| tool.message_id.clone()),
                request.tool.as_ref().map(|tool| tool.call_id.clone()),
            )
            .map_err(|source| ToolError::Failed {
                tool: String::from("permission"),
                source: Box::new(source),
            })?
            .is_some()
        {
            return Ok(());
        }
        durable
            .store
            .create(NewHumanRequest {
                id: request.id.clone(),
                session_id: request.session_id.clone(),
                goal_id: None,
                kind: HumanRequestKind::Permission,
                payload,
                message_id: request.tool.as_ref().map(|tool| tool.message_id.clone()),
                call_id: request.tool.as_ref().map(|tool| tool.call_id.clone()),
                time_created: zuno_db::message::now_millis(),
            })
            .map(|_| ())
            .map_err(|source| ToolError::Failed {
                tool: String::from("permission"),
                source: Box::new(source),
            })
    }
}

#[async_trait]
impl PermissionAsker for PermissionBroker {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        let request_id = format!("per_{}", Uuid::new_v4().simple());
        let (sender, receiver) = oneshot::channel();
        let session_id = origin.session_id().to_owned();
        let grant: Grant = (
            session_id.clone(),
            ask.permission.clone(),
            ask.patterns.clone(),
        );
        let reusable = !ask.manual && !ask.always.is_empty();
        let request = origin.into_request(request_id.clone(), ask);
        {
            let mut parked = locked(&self.parked);
            if parked.surfaces == 0 {
                return Err(ToolError::Denied {
                    tool: tool.to_owned(),
                });
            }
            if reusable && parked.standing.contains(&grant) {
                return Ok(());
            }
            self.persist(&request)?;
            parked.pending.insert(
                (session_id.clone(), request_id.clone()),
                Pending {
                    answer: sender,
                    grant: reusable.then_some(grant),
                },
            );
            parked.waiting.push_back(request);
        }
        // A full terminal channel means at least 64 events are already queued, so the
        // bridge is about to run anyway and will find the request; the nudge only
        // matters when nothing else would wake the loop.
        if matches!(
            self.wake.try_send(TerminalEvent::Wake),
            Err(mpsc::error::TrySendError::Closed(_))
        ) {
            self.close_surfaces();
        }
        let reply = receiver.await.unwrap_or(ReplyKind::Reject);
        if let Some(durable) = locked(&self.durable).clone()
            && let Ok(Some(request)) = durable.store.get(&request_id)
            && request.state == HumanRequestState::Pending
        {
            let response = json!({"reply": reply});
            if durable
                .store
                .resolve(
                    &request_id,
                    HumanRequestState::Answered,
                    Some(&response),
                    zuno_db::message::now_millis(),
                )
                .is_ok()
                && request.goal_id.is_some()
            {
                let _resumed = durable.goals.resume_for_work(&session_id);
            }
        }
        match reply {
            ReplyKind::Once | ReplyKind::Always => Ok(()),
            ReplyKind::Reject => Err(ToolError::Denied {
                tool: tool.to_owned(),
            }),
        }
    }
}

/// An asker that approves without asking, for `--auto`.
///
/// The dangerous half of upstream's flag, and it is deliberately its own type rather
/// than a boolean inside the broker: a surface either has a human in the loop or it
/// does not, and making that a choice of collaborator means the prompt path cannot be
/// silently bypassed by a flag that flipped somewhere far away.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AutoApproval;

#[async_trait]
impl PermissionAsker for AutoApproval {
    async fn ask(
        &self,
        _origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        if ask.manual {
            Err(ToolError::Denied {
                tool: tool.to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

/// The component that carries parked requests to the dialog stack and back.
pub(crate) struct PermissionBridge {
    context: ViewContext,
    broker: Arc<PermissionBroker>,
    host: DialogHost,
    question: Option<QuestionBridge>,
    _surface: PermissionSurfaceLease,
}

impl PermissionBridge {
    /// Mount `host` behind a bridge to `broker`.
    pub(crate) fn new(
        context: ViewContext,
        broker: Arc<PermissionBroker>,
        host: DialogHost,
    ) -> Self {
        let surface = broker.surface_lease();
        Self {
            context,
            broker,
            host,
            question: None,
            _surface: surface,
        }
    }

    pub(crate) fn with_question(mut self, question: QuestionBridge) -> Self {
        self.question = Some(question);
        self
    }

    /// Open a prompt for every request waiting, and deliver every decision made.
    ///
    /// Called on every event rather than only on [`TerminalEvent::Wake`] so that a
    /// dropped nudge cannot leave a turn waiting forever on an answer nobody was
    /// asked for.
    fn pump(&mut self) -> EventResult {
        let mut result = EventResult::IGNORED;
        while let Some(request) = self.broker.next_request() {
            let arguments = request_arguments(&request);
            self.host.open(Box::new(PermissionPrompt::new(
                self.context.clone(),
                request,
                &arguments,
            )));
            result = EventResult::REDRAW;
        }
        for (dialog, outcome) in self.host.drain_outcomes() {
            match outcome {
                DialogOutcome::Permission(decision) => {
                    self.broker
                        .resolve(&decision.session_id, &decision.request_id, decision.reply);
                    result = EventResult::REDRAW;
                }
                DialogOutcome::Question(answers) => {
                    if let Some(question) = self.question.as_mut() {
                        result = result.merge(question.resolve(answers));
                    }
                }
                DialogOutcome::Cancelled if dialog == zuno_tui::views::question::DIALOG_ID => {
                    if let Some(question) = self.question.as_mut() {
                        result = result.merge(question.cancel());
                    }
                }
                _ => {}
            }
        }
        if let Some(question) = self.question.as_mut() {
            result = result.merge(question.open_next(&mut self.host));
        }
        result
    }
}

fn request_arguments(request: &PermissionRequest) -> serde_json::Value {
    request
        .metadata
        .get("arguments")
        .filter(|arguments| arguments.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

impl Component for PermissionBridge {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.host.render(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        // A wake is forwarded like every other non-key event. It used to be swallowed
        // here on the grounds that "a wake carries no state of its own, so there is
        // nothing below to hand it to" — which was true while this broker was the only
        // thing that nudged the loop. It stopped being true the moment a second producer
        // existed: the language-server probe queues a report and then nudges, and a wake
        // absorbed here meant the report was never drained. A completed turn is the last
        // event the loop will see, so "it will be picked up on the next event" is not a
        // fallback — it is never.
        self.pump().merge(self.host.handle_event(event))
    }
}

impl ActionComponent for PermissionBridge {
    fn handle_action(
        &mut self,
        action: &'static Definition,
        event: &zuno_tui::crossterm::event::KeyEvent,
    ) -> EventResult {
        let result = self.host.handle_action(action, event);
        result.merge(self.pump())
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        self.host.focused_scopes()
    }

    fn pending_changed(&mut self, pending: &PendingPrefix) -> EventResult {
        self.host.pending_changed(pending)
    }
}

#[cfg(test)]
#[path = "tui_permission_tests.rs"]
mod tests;
