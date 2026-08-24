//! Process-local permission and question requests for HTTP-driven turns.
//!
//! A turn parks on a oneshot receiver while the corresponding request remains in
//! this broker. HTTP list routes expose that pending state. Reply routes validate
//! their untrusted body before checking request ownership to match the upstream API;
//! when validation fails, they claim only a matching `(session_id, request_id)` for
//! fail-closed cleanup. A claimed request owns its answer sender, and dropping the
//! claim sends a failed terminal decision. Consequently a malformed or disconnected
//! reply can never authorize a tool by accident or consume another session's request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use zuno_permission::ReplyKind;

use crate::{EventService, EventStreamError, NewEvent};

pub type QuestionAnswers = Vec<Vec<String>>;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub save: Vec<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<RequestSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestSource {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionToolCall {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionDecision {
    Answered(QuestionAnswers),
    Cancelled,
    Expired,
    Failed,
}

struct PendingPermission {
    request: PermissionRequest,
    answer: oneshot::Sender<ReplyKind>,
}

struct PendingQuestion {
    request: QuestionRequest,
    answer: oneshot::Sender<QuestionDecision>,
}

#[derive(Default)]
struct Pending {
    permissions: HashMap<String, PendingPermission>,
    questions: HashMap<String, PendingQuestion>,
    standing: Vec<(String, Vec<String>)>,
    observers: HashMap<String, usize>,
}

#[derive(Clone)]
pub struct RequestBroker {
    pending: Arc<Mutex<Pending>>,
    events: Option<EventService>,
    request_timeout: Duration,
}

impl Default for RequestBroker {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(Pending::default())),
            events: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

impl std::fmt::Debug for RequestBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = self.lock();
        formatter
            .debug_struct("RequestBroker")
            .field("pending_permissions", &pending.permissions.len())
            .field("pending_questions", &pending.questions.len())
            .field("standing_permissions", &pending.standing.len())
            .field("observed_sessions", &pending.observers.len())
            .field("publishes_events", &self.events.is_some())
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl RequestBroker {
    #[must_use]
    pub fn with_events(events: EventService) -> Self {
        Self {
            events: Some(events),
            ..Self::default()
        }
    }

    pub async fn ask_permission(&self, request: PermissionRequest) -> ReplyKind {
        let grant = (request.action.clone(), request.resources.clone());
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.lock();
            if pending.standing.contains(&grant) {
                return ReplyKind::Once;
            }
            pending.permissions.insert(
                request.id.clone(),
                PendingPermission {
                    request: request.clone(),
                    answer: sender,
                },
            );
        }
        self.spawn_permission_watchdog(&request);
        if let Err(error) = self.publish("permission.v2.asked", &request).await {
            eprintln!(
                "failed to publish HTTP permission request `{}`: {error}",
                request.id
            );
            self.reject_permission(&request.session_id, &request.id);
        }
        receiver.await.unwrap_or(ReplyKind::Reject)
    }

    pub async fn ask_question(&self, request: QuestionRequest) -> QuestionDecision {
        let (sender, receiver) = oneshot::channel();
        {
            self.lock().questions.insert(
                request.id.clone(),
                PendingQuestion {
                    request: request.clone(),
                    answer: sender,
                },
            );
        }
        self.spawn_question_watchdog(&request);
        if let Err(error) = self.publish("question.v2.asked", &request).await {
            eprintln!(
                "failed to publish HTTP question request `{}`: {error}",
                request.id
            );
            self.finish_question(&request.session_id, &request.id, QuestionDecision::Failed);
        }
        receiver.await.unwrap_or(QuestionDecision::Failed)
    }

    #[must_use]
    pub fn permissions(&self, session_id: Option<&str>) -> Vec<PermissionRequest> {
        let mut requests = self
            .lock()
            .permissions
            .values()
            .filter(|pending| {
                session_id.is_none_or(|session_id| pending.request.session_id == session_id)
            })
            .map(|pending| pending.request.clone())
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id.cmp(&right.id));
        requests
    }

    #[must_use]
    pub fn questions(&self, session_id: Option<&str>) -> Vec<QuestionRequest> {
        let mut requests = self
            .lock()
            .questions
            .values()
            .filter(|pending| {
                session_id.is_none_or(|session_id| pending.request.session_id == session_id)
            })
            .map(|pending| pending.request.clone())
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id.cmp(&right.id));
        requests
    }

    pub(crate) fn observe_session(&self, session_id: &str) -> SessionRequestObserver {
        *self
            .lock()
            .observers
            .entry(session_id.to_owned())
            .or_default() += 1;
        SessionRequestObserver {
            session_id: session_id.to_owned(),
            pending: Arc::downgrade(&self.pending),
        }
    }

    #[must_use]
    pub fn claim_permission(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Option<PermissionResolution> {
        let mut pending = self.lock();
        let request = pending.permissions.get(request_id)?;
        if request.request.session_id != session_id {
            return None;
        }
        let pending = pending
            .permissions
            .remove(request_id)
            .expect("the request was checked while holding the same lock");
        Some(PermissionResolution {
            answer: Some(pending.answer),
            grant: (pending.request.action, pending.request.resources),
            standing: Arc::clone(&self.pending),
        })
    }

    #[must_use]
    pub fn claim_question(&self, session_id: &str, request_id: &str) -> Option<QuestionResolution> {
        let mut pending = self.lock();
        let request = pending.questions.get(request_id)?;
        if request.request.session_id != session_id {
            return None;
        }
        let pending = pending
            .questions
            .remove(request_id)
            .expect("the request was checked while holding the same lock");
        Some(QuestionResolution {
            answer: Some(pending.answer),
        })
    }

    pub async fn publish_permission_reply(
        &self,
        session_id: &str,
        request_id: &str,
        reply: ReplyKind,
    ) -> Result<(), EventStreamError> {
        self.publish(
            "permission.v2.replied",
            &json!({
                "sessionID": session_id,
                "requestID": request_id,
                "reply": reply,
            }),
        )
        .await
    }

    pub async fn publish_question_reply(
        &self,
        session_id: &str,
        request_id: &str,
        decision: &QuestionDecision,
    ) -> Result<(), EventStreamError> {
        let (event_type, properties) = match decision {
            QuestionDecision::Answered(answers) => (
                "question.v2.replied",
                json!({
                    "sessionID": session_id,
                    "requestID": request_id,
                    "answers": answers,
                }),
            ),
            QuestionDecision::Cancelled => (
                "question.v2.cancelled",
                json!({
                    "sessionID": session_id,
                    "requestID": request_id,
                }),
            ),
            QuestionDecision::Expired => (
                "question.v2.expired",
                json!({
                    "sessionID": session_id,
                    "requestID": request_id,
                }),
            ),
            QuestionDecision::Failed => (
                "question.v2.failed",
                json!({
                    "sessionID": session_id,
                    "requestID": request_id,
                }),
            ),
        };
        self.publish(event_type, &properties).await
    }

    fn reject_permission(&self, session_id: &str, request_id: &str) {
        reject_permission(&self.pending, session_id, request_id);
    }

    fn finish_question(&self, session_id: &str, request_id: &str, decision: QuestionDecision) {
        finish_question(&self.pending, session_id, request_id, decision);
    }

    fn spawn_permission_watchdog(&self, request: &PermissionRequest) {
        let pending = Arc::downgrade(&self.pending);
        let session_id = request.session_id.clone();
        let request_id = request.id.clone();
        let timeout = self.request_timeout;
        let _watchdog = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(pending) = pending.upgrade() {
                reject_permission(&pending, &session_id, &request_id);
            }
        });
    }

    fn spawn_question_watchdog(&self, request: &QuestionRequest) {
        let pending = Arc::downgrade(&self.pending);
        let session_id = request.session_id.clone();
        let request_id = request.id.clone();
        let timeout = self.request_timeout;
        let _watchdog = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(pending) = pending.upgrade() {
                finish_question(
                    &pending,
                    &session_id,
                    &request_id,
                    QuestionDecision::Expired,
                );
            }
        });
    }

    async fn publish<T: Serialize>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(), EventStreamError> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        let value = serde_json::to_value(payload)?;
        let properties = value
            .as_object()
            .cloned()
            .expect("request and reply event payloads are objects");
        let session_id = properties
            .get("sessionID")
            .and_then(Value::as_str)
            .expect("request and reply events carry sessionID")
            .to_owned();
        events
            .publish(&session_id, NewEvent::new(event_type, properties)?)
            .await?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct SessionRequestObserver {
    session_id: String,
    pending: Weak<Mutex<Pending>>,
}

impl Drop for SessionRequestObserver {
    fn drop(&mut self) {
        let Some(pending) = self.pending.upgrade() else {
            return;
        };
        let (permissions, questions) = {
            let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
            match pending.observers.get_mut(&self.session_id) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    return;
                }
                Some(_) => {
                    pending.observers.remove(&self.session_id);
                }
                None => return,
            }
            take_session_requests(&mut pending, &self.session_id)
        };
        for permission in permissions {
            let _delivered = permission.answer.send(ReplyKind::Reject);
        }
        for question in questions {
            let _delivered = question.answer.send(QuestionDecision::Cancelled);
        }
    }
}

fn reject_permission(pending: &Mutex<Pending>, session_id: &str, request_id: &str) {
    let request = {
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(request) = pending.permissions.get(request_id) else {
            return;
        };
        if request.request.session_id != session_id {
            return;
        }
        pending
            .permissions
            .remove(request_id)
            .expect("the request was checked while holding the same lock")
    };
    let _delivered = request.answer.send(ReplyKind::Reject);
}

fn finish_question(
    pending: &Mutex<Pending>,
    session_id: &str,
    request_id: &str,
    decision: QuestionDecision,
) {
    let request = {
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(request) = pending.questions.get(request_id) else {
            return;
        };
        if request.request.session_id != session_id {
            return;
        }
        pending
            .questions
            .remove(request_id)
            .expect("the request was checked while holding the same lock")
    };
    let _delivered = request.answer.send(decision);
}

fn take_session_requests(
    pending: &mut Pending,
    session_id: &str,
) -> (Vec<PendingPermission>, Vec<PendingQuestion>) {
    let permission_ids = pending
        .permissions
        .iter()
        .filter(|(_, request)| request.request.session_id == session_id)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let question_ids = pending
        .questions
        .iter()
        .filter(|(_, request)| request.request.session_id == session_id)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let permissions = permission_ids
        .into_iter()
        .filter_map(|id| pending.permissions.remove(&id))
        .collect();
    let questions = question_ids
        .into_iter()
        .filter_map(|id| pending.questions.remove(&id))
        .collect();
    (permissions, questions)
}

pub struct PermissionResolution {
    answer: Option<oneshot::Sender<ReplyKind>>,
    grant: (String, Vec<String>),
    standing: Arc<Mutex<Pending>>,
}

impl PermissionResolution {
    pub fn resolve(mut self, reply: ReplyKind) -> bool {
        if reply == ReplyKind::Always {
            self.standing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .standing
                .push(self.grant.clone());
        }
        self.answer
            .take()
            .is_some_and(|answer| answer.send(reply).is_ok())
    }
}

impl Drop for PermissionResolution {
    fn drop(&mut self) {
        if let Some(answer) = self.answer.take() {
            let _delivered = answer.send(ReplyKind::Reject);
        }
    }
}

pub struct QuestionResolution {
    answer: Option<oneshot::Sender<QuestionDecision>>,
}

impl QuestionResolution {
    pub fn resolve(mut self, decision: QuestionDecision) -> bool {
        self.answer
            .take()
            .is_some_and(|answer| answer.send(decision).is_ok())
    }
}

impl Drop for QuestionResolution {
    fn drop(&mut self) {
        if let Some(answer) = self.answer.take() {
            let _delivered = answer.send(QuestionDecision::Failed);
        }
    }
}
