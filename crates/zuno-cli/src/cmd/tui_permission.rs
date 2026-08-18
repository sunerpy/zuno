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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zuno_error::ToolError;
use zuno_permission::{PermissionRequest, ReplyKind};
use zuno_tool::{PermissionAsk, PermissionAsker};
use zuno_tui::app::{AppEvent, Component, EventResult, TerminalEvent};
use zuno_tui::keybind::{ActionComponent, Chord, Definition};
use zuno_tui::ratatui::Frame;
use zuno_tui::ratatui::layout::Rect;
use zuno_tui::views::ViewContext;
use zuno_tui::views::dialog::{DialogHost, DialogOutcome};
use zuno_tui::views::permission::PermissionPrompt;

use super::tui_question::QuestionBridge;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A `(permission, patterns)` pair, which is what a rule matches on.
type Grant = (String, Vec<String>);

struct Pending {
    answer: oneshot::Sender<ReplyKind>,
    grant: Grant,
}

#[derive(Default)]
struct Parked {
    waiting: VecDeque<PermissionRequest>,
    pending: HashMap<String, Pending>,
    standing: Vec<Grant>,
}

/// The asker a surface with a human attached hands to the dispatcher.
pub(crate) struct PermissionBroker {
    parked: Mutex<Parked>,
    wake: mpsc::Sender<TerminalEvent>,
    session_id: OnceLock<String>,
}

impl PermissionBroker {
    /// A broker that nudges the render loop through `wake`.
    pub(crate) fn new(wake: mpsc::Sender<TerminalEvent>) -> Self {
        Self {
            parked: Mutex::new(Parked::default()),
            wake,
            session_id: OnceLock::new(),
        }
    }

    /// Record the session every request belongs to, once it is known.
    ///
    /// Separate from construction because the session is resolved by the same call
    /// that needs the broker: the dispatcher is built with the asker already in hand.
    pub(crate) fn bind_session(&self, session_id: &str) {
        let _first = self.session_id.set(session_id.to_owned());
    }

    /// Take the next request that has not been shown yet.
    fn next_request(&self) -> Option<PermissionRequest> {
        locked(&self.parked).waiting.pop_front()
    }

    /// Answer a resolved request, and remember an `always` for the process.
    fn resolve(&self, request_id: &str, reply: ReplyKind) {
        let answer = {
            let mut parked = locked(&self.parked);
            let Some(pending) = parked.pending.remove(request_id) else {
                return;
            };
            if reply == ReplyKind::Always {
                parked.standing.push(pending.grant);
            }
            pending.answer
        };
        let _delivered = answer.send(reply);
    }
}

#[async_trait]
impl PermissionAsker for PermissionBroker {
    async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        let request_id = format!("per_{}", Uuid::new_v4().simple());
        let (sender, receiver) = oneshot::channel();
        {
            let grant: Grant = (ask.permission.clone(), ask.patterns.clone());
            let mut parked = locked(&self.parked);
            if parked.standing.contains(&grant) {
                return Ok(());
            }
            parked.pending.insert(
                request_id.clone(),
                Pending {
                    answer: sender,
                    grant,
                },
            );
            parked.waiting.push_back(ask.into_request(
                request_id,
                self.session_id.get().cloned().unwrap_or_default(),
                None,
            ));
        }
        // A full terminal channel means at least 64 events are already queued, so the
        // bridge is about to run anyway and will find the request; the nudge only
        // matters when nothing else would wake the loop.
        let _nudged = self.wake.try_send(TerminalEvent::Wake);
        match receiver.await {
            Ok(ReplyKind::Once | ReplyKind::Always) => Ok(()),
            Ok(ReplyKind::Reject) | Err(_) => Err(ToolError::Denied {
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
    async fn ask(&self, _tool: &str, _ask: PermissionAsk) -> Result<(), ToolError> {
        Ok(())
    }
}

/// The component that carries parked requests to the dialog stack and back.
pub(crate) struct PermissionBridge {
    context: ViewContext,
    broker: Arc<PermissionBroker>,
    host: DialogHost,
    question: Option<QuestionBridge>,
}

impl PermissionBridge {
    /// Mount `host` behind a bridge to `broker`.
    pub(crate) fn new(
        context: ViewContext,
        broker: Arc<PermissionBroker>,
        host: DialogHost,
    ) -> Self {
        Self {
            context,
            broker,
            host,
            question: None,
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
        for (_dialog, outcome) in self.host.drain_outcomes() {
            match outcome {
                DialogOutcome::Permission(decision) => {
                    self.broker.resolve(&decision.request_id, decision.reply);
                    result = EventResult::REDRAW;
                }
                DialogOutcome::Question(answers) => {
                    if let Some(question) = self.question.as_mut() {
                        result = result.merge(question.resolve(answers));
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

    fn pending_changed(&mut self, pending: &[Chord]) -> EventResult {
        self.host.pending_changed(pending)
    }
}

#[cfg(test)]
#[path = "tui_permission_tests.rs"]
mod tests;
