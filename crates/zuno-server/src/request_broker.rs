//! Process-local permission and question requests for HTTP-driven turns.
//!
//! A turn parks on a oneshot receiver while the corresponding request remains in
//! this broker. HTTP list routes expose that pending state. Reply routes validate
//! their untrusted body before checking request ownership to match the upstream API;
//! when validation fails, they claim only a matching `(session_id, request_id)` for
//! fail-closed cleanup. A claimed request owns its answer sender, and dropping the
//! claim sends a failed terminal decision. Consequently a malformed or disconnected
//! reply can never authorize a tool by accident or consume another session's request.
//!
//! A saved `always` reply installs a process-local standing authorization keyed by the
//! granting session, so it can only pre-approve later calls in that same session, and
//! only for an ask that offered something to save. Archiving or deleting the session
//! withdraws it; an SSE client disconnecting does not, because a stream is not the
//! session.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use zuno_db::human_request::{
    HumanRequest, HumanRequestKind, HumanRequestState, HumanRequestStore, NewHumanRequest,
};
use zuno_permission::ReplyKind;

use crate::{EventService, EventStreamError, NewEvent};

pub type QuestionAnswers = Vec<Vec<String>>;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestSource {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// A standing authorization: the session that granted it, plus what it covers.
///
/// The session id is part of the key because an `always` reply is one session's
/// decision. Without it, a grant taken in one session would silently pre-approve a
/// matching call in every other session this process serves, including sessions
/// started later and sessions the replying client never saw.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StandingGrant {
    session_id: String,
    action: String,
    resources: Vec<String>,
}

/// The grant an ask could install, or `None` when the ask is single-use.
///
/// A request only offers a standing authorization when it carries something to save.
/// `ServerPermissionAsker` clears `save` for a manual ask, so a manual ask can
/// neither install a grant nor be satisfied by one.
fn reusable_grant(request: &PermissionRequest) -> Option<StandingGrant> {
    (!request.save.is_empty()).then(|| StandingGrant {
        session_id: request.session_id.clone(),
        action: request.action.clone(),
        resources: request.resources.clone(),
    })
}

struct PendingPermission {
    request: PermissionRequest,
    answer: oneshot::Sender<ReplyKind>,
    grant: Option<StandingGrant>,
}

struct PendingQuestion {
    request: QuestionRequest,
    answer: oneshot::Sender<QuestionDecision>,
}

#[derive(Default)]
struct Pending {
    permissions: HashMap<String, PendingPermission>,
    questions: HashMap<String, PendingQuestion>,
    standing: BTreeSet<StandingGrant>,
    observers: HashMap<String, usize>,
}

#[derive(Clone)]
pub struct RequestBroker {
    pending: Arc<Mutex<Pending>>,
    events: Option<EventService>,
    durable: Option<HumanRequestStore>,
    goals: Option<Arc<zuno_goal::GoalStore>>,
    request_timeout: Duration,
}

impl Default for RequestBroker {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(Pending::default())),
            events: None,
            durable: None,
            goals: None,
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
            .field("persists_requests", &self.durable.is_some())
            .field("coordinates_goals", &self.goals.is_some())
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

    #[must_use]
    pub fn with_store(mut self, durable: HumanRequestStore) -> Self {
        self.durable = Some(durable);
        self
    }

    #[must_use]
    pub fn with_goal_store(mut self, goals: Arc<zuno_goal::GoalStore>) -> Self {
        self.goals = Some(goals);
        self
    }

    pub async fn ask_permission(&self, request: PermissionRequest) -> ReplyKind {
        let grant = reusable_grant(&request);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.lock();
            if let Some(grant) = &grant
                && pending.standing.contains(grant)
            {
                return ReplyKind::Once;
            }
            if self.persist_permission(&request).is_err() {
                return ReplyKind::Reject;
            }
            pending.permissions.insert(
                request.id.clone(),
                PendingPermission {
                    request: request.clone(),
                    answer: sender,
                    grant,
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
        let reply = receiver.await.unwrap_or(ReplyKind::Reject);
        if let Some(store) = &self.durable {
            let goal_owned = store
                .get(&request.id)
                .ok()
                .flatten()
                .is_some_and(|request| request.goal_id.is_some());
            let response = json!({"reply": reply});
            let _settled = store.resolve(
                &request.id,
                HumanRequestState::Answered,
                Some(&response),
                zuno_db::message::now_millis(),
            );
            if goal_owned && let Some(goals) = &self.goals {
                let _resumed = goals.resume_for_work(&request.session_id);
            }
        }
        reply
    }

    pub async fn ask_question(&self, request: QuestionRequest) -> QuestionDecision {
        if self.persist_question(&request).is_err() {
            return QuestionDecision::Failed;
        }
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
        let decision = receiver.await.unwrap_or(QuestionDecision::Failed);
        if let Some(store) = &self.durable {
            let goal_owned = store
                .get(&request.id)
                .ok()
                .flatten()
                .is_some_and(|request| request.goal_id.is_some());
            let (state, response) = question_state_and_response(&decision);
            let _settled = store.resolve(
                &request.id,
                state,
                response.as_ref(),
                zuno_db::message::now_millis(),
            );
            if goal_owned
                && state == HumanRequestState::Answered
                && let Some(goals) = &self.goals
            {
                let _resumed = goals.resume_for_work(&request.session_id);
            }
        }
        decision
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
        if let Some(store) = &self.durable
            && let Ok(pending) = store.pending(session_id)
        {
            for request in pending {
                if request.kind == HumanRequestKind::Permission
                    && !requests.iter().any(|existing| existing.id == request.id)
                    && let Some(projected) = permission_from_durable(&request)
                {
                    requests.push(projected);
                }
            }
        }
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
        if let Some(store) = &self.durable
            && let Ok(pending) = store.pending(session_id)
        {
            for request in pending {
                if request.kind == HumanRequestKind::Input
                    && !requests.iter().any(|existing| existing.id == request.id)
                    && let Some(projected) = question_from_durable(&request)
                {
                    requests.push(projected);
                }
            }
        }
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

    /// Withdraws every standing authorization granted by these sessions.
    ///
    /// A standing `always` is one session's decision, so it lives exactly as long as
    /// that session: archiving or deleting the session drops its grants, and a later
    /// session that reuses the id inherits nothing.
    pub fn forget_session_grants(&self, session_ids: &[String]) {
        if session_ids.is_empty() {
            return;
        }
        self.lock()
            .standing
            .retain(|grant| !session_ids.contains(&grant.session_id));
    }

    #[must_use]
    pub fn claim_permission(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Option<PermissionResolution> {
        let live_resumes_goal = self
            .durable
            .as_ref()
            .and_then(|store| store.get(request_id).ok().flatten())
            .is_some_and(|request| {
                request.session_id == session_id
                    && request.kind == HumanRequestKind::Permission
                    && request.goal_id.is_some()
            });
        let live = {
            let mut pending = self.lock();
            match pending.permissions.get(request_id) {
                Some(request) if request.request.session_id == session_id => pending
                    .permissions
                    .remove(request_id)
                    .map(|pending| (pending.answer, pending.grant)),
                Some(_) | None => None,
            }
        };
        if let Some((answer, grant)) = live {
            return Some(PermissionResolution {
                answer: Some(answer),
                grant,
                standing: Arc::clone(&self.pending),
                durable: self.durable.clone(),
                goals: self.goals.clone(),
                session_id: session_id.to_owned(),
                request_id: request_id.to_owned(),
                resume_goal: live_resumes_goal,
            });
        }
        let request = self
            .durable
            .as_ref()?
            .get(request_id)
            .ok()
            .flatten()
            .filter(|request| {
                request.session_id == session_id
                    && request.kind == HumanRequestKind::Permission
                    && request.state == HumanRequestState::Pending
            })?;
        let resume_goal = request.goal_id.is_some();
        let projected = permission_from_durable(&request)?;
        Some(PermissionResolution {
            answer: None,
            grant: reusable_grant(&projected),
            standing: Arc::clone(&self.pending),
            durable: self.durable.clone(),
            goals: self.goals.clone(),
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            resume_goal,
        })
    }

    #[must_use]
    pub fn claim_question(&self, session_id: &str, request_id: &str) -> Option<QuestionResolution> {
        let live_resumes_goal = self
            .durable
            .as_ref()
            .and_then(|store| store.get(request_id).ok().flatten())
            .is_some_and(|request| {
                request.session_id == session_id
                    && request.kind == HumanRequestKind::Input
                    && request.goal_id.is_some()
            });
        let live = {
            let mut pending = self.lock();
            match pending.questions.get(request_id) {
                Some(request) if request.request.session_id == session_id => {
                    pending.questions.remove(request_id)
                }
                Some(_) | None => None,
            }
        };
        if let Some(pending) = live {
            return Some(QuestionResolution {
                answer: Some(pending.answer),
                durable: self.durable.clone(),
                goals: self.goals.clone(),
                session_id: session_id.to_owned(),
                request_id: request_id.to_owned(),
                resume_goal: live_resumes_goal,
            });
        }
        let request = self
            .durable
            .as_ref()?
            .get(request_id)
            .ok()
            .flatten()
            .filter(|request| {
                request.session_id == session_id
                    && request.kind == HumanRequestKind::Input
                    && request.state == HumanRequestState::Pending
            })?;
        Some(QuestionResolution {
            answer: None,
            durable: self.durable.clone(),
            goals: self.goals.clone(),
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            resume_goal: request.goal_id.is_some(),
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

    fn persist_permission(&self, request: &PermissionRequest) -> Result<(), zuno_error::DbError> {
        let Some(store) = &self.durable else {
            return Ok(());
        };
        let payload = serde_json::to_value(permission_payload(request)).map_err(|source| {
            zuno_error::DbError::Decode {
                table: "human_request".to_owned(),
                source,
            }
        })?;
        if let Some(goals) = &self.goals
            && goals
                .request_permission(
                    &request.session_id,
                    request.id.clone(),
                    payload.clone(),
                    request
                        .source
                        .as_ref()
                        .map(|source| source.message_id.clone()),
                    request.source.as_ref().map(|source| source.call_id.clone()),
                )
                .map_err(goal_db_error)?
                .is_some()
        {
            return Ok(());
        }
        store.create(NewHumanRequest {
            id: request.id.clone(),
            session_id: request.session_id.clone(),
            goal_id: None,
            kind: HumanRequestKind::Permission,
            payload,
            message_id: request
                .source
                .as_ref()
                .map(|source| source.message_id.clone()),
            call_id: request.source.as_ref().map(|source| source.call_id.clone()),
            time_created: zuno_db::message::now_millis(),
        })?;
        Ok(())
    }

    fn persist_question(&self, request: &QuestionRequest) -> Result<(), zuno_error::DbError> {
        let Some(store) = &self.durable else {
            return Ok(());
        };
        store.create(NewHumanRequest {
            id: request.id.clone(),
            session_id: request.session_id.clone(),
            goal_id: None,
            kind: HumanRequestKind::Input,
            payload: serde_json::json!({
                "source": "question",
                "questions": request.questions,
                "tool": request.tool,
            }),
            message_id: request.tool.as_ref().map(|tool| tool.message_id.clone()),
            call_id: request.tool.as_ref().map(|tool| tool.call_id.clone()),
            time_created: zuno_db::message::now_millis(),
        })?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn goal_db_error(error: zuno_goal::GoalError) -> zuno_error::DbError {
    match error {
        zuno_goal::GoalError::Db(error) => error,
        other => zuno_error::DbError::Query {
            source: Box::new(std::io::Error::other(other.to_string())),
        },
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
    grant: Option<StandingGrant>,
    standing: Arc<Mutex<Pending>>,
    durable: Option<HumanRequestStore>,
    goals: Option<Arc<zuno_goal::GoalStore>>,
    session_id: String,
    request_id: String,
    resume_goal: bool,
}

impl PermissionResolution {
    pub fn resolve(mut self, reply: ReplyKind) -> bool {
        let live = self.answer.is_some();
        if let Some(store) = &self.durable {
            let response = json!({"reply": reply});
            let persisted = if live {
                store
                    .resolve(
                        &self.request_id,
                        HumanRequestState::Answered,
                        Some(&response),
                        zuno_db::message::now_millis(),
                    )
                    .ok()
                    .flatten()
                    .is_some()
            } else {
                store
                    .answer_with_input(&self.request_id, response, zuno_db::message::now_millis())
                    .ok()
                    .flatten()
                    .is_some()
            };
            if !persisted {
                return false;
            }
            self.durable = None;
        }
        if self.resume_goal
            && self
                .goals
                .as_ref()
                .is_some_and(|goals| goals.resume_for_work(&self.session_id).is_err())
        {
            return false;
        }
        self.goals = None;
        let delivered = self
            .answer
            .take()
            .map_or(!live, |answer| answer.send(reply).is_ok());
        // A standing authorization is installed only once the reply it came from has
        // actually landed. A reply that failed to persist, or whose asker is gone,
        // leaves the call denied, so it must not leave an `always` behind that would
        // auto-approve the next matching call.
        if delivered
            && reply == ReplyKind::Always
            && let Some(grant) = self.grant.take()
        {
            self.standing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .standing
                .insert(grant);
        }
        delivered
    }
}

impl Drop for PermissionResolution {
    fn drop(&mut self) {
        if let Some(answer) = self.answer.take() {
            let _delivered = answer.send(ReplyKind::Reject);
        }
        if let Some(store) = &self.durable {
            let _settled = store.resolve(
                &self.request_id,
                HumanRequestState::Failed,
                None,
                zuno_db::message::now_millis(),
            );
        }
    }
}

pub struct QuestionResolution {
    answer: Option<oneshot::Sender<QuestionDecision>>,
    durable: Option<HumanRequestStore>,
    goals: Option<Arc<zuno_goal::GoalStore>>,
    session_id: String,
    request_id: String,
    resume_goal: bool,
}

impl QuestionResolution {
    pub fn resolve(mut self, decision: QuestionDecision) -> bool {
        let live = self.answer.is_some();
        if let Some(store) = &self.durable {
            let (state, response) = question_state_and_response(&decision);
            let persisted = if live || state != HumanRequestState::Answered {
                store
                    .resolve(
                        &self.request_id,
                        state,
                        response.as_ref(),
                        zuno_db::message::now_millis(),
                    )
                    .ok()
                    .flatten()
                    .is_some()
            } else {
                store
                    .answer_with_input(
                        &self.request_id,
                        response.expect("answered questions carry a response"),
                        zuno_db::message::now_millis(),
                    )
                    .ok()
                    .flatten()
                    .is_some()
            };
            if !persisted {
                return false;
            }
            self.durable = None;
        }
        if self.resume_goal
            && self
                .goals
                .as_ref()
                .is_some_and(|goals| goals.resume_for_work(&self.session_id).is_err())
        {
            return false;
        }
        self.goals = None;
        self.answer
            .take()
            .map_or(!live, |answer| answer.send(decision).is_ok())
    }
}

impl Drop for QuestionResolution {
    fn drop(&mut self) {
        if let Some(answer) = self.answer.take() {
            let _delivered = answer.send(QuestionDecision::Failed);
        }
        if let Some(store) = &self.durable {
            let _settled = store.resolve(
                &self.request_id,
                HumanRequestState::Failed,
                None,
                zuno_db::message::now_millis(),
            );
        }
    }
}

fn question_state_and_response(decision: &QuestionDecision) -> (HumanRequestState, Option<Value>) {
    match decision {
        QuestionDecision::Answered(answers) => (
            HumanRequestState::Answered,
            Some(json!({"answers": answers})),
        ),
        QuestionDecision::Cancelled => (HumanRequestState::Cancelled, None),
        QuestionDecision::Expired => (HumanRequestState::Expired, None),
        QuestionDecision::Failed => (HumanRequestState::Failed, None),
    }
}

fn question_from_durable(request: &HumanRequest) -> Option<QuestionRequest> {
    let questions = request.payload.get("questions")?.as_array()?.clone();
    let tool = match (&request.message_id, &request.call_id) {
        (Some(message_id), Some(call_id)) => Some(QuestionToolCall {
            message_id: message_id.clone(),
            call_id: call_id.clone(),
        }),
        _ => None,
    };
    Some(QuestionRequest {
        id: request.id.clone(),
        session_id: request.session_id.clone(),
        questions,
        tool,
    })
}

fn permission_from_durable(request: &HumanRequest) -> Option<PermissionRequest> {
    let payload =
        serde_json::from_value::<zuno_permission::PermissionRequest>(request.payload.clone())
            .ok()?;
    let source = payload.tool.map(|tool| RequestSource {
        kind: "tool",
        message_id: tool.message_id,
        call_id: tool.call_id,
    });
    Some(PermissionRequest {
        id: payload.id,
        session_id: payload.session_id,
        action: payload.permission,
        resources: payload.patterns,
        save: payload.always,
        metadata: payload.metadata,
        source,
    })
}

fn permission_payload(request: &PermissionRequest) -> zuno_permission::PermissionRequest {
    zuno_permission::PermissionRequest {
        id: request.id.clone(),
        session_id: request.session_id.clone(),
        permission: request.action.clone(),
        patterns: request.resources.clone(),
        metadata: request.metadata.clone(),
        always: request.save.clone(),
        tool: request
            .source
            .as_ref()
            .map(|source| zuno_permission::ToolCall {
                message_id: source.message_id.clone(),
                call_id: source.call_id.clone(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn durable_broker() -> (
        tempfile::TempDir,
        Arc<zuno_db::Pool>,
        Arc<zuno_goal::GoalStore>,
        RequestBroker,
    ) {
        let spill = tempfile::tempdir().expect("spill directory");
        let pool =
            Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.get().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
                  time_updated,time_initialized,sandboxes,commands) \
                 VALUES ('prj','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
                [],
            )
            .expect("project");
        connection
            .execute(
                "INSERT INTO session \
                 (id,project_id,slug,directory,title,version,time_created,time_updated) \
                 VALUES ('ses_http','prj','ses_http','/tmp','session','test',1,1)",
                [],
            )
            .expect("session");
        drop(connection);
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
                .expect("goal store"),
        );
        let broker = RequestBroker::default()
            .with_store(HumanRequestStore::new(Arc::clone(&pool)))
            .with_goal_store(Arc::clone(&goals));
        (spill, pool, goals, broker)
    }

    #[test]
    fn recovered_http_requests_settle_the_same_durable_rows_and_resume_the_goal() {
        let (_spill, pool, goals, broker) = durable_broker();
        let goal = goals
            .create_goal("ses_http", "finish after HTTP answers", None)
            .expect("create goal");
        goals
            .request_human_input(
                "ses_http",
                goal.revision,
                "que_http".to_owned(),
                json!({
                    "source": "goal_request_input",
                    "questions": [{
                        "question": "Which channel?",
                        "header": "Channel",
                        "options": [],
                        "multiple": false,
                        "custom": true
                    }]
                }),
                Some("msg_http".to_owned()),
                Some("call_http".to_owned()),
            )
            .expect("persist question");
        assert_eq!(broker.questions(Some("ses_http")).len(), 1);
        assert!(
            broker
                .claim_question("ses_http", "que_http")
                .expect("claim recovered question")
                .resolve(QuestionDecision::Answered(vec![vec!["canary".to_owned()]]))
        );
        assert_eq!(
            goals
                .human_requests()
                .get("que_http")
                .expect("read question")
                .expect("question")
                .state,
            HumanRequestState::Answered
        );
        assert_eq!(
            goals
                .goal("ses_http")
                .expect("read goal")
                .expect("goal")
                .status,
            zuno_goal::GoalStatus::Active
        );

        let permission = zuno_permission::PermissionRequest {
            id: "per_http".to_owned(),
            session_id: "ses_http".to_owned(),
            permission: "shell".to_owned(),
            patterns: vec!["git push".to_owned()],
            metadata: Map::new(),
            always: Vec::new(),
            tool: Some(zuno_permission::ToolCall {
                message_id: "msg_permission".to_owned(),
                call_id: "call_permission".to_owned(),
            }),
        };
        goals
            .request_permission(
                "ses_http",
                permission.id.clone(),
                serde_json::to_value(permission).expect("serialize permission"),
                Some("msg_permission".to_owned()),
                Some("call_permission".to_owned()),
            )
            .expect("persist permission")
            .expect("active goal pauses");
        assert_eq!(broker.permissions(Some("ses_http")).len(), 1);
        assert!(
            broker
                .claim_permission("ses_http", "per_http")
                .expect("claim recovered permission")
                .resolve(ReplyKind::Once)
        );
        assert_eq!(
            zuno_db::inbox::SessionInbox::new(pool)
                .pending("ses_http")
                .expect("pending input")
                .len(),
            2
        );
    }
}
