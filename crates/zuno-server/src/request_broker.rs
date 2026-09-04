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
//! session. Applying one is recorded durably before the call runs, so an
//! auto-approved tool call is still visible in the session's history.
//!
//! Only a decision answers a request. A disconnect, a deadline, and a failed publish
//! each carry their own terminal state into the durable row, so reading the history
//! never shows a denial the user did not make.

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

/// How long a session's pending requests survive with no SSE observer.
///
/// A disconnect is a transport event, not a decision: the server itself tells a
/// lagged client to reconnect, and a proxy idle-timeout or a backgrounded tab is
/// indistinguishable from a client that is gone. Resolving on the last drop would
/// answer the user's permission prompt on their behalf, so the requests are held long
/// enough for the reconnect to re-register and left to the request deadline after
/// that.
const OBSERVER_GRACE: Duration = Duration::from_secs(30);

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

/// How a permission ask ended.
///
/// The asker only ever needs the [`ReplyKind`], but the durable row needs the reason:
/// a timeout, a disconnected stream, and a failed publish are all denials, and folding
/// them into `Answered` writes a user decision that was never made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionOutcome {
    /// A reply the user actually sent.
    Replied(ReplyKind),
    /// Every stream watching the session went away and none came back.
    Cancelled,
    /// The request deadline elapsed with no answer.
    Expired,
    /// The ask could not be published, or its resolver was dropped.
    Failed,
}

impl PermissionOutcome {
    /// The answer the asker acts on. Everything that is not a reply denies the call.
    const fn reply(self) -> ReplyKind {
        match self {
            Self::Replied(reply) => reply,
            Self::Cancelled | Self::Expired | Self::Failed => ReplyKind::Reject,
        }
    }
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
    /// The asker's sender while the ask is unclaimed; `None` once a reply owns it.
    ///
    /// A claimed ask stays in the map as a marker instead of being removed, and the
    /// marker outlives the durable write. Removing the entry on claim made a claim two
    /// transitions instead of one: a second concurrent reply found nothing live, fell
    /// through to the recovered branch — whose row is still `pending`, because the first
    /// claim has not committed yet — and settled the same request with its own decision.
    /// See [`RequestBroker::claim_permission`].
    answer: Option<oneshot::Sender<PermissionOutcome>>,
    grant: Option<StandingGrant>,
    /// Whether the durable row this ask is recovered from has been written yet.
    ///
    /// An ask is registered before that write so no durable row is ever answerable
    /// while its asker has no sender, and stays hidden until the write lands so a
    /// reply cannot settle an ask that never persisted.
    persisted: bool,
}

impl PendingPermission {
    /// Whether this entry is a claim marker rather than an answerable ask.
    const fn claimed(&self) -> bool {
        self.answer.is_none()
    }
}

struct PendingQuestion {
    request: QuestionRequest,
    /// The asker's sender while the question is unclaimed; `None` once a reply owns it.
    ///
    /// Same claim marker as [`PendingPermission::answer`], for the same reason.
    answer: Option<oneshot::Sender<QuestionDecision>>,
    /// Whether the durable row this question is recovered from has been written yet.
    ///
    /// Same invariant as [`PendingPermission::persisted`]: registering first keeps the
    /// asker's sender attached to the id, and staying hidden until the write lands keeps
    /// a client from claiming the row through the recovered-request path — which would
    /// admit inbox input for a question whose live asker is still waiting.
    persisted: bool,
}

impl PendingQuestion {
    /// Whether this entry is a claim marker rather than an answerable question.
    const fn claimed(&self) -> bool {
        self.answer.is_none()
    }
}

#[derive(Default)]
struct Pending {
    permissions: HashMap<String, PendingPermission>,
    questions: HashMap<String, PendingQuestion>,
    standing: BTreeSet<StandingGrant>,
    observers: HashMap<String, usize>,
    /// How many times a stream has registered for each observed session.
    ///
    /// A grace timer captures this at disconnect and gives up if a later stream has
    /// since registered, so a reconnect retires the previous deadline instead of
    /// letting it fire against the next disconnect's window.
    registrations: HashMap<String, u64>,
}

#[derive(Clone)]
pub struct RequestBroker {
    pending: Arc<Mutex<Pending>>,
    events: Option<EventService>,
    durable: Option<HumanRequestStore>,
    goals: Option<Arc<zuno_goal::GoalStore>>,
    request_timeout: Duration,
    observer_grace: Duration,
}

impl Default for RequestBroker {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(Pending::default())),
            events: None,
            durable: None,
            goals: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            observer_grace: OBSERVER_GRACE,
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
            .field("observer_grace", &self.observer_grace)
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
        let covered = grant
            .as_ref()
            .is_some_and(|grant| self.lock().standing.contains(grant));
        // A pre-approval that could not be recorded is not applied: the ask falls
        // through to a real prompt instead of being denied, because a store that
        // failed is not the user refusing.
        if covered && let Some(reply) = self.apply_standing_grant(&request).await {
            return reply;
        }
        let (sender, receiver) = oneshot::channel();
        self.lock().permissions.insert(
            request.id.clone(),
            PendingPermission {
                request: request.clone(),
                answer: Some(sender),
                grant,
                persisted: false,
            },
        );
        // Fail closed before the request can be answered: an ask nobody can find in the
        // durable store is an ask no restart can recover, so it never becomes
        // answerable. The write itself stays off the reactor, because the store is
        // synchronous rusqlite and on the single-threaded serve runtime a contended
        // write freezes every SSE stream and every live turn until it returns.
        if self.persist_permission(&request).await.is_err() {
            self.lock().permissions.remove(&request.id);
            return ReplyKind::Reject;
        }
        self.spawn_permission_watchdog(&request);
        if let Err(error) = self.publish("permission.v2.asked", &request).await {
            eprintln!(
                "failed to publish HTTP permission request `{}`: {error}",
                request.id
            );
            self.finish_permission(&request.session_id, &request.id, PermissionOutcome::Failed);
        }
        let outcome = receiver.await.unwrap_or(PermissionOutcome::Failed);
        self.settle_permission(&request, outcome).await;
        outcome.reply()
    }

    /// Records that a standing `always` authorized this call, then authorizes it.
    ///
    /// The grant is one session's earlier decision, but the call it pre-approves is new
    /// and model-visible, so it gets its own settled durable row rather than running
    /// with nothing in the session's history to show it happened.
    ///
    /// That row is created **already settled, in one transaction**. Creating it pending
    /// and resolving it afterwards leaves a window — and, after an unclean shutdown, a
    /// permanent row — in which [`Self::permissions`] projects an already-decided call
    /// to clients as an open prompt and [`Self::claim_permission`] lets one of them
    /// answer it, so the audit trail ends up recording a denial for a call this
    /// function authorized and the tool then ran.
    ///
    /// `None` means the authorization could not be recorded, and the caller must ask a
    /// human rather than treat that as a decision in either direction.
    async fn apply_standing_grant(&self, request: &PermissionRequest) -> Option<ReplyKind> {
        let Some(store) = self.durable.clone() else {
            // Nothing records requests at all in this wiring, so there is no row to
            // leave half-written and nothing for a client to claim.
            return Some(ReplyKind::Once);
        };
        let Some(pool) = self
            .events
            .as_ref()
            .map(crate::EventService::application_pool)
        else {
            // Without a handle on the application database the create and the settle
            // cannot share a transaction. Asking a human is the fail-closed answer;
            // pre-approving would publish the pending row this function exists to
            // avoid.
            return None;
        };
        let record = request.clone();
        let recorded = tokio::task::spawn_blocking(move || {
            let row = new_permission_row(&record)?;
            let response = json!({"reply": ReplyKind::Once, "source": "standing"});
            pool.transaction(|transaction| {
                zuno_db::human_request::create_in(transaction, &row)?;
                settle_pending(
                    transaction,
                    &record.id,
                    HumanRequestState::Answered,
                    Some(&response),
                )
            })?;
            // Read back through the store every projection and claim uses. A wiring
            // whose event log and request store point at different databases would
            // otherwise authorize the call on a row nobody else can see.
            store.get(&record.id)
        })
        .await;
        match recorded {
            Ok(Ok(Some(row))) if row.state == HumanRequestState::Answered => Some(ReplyKind::Once),
            Ok(Ok(_unrecorded)) => {
                eprintln!(
                    "the standing authorization for `{}` is not readable through the request store",
                    request.id
                );
                None
            }
            // An unrecordable authorization is not applied for the same reason an
            // unrecordable ask is not answered: the alternative is a tool call with no
            // durable trace. It is not a denial either, so the ask goes to a human.
            Ok(Err(error)) => {
                eprintln!(
                    "failed to record the standing authorization for `{}`: {error}",
                    request.id
                );
                None
            }
            Err(_joined) => None,
        }
    }

    /// Settles the durable row with the state the outcome earned.
    async fn settle_permission(&self, request: &PermissionRequest, outcome: PermissionOutcome) {
        let Some(store) = self.durable.clone() else {
            return;
        };
        let goals = self.goals.clone();
        let request_id = request.id.clone();
        let session_id = request.session_id.clone();
        let (state, response) = permission_state_and_response(outcome);
        let _settled = tokio::task::spawn_blocking(move || {
            let goal_owned = store
                .get(&request_id)
                .ok()
                .flatten()
                .is_some_and(|request| request.goal_id.is_some());
            // A reply already settled its own row inside `PermissionResolution::settle`;
            // `resolve` only touches a row that is still pending, so this is the write
            // for every path that never reached a resolution.
            let _settled = store.resolve(
                &request_id,
                state,
                response.as_ref(),
                zuno_db::message::now_millis(),
            );
            if goal_owned && let Some(goals) = &goals {
                let _resumed = goals.resume_for_work(&session_id);
            }
        })
        .await;
    }

    pub async fn ask_question(&self, request: QuestionRequest) -> QuestionDecision {
        let (sender, receiver) = oneshot::channel();
        {
            self.lock().questions.insert(
                request.id.clone(),
                PendingQuestion {
                    request: request.clone(),
                    answer: Some(sender),
                    persisted: false,
                },
            );
        }
        // Fail closed before the question can be answered, for the reason spelled out in
        // `ask_permission`, and off the reactor for the same reason: the store is
        // synchronous rusqlite, and on the single-threaded serve runtime a contended
        // write here freezes every SSE stream and every live turn.
        if self.persist_question(&request).await.is_err() {
            self.lock().questions.remove(&request.id);
            return QuestionDecision::Failed;
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
        self.settle_question(&request, &decision).await;
        decision
    }

    /// Settles the durable row with the state the decision earned, off the reactor.
    async fn settle_question(&self, request: &QuestionRequest, decision: &QuestionDecision) {
        let Some(store) = self.durable.clone() else {
            return;
        };
        let goals = self.goals.clone();
        let request_id = request.id.clone();
        let session_id = request.session_id.clone();
        let (state, response) = question_state_and_response(decision);
        let _settled = tokio::task::spawn_blocking(move || {
            let goal_owned = store
                .get(&request_id)
                .ok()
                .flatten()
                .is_some_and(|request| request.goal_id.is_some());
            // A reply already settled its own row inside `QuestionResolution::settle`;
            // `resolve` only touches a row that is still pending, so this is the write
            // for every path that never reached a resolution.
            let _settled = store.resolve(
                &request_id,
                state,
                response.as_ref(),
                zuno_db::message::now_millis(),
            );
            if goal_owned
                && state == HumanRequestState::Answered
                && let Some(goals) = &goals
            {
                let _resumed = goals.resume_for_work(&session_id);
            }
        })
        .await;
    }

    #[must_use]
    pub fn permissions(&self, session_id: Option<&str>) -> Vec<PermissionRequest> {
        // Every live id, not just the answerable ones: an ask whose row is on disk but
        // whose registration has not finished is deliberately unclaimable, so projecting
        // it from the store would offer clients a prompt no reply can settle.
        let (mut requests, live) = {
            let pending = self.lock();
            let projected = pending
                .permissions
                .values()
                .filter(|pending| {
                    pending.persisted
                        && !pending.claimed()
                        && session_id
                            .is_none_or(|session_id| pending.request.session_id == session_id)
                })
                .map(|pending| pending.request.clone())
                .collect::<Vec<_>>();
            // Every live id, including the claim markers: a request a reply already owns
            // is neither projected from here nor projected from the store below, because
            // offering it as an open prompt would advertise a request no second reply can
            // settle.
            let live = pending.permissions.keys().cloned().collect::<BTreeSet<_>>();
            (projected, live)
        };
        if let Some(store) = &self.durable
            && let Ok(pending) = store.pending(session_id)
        {
            for request in pending {
                if request.kind == HumanRequestKind::Permission
                    && !live.contains(&request.id)
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
        // See `permissions` for why an unpersisted live id is suppressed on both sides.
        let (mut requests, live) = {
            let pending = self.lock();
            let projected = pending
                .questions
                .values()
                .filter(|pending| {
                    pending.persisted
                        && !pending.claimed()
                        && session_id
                            .is_none_or(|session_id| pending.request.session_id == session_id)
                })
                .map(|pending| pending.request.clone())
                .collect::<Vec<_>>();
            let live = pending.questions.keys().cloned().collect::<BTreeSet<_>>();
            (projected, live)
        };
        if let Some(store) = &self.durable
            && let Ok(pending) = store.pending(session_id)
        {
            for request in pending {
                if request.kind == HumanRequestKind::Input
                    && !live.contains(&request.id)
                    && let Some(projected) = question_from_durable(&request)
                {
                    requests.push(projected);
                }
            }
        }
        requests.sort_by(|left, right| left.id.cmp(&right.id));
        requests
    }

    /// Overrides how long a disconnected session's requests survive.
    #[must_use]
    pub fn with_observer_grace(mut self, grace: Duration) -> Self {
        self.observer_grace = grace;
        self
    }

    pub(crate) fn observe_session(&self, session_id: &str) -> SessionRequestObserver {
        {
            let mut pending = self.lock();
            *pending.observers.entry(session_id.to_owned()).or_default() += 1;
            let registrations = pending
                .registrations
                .entry(session_id.to_owned())
                .or_default();
            *registrations = registrations.wrapping_add(1);
        }
        SessionRequestObserver {
            session_id: session_id.to_owned(),
            pending: Arc::downgrade(&self.pending),
            grace: self.observer_grace,
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

    /// Claims one permission request for exactly one reply.
    ///
    /// # A claim is a single transition, whichever branch observes the request
    ///
    /// Two branches can answer an id: the live one, holding a waiting asker's sender,
    /// and the recovered one, reading a `pending` row left by a restart. The live branch
    /// used to *remove* its map entry, so a second concurrent claim for the same id found
    /// nothing live and fell through to the recovered branch — where the row is still
    /// `pending`, because the first claim has not committed yet. Both claims then settled
    /// one request with different decisions: the recovered one wrote its own reply,
    /// published `permission.v2.replied`, admitted inbox input and installed the standing
    /// `always` grant, while the live one lost the race and the tool call was denied. The
    /// durable log, the audit row, the inbox and the standing grant all recorded an
    /// authorization the execution contradicted, and the next matching call was
    /// auto-approved on the strength of it.
    ///
    /// A claim now leaves a marker in `permissions` for as long as the durable row can
    /// still be `pending` — released by [`PermissionResolution`]'s `Drop`, after the
    /// compensating write when nothing committed — and the recovered branch refuses any
    /// id the map knows about. So the two branches cannot both be entered for one id,
    /// and neither can be entered twice.
    #[must_use]
    pub fn claim_permission(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Option<PermissionResolution> {
        // One durable read serves both branches: whether this request pauses a goal, and
        // whether a request with no live asker is still answerable.
        let row = self
            .durable
            .as_ref()
            .and_then(|store| store.get(request_id).ok().flatten())
            .filter(|row| row.session_id == session_id && row.kind == HumanRequestKind::Permission);
        let mut pending = self.lock();
        let live = match pending.permissions.get_mut(request_id) {
            // An ask that is still being registered is not answerable through either
            // branch: its durable row may not exist yet, so claiming it durably would
            // settle a request whose asker never hears the reply.
            Some(request) if !request.persisted => return None,
            // Already owned by another reply. The marker is what keeps this second
            // claimant out of the recovered branch below.
            Some(request) if request.claimed() => return None,
            Some(request) if request.request.session_id == session_id => {
                Some((request.answer.take(), request.grant.take()))
            }
            // A live ask this session does not own is not claimable, and its presence
            // keeps the recovered branch off the id as well.
            Some(_other_session) => return None,
            None => None,
        };
        let (answer, grant) = match live {
            Some(claimed) => claimed,
            None => {
                let row = row.as_ref()?;
                if row.state != HumanRequestState::Pending {
                    return None;
                }
                let projected = permission_from_durable(row)?;
                let grant = reusable_grant(&projected);
                // The recovered branch takes the same marker, so two concurrent replies
                // to one recovered id can no longer both reach `settle`.
                pending.permissions.insert(
                    request_id.to_owned(),
                    PendingPermission {
                        request: projected,
                        answer: None,
                        grant: None,
                        persisted: true,
                    },
                );
                (None, grant)
            }
        };
        drop(pending);
        Some(PermissionResolution {
            answer,
            grant,
            pending: Arc::clone(&self.pending),
            events: self.events.clone(),
            durable: self.durable.clone(),
            goals: self.goals.clone(),
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            resume_goal: row.is_some_and(|row| row.goal_id.is_some()),
        })
    }

    /// Claims one question for exactly one reply.
    ///
    /// Same single-transition contract as [`Self::claim_permission`], and the same claim
    /// marker: the live branch used to remove its entry, so a second concurrent reply
    /// took the recovered branch and both settled one question — one of them admitting
    /// model-visible input through `answer_with_input` for an answer the asking tool
    /// never received.
    #[must_use]
    pub fn claim_question(&self, session_id: &str, request_id: &str) -> Option<QuestionResolution> {
        let row = self
            .durable
            .as_ref()
            .and_then(|store| store.get(request_id).ok().flatten())
            .filter(|row| row.session_id == session_id && row.kind == HumanRequestKind::Input);
        let mut pending = self.lock();
        let live = match pending.questions.get_mut(request_id) {
            // A question that is still being registered is not answerable through
            // either branch, for the reason given in `claim_permission`.
            Some(request) if !request.persisted => return None,
            Some(request) if request.claimed() => return None,
            Some(request) if request.request.session_id == session_id => {
                Some(request.answer.take())
            }
            Some(_other_session) => return None,
            None => None,
        };
        let answer = match live {
            Some(claimed) => claimed,
            None => {
                let row = row.as_ref()?;
                if row.state != HumanRequestState::Pending {
                    return None;
                }
                let projected = question_from_durable(row)?;
                pending.questions.insert(
                    request_id.to_owned(),
                    PendingQuestion {
                        request: projected,
                        answer: None,
                        persisted: true,
                    },
                );
                None
            }
        };
        drop(pending);
        Some(QuestionResolution {
            answer,
            pending: Arc::clone(&self.pending),
            events: self.events.clone(),
            durable: self.durable.clone(),
            goals: self.goals.clone(),
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            resume_goal: row.is_some_and(|row| row.goal_id.is_some()),
        })
    }

    fn finish_permission(&self, session_id: &str, request_id: &str, outcome: PermissionOutcome) {
        finish_permission(&self.pending, session_id, request_id, outcome);
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
                finish_permission(
                    &pending,
                    &session_id,
                    &request_id,
                    PermissionOutcome::Expired,
                );
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

    /// Persists one permission ask off the reactor.
    ///
    /// `HumanRequestStore` is synchronous rusqlite and the write can wait on the
    /// database lock, so on the single-threaded serve runtime it runs on the blocking
    /// pool instead of the reactor.
    async fn persist_permission(
        &self,
        request: &PermissionRequest,
    ) -> Result<(), zuno_error::DbError> {
        let Some(store) = self.durable.clone() else {
            mark_permission_persisted(&self.pending, &request.id);
            return Ok(());
        };
        let goals = self.goals.clone();
        let pending = Arc::clone(&self.pending);
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            write_permission_row(&store, goals.as_deref(), &request)?;
            // Under the same lock the reply routes use, so an ask is answerable only
            // once the row it settles is already on disk.
            mark_permission_persisted(&pending, &request.id);
            Ok(())
        })
        .await
        .map_err(|error| zuno_error::DbError::Query {
            source: Box::new(std::io::Error::other(error.to_string())),
        })?
    }

    /// Persists one question off the reactor, then makes it answerable.
    async fn persist_question(&self, request: &QuestionRequest) -> Result<(), zuno_error::DbError> {
        let Some(store) = self.durable.clone() else {
            mark_question_persisted(&self.pending, &request.id);
            return Ok(());
        };
        let pending = Arc::clone(&self.pending);
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
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
            // Under the same lock the reply routes use, so a question is answerable only
            // once the row it settles is already on disk.
            mark_question_persisted(&pending, &request.id);
            Ok(())
        })
        .await
        .map_err(|error| zuno_error::DbError::Query {
            source: Box::new(std::io::Error::other(error.to_string())),
        })?
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
    grace: Duration,
}

impl Drop for SessionRequestObserver {
    fn drop(&mut self) {
        let Some(pending) = self.pending.upgrade() else {
            return;
        };
        let (held, generation) = {
            let mut locked = pending.lock().unwrap_or_else(PoisonError::into_inner);
            match locked.observers.get_mut(&self.session_id) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    return;
                }
                Some(_) => {
                    locked.observers.remove(&self.session_id);
                }
                None => return,
            }
            let generation = locked
                .registrations
                .get(&self.session_id)
                .copied()
                .unwrap_or_default();
            (session_request_ids(&locked, &self.session_id), generation)
        };
        if held.is_empty() {
            let mut locked = pending.lock().unwrap_or_else(PoisonError::into_inner);
            if !locked.observers.contains_key(&self.session_id) {
                locked.registrations.remove(&self.session_id);
            }
            return;
        }
        // Only the requests this stream was already showing are on the clock. One asked
        // after the disconnect gets its own full deadline rather than inheriting the
        // remainder of this window.
        let weak = Weak::clone(&self.pending);
        let session_id = self.session_id.clone();
        let grace = self.grace;
        // Off a runtime there is no asker left to answer and no watchdog left to fire,
        // so leaving the requests pending is the fail-safe rather than resolving them.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let _grace = runtime.spawn(async move {
            tokio::time::sleep(grace).await;
            cancel_unobserved(&weak, &session_id, &held, generation);
        });
    }
}

/// Cancels the named requests unless a stream came back for the session.
///
/// `generation` is the registration count this timer was armed at. A stream that
/// registered afterwards owns the session's window now, so an older timer gives up
/// rather than spending part of the newer stream's grace period.
fn cancel_unobserved(
    pending: &Weak<Mutex<Pending>>,
    session_id: &str,
    held: &SessionRequestIds,
    generation: u64,
) {
    let Some(pending) = pending.upgrade() else {
        return;
    };
    let (permissions, questions) = {
        let mut locked = pending.lock().unwrap_or_else(PoisonError::into_inner);
        if locked.observers.contains_key(session_id) {
            return;
        }
        if locked.registrations.get(session_id).copied() != Some(generation) {
            return;
        }
        locked.registrations.remove(session_id);
        take_named_requests(&mut locked, session_id, held)
    };
    for answer in permissions {
        let _delivered = answer.send(PermissionOutcome::Cancelled);
    }
    for answer in questions {
        let _delivered = answer.send(QuestionDecision::Cancelled);
    }
}

fn finish_permission(
    pending: &Mutex<Pending>,
    session_id: &str,
    request_id: &str,
    outcome: PermissionOutcome,
) {
    let answer = {
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(request) = pending.permissions.get(request_id) else {
            return;
        };
        if request.request.session_id != session_id {
            return;
        }
        // A reply already owns this id. Removing its claim marker would let a second
        // reply take the recovered branch for a row that is still `pending`, so an
        // expiry or a cancellation leaves the marker to the reply that holds it.
        if request.claimed() {
            return;
        }
        pending
            .permissions
            .remove(request_id)
            .and_then(|request| request.answer)
    };
    if let Some(answer) = answer {
        let _delivered = answer.send(outcome);
    }
}

fn finish_question(
    pending: &Mutex<Pending>,
    session_id: &str,
    request_id: &str,
    decision: QuestionDecision,
) {
    let answer = {
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(request) = pending.questions.get(request_id) else {
            return;
        };
        if request.request.session_id != session_id {
            return;
        }
        // See `finish_permission`: a claimed id belongs to the reply holding it.
        if request.claimed() {
            return;
        }
        pending
            .questions
            .remove(request_id)
            .and_then(|request| request.answer)
    };
    if let Some(answer) = answer {
        let _delivered = answer.send(decision);
    }
}

/// The pending request ids one session held at a point in time.
struct SessionRequestIds {
    permissions: Vec<String>,
    questions: Vec<String>,
}

impl SessionRequestIds {
    fn is_empty(&self) -> bool {
        self.permissions.is_empty() && self.questions.is_empty()
    }
}

/// The ids a disconnecting stream is still responsible for.
///
/// A claimed id is excluded: a reply already owns it and will settle it, so putting it
/// on the observer clock would race a committed answer against a cancellation.
fn session_request_ids(pending: &Pending, session_id: &str) -> SessionRequestIds {
    SessionRequestIds {
        permissions: pending
            .permissions
            .iter()
            .filter(|(_, request)| request.request.session_id == session_id && !request.claimed())
            .map(|(id, _)| id.clone())
            .collect(),
        questions: pending
            .questions
            .iter()
            .filter(|(_, request)| request.request.session_id == session_id && !request.claimed())
            .map(|(id, _)| id.clone())
            .collect(),
    }
}

/// Removes the named requests and returns the askers still waiting on them.
///
/// A claim marker is left in place: between the observer arming its timer and the timer
/// firing, a reply may have claimed one of these ids, and that reply owns the id until
/// its own durable outcome is known.
fn take_named_requests(
    pending: &mut Pending,
    session_id: &str,
    ids: &SessionRequestIds,
) -> (
    Vec<oneshot::Sender<PermissionOutcome>>,
    Vec<oneshot::Sender<QuestionDecision>>,
) {
    // An id can be reused only by the same session, but re-checking the owner keeps the
    // cancellation scoped to what this stream actually showed.
    let mut permissions = Vec::new();
    for id in &ids.permissions {
        let cancellable = pending
            .permissions
            .get(id)
            .is_some_and(|request| request.request.session_id == session_id && !request.claimed());
        if cancellable
            && let Some(request) = pending.permissions.remove(id)
            && let Some(answer) = request.answer
        {
            permissions.push(answer);
        }
    }
    let mut questions = Vec::new();
    for id in &ids.questions {
        let cancellable = pending
            .questions
            .get(id)
            .is_some_and(|request| request.request.session_id == session_id && !request.claimed());
        if cancellable
            && let Some(request) = pending.questions.remove(id)
            && let Some(answer) = request.answer
        {
            questions.push(answer);
        }
    }
    (permissions, questions)
}

/// Why a claimed request could not be settled.
///
/// The distinction is the HTTP status the reply routes answer with, and it exists
/// because "somebody else already answered this" and "the database refused the write"
/// are different facts about the same request.
#[derive(Debug)]
pub enum SettleError {
    /// The request is no longer answerable, and nothing was written or published.
    Gone,
    /// The reply could not be committed durably; the detail is for the log, not the client.
    Durable(String),
}

/// What happened around a reply whose durable outcome already landed.
///
/// Returning this instead of an error is the point: once the audit row, the event and —
/// for a recovered request — the inbox input have committed, the reply *did* take
/// effect, and telling the caller otherwise invites a retry that can only be refused as
/// `Gone`. The two flags describe side effects that happen after the commit and cannot
/// be rolled back into it, so they are reported rather than raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// Whether a waiting asker received the reply.
    ///
    /// `false` means the tool call that asked is already gone — it timed out, its turn
    /// was interrupted, or its client disconnected — so the call itself was not
    /// authorized even though the reply is on record. No standing `always` is installed
    /// in that case: the durable row keeps the human decision, while this process never
    /// auto-approves a later call on the strength of a reply nothing consumed.
    pub delivered: bool,
    /// Whether the goal this request paused could not be resumed.
    ///
    /// The reply is committed either way; the goal stays waiting until another resume
    /// reaches it, so this is an operational signal, not a failed write.
    pub goal_stuck: bool,
}

impl Settled {
    const fn new(delivered: bool, goal_stuck: bool) -> Self {
        Self {
            delivered,
            goal_stuck,
        }
    }
}

pub struct PermissionResolution {
    answer: Option<oneshot::Sender<PermissionOutcome>>,
    grant: Option<StandingGrant>,
    pending: Arc<Mutex<Pending>>,
    events: Option<EventService>,
    durable: Option<HumanRequestStore>,
    goals: Option<Arc<zuno_goal::GoalStore>>,
    session_id: String,
    request_id: String,
    resume_goal: bool,
}

impl PermissionResolution {
    /// Commits the reply, the event announcing it, and the state that event asserts,
    /// then delivers the reply to the waiting ask.
    ///
    /// The event is published *inside* the transaction that settles the durable row, so
    /// a `permission.v2.replied` event exists only for the reply that actually landed.
    /// Publishing first — as the reply routes used to — lets two claims of the same
    /// recovered request both announce a decision while only one of them writes one,
    /// which leaves the event log, the only thing clients reconstruct state from,
    /// asserting an authorization the audit row contradicts.
    ///
    /// Every durable write here leaves the reactor: `HumanRequestStore` is synchronous
    /// rusqlite, and on the single-threaded `zuno serve` runtime a contended write on
    /// the reply path would freeze every SSE stream and every live turn.
    pub async fn settle(mut self, reply: ReplyKind) -> Result<Settled, SettleError> {
        let live = self.answer.is_some();
        // An asker that is already gone cannot be authorized, so nothing is committed
        // for it: the alternative is an `answered` audit row and a published reply for
        // a call that never ran.
        if self.answer.as_ref().is_some_and(oneshot::Sender::is_closed) {
            return Err(SettleError::Gone);
        }
        self.commit(reply, live).await?;
        // Everything past this line is a side effect of a decision that is already
        // durable. Reporting any of it as `Err` would tell the client its reply had no
        // effect — the reply routes answer `Gone` with 404 and a `Durable` failure with
        // 500 "worth retrying" — while the audit row, the event log and the inbox all
        // say it landed, and the retry it invites can only be refused. Nothing below
        // returns an error; the claim is released by `Drop` once `durable` is clear.
        self.durable = None;
        let goal_stuck = !self.resume_goal().await;
        let delivered = self.answer.take().map_or(!live, |answer| {
            answer.send(PermissionOutcome::Replied(reply)).is_ok()
        });
        // A standing authorization is installed only once the reply it came from has
        // reached the call that asked. A reply whose asker is gone leaves that call
        // denied, so it must not leave an `always` behind that would auto-approve the
        // next matching call: the row records what the human chose, and this process
        // never widens what it allows on the strength of a reply nothing consumed.
        if delivered
            && reply == ReplyKind::Always
            && let Some(grant) = self.grant.take()
        {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .standing
                .insert(grant);
        }
        Ok(Settled::new(delivered, goal_stuck))
    }

    async fn commit(&mut self, reply: ReplyKind, live: bool) -> Result<(), SettleError> {
        let response = json!({"reply": reply});
        let request_id = self.request_id.clone();
        match (self.durable.clone(), self.events.clone()) {
            (Some(store), events) if !live => {
                // A request recovered from a restart has no asker to hand the reply to,
                // so the answer must also reach the durable inbox. `answer_with_input`
                // owns that transaction inside `zuno-db`, and its inbox insert is not
                // reachable from here, so the write commits first and the event follows.
                // The write is the authoritative record; a lost event is the safe half
                // of the pair, unlike a lost write.
                let settled = tokio::task::spawn_blocking(move || {
                    store.answer_with_input(&request_id, response, zuno_db::message::now_millis())
                })
                .await
                .map_err(|error| worker_error(&error))?;
                match settled {
                    Ok(Some(_answered)) => {}
                    Ok(None) => return Err(SettleError::Gone),
                    Err(error) => return Err(SettleError::Durable(error.to_string())),
                }
                if let Some(events) = events {
                    self.announce(events, reply).await;
                }
                Ok(())
            }
            (Some(_store), Some(events)) => {
                let event = permission_reply_event(&self.session_id, &self.request_id, reply)
                    .map_err(|error| SettleError::Durable(error.to_string()))?;
                let settle_id = request_id.clone();
                events
                    .publish_with(&self.session_id, event, move |transaction| {
                        settle_pending(
                            transaction,
                            &settle_id,
                            HumanRequestState::Answered,
                            Some(&response),
                        )
                        .map(|_settled| ())
                    })
                    .await
                    .map_err(|error| settle_error(&request_id, &error))?;
                Ok(())
            }
            (Some(store), None) => {
                let settled = tokio::task::spawn_blocking(move || {
                    resolve_if_pending(
                        &store,
                        &request_id,
                        HumanRequestState::Answered,
                        Some(&response),
                    )
                })
                .await
                .map_err(|error| worker_error(&error))?;
                match settled {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(SettleError::Gone),
                    Err(error) => Err(SettleError::Durable(error.to_string())),
                }
            }
            (None, Some(events)) => {
                // Nothing records requests in this wiring, so there is no row for the
                // event to contradict.
                self.announce(events, reply).await;
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    /// Publishes a reply whose durable row is already committed.
    async fn announce(&self, events: EventService, reply: ReplyKind) {
        let published = match permission_reply_event(&self.session_id, &self.request_id, reply) {
            Ok(event) => events
                .publish(&self.session_id, event)
                .await
                .map(|_stored| ()),
            Err(error) => Err(error),
        };
        if let Err(error) = published {
            eprintln!(
                "failed to publish the reply to permission `{}`: {error}",
                self.request_id
            );
        }
    }

    /// Wakes the goal this request paused. `false` means it is still waiting.
    ///
    /// Called only after the reply is durable, so a failure here cannot be reported as
    /// a failed reply. See [`Settled::goal_stuck`].
    async fn resume_goal(&mut self) -> bool {
        let Some(goals) = self.goals.take().filter(|_goals| self.resume_goal) else {
            return true;
        };
        let session_id = self.session_id.clone();
        let resumed = tokio::task::spawn_blocking(move || goals.resume_for_work(&session_id)).await;
        let failure: Option<String> = match resumed {
            Ok(Ok(_resumed)) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(failure) = failure {
            eprintln!(
                "the reply to permission `{}` is committed, but its goal did not resume: {failure}",
                self.request_id
            );
            return false;
        }
        true
    }
}

impl Drop for PermissionResolution {
    fn drop(&mut self) {
        if let Some(answer) = self.answer.take() {
            let _delivered = answer.send(PermissionOutcome::Failed);
        }
        let pending = Arc::clone(&self.pending);
        let request_id = self.request_id.clone();
        let release = move || release_permission_claim(&pending, &request_id);
        match self.durable.take() {
            // Nothing committed: the row is settled `failed` first and the claim is
            // released only afterwards, so there is no window in which the row is
            // `pending` and the id unclaimed.
            Some(store) => fail_claim(store, self.request_id.clone(), release),
            None => release(),
        }
    }
}

pub struct QuestionResolution {
    answer: Option<oneshot::Sender<QuestionDecision>>,
    pending: Arc<Mutex<Pending>>,
    events: Option<EventService>,
    durable: Option<HumanRequestStore>,
    goals: Option<Arc<zuno_goal::GoalStore>>,
    session_id: String,
    request_id: String,
    resume_goal: bool,
}

impl QuestionResolution {
    /// Commits the decision, the event announcing it, and the row state it asserts,
    /// then delivers the decision. See [`PermissionResolution::settle`].
    pub async fn settle(mut self, decision: QuestionDecision) -> Result<Settled, SettleError> {
        let live = self.answer.is_some();
        if self.answer.as_ref().is_some_and(oneshot::Sender::is_closed) {
            return Err(SettleError::Gone);
        }
        self.commit(&decision, live).await?;
        // See `PermissionResolution::settle`: past the commit the answer is durable —
        // for a recovered question it is also model-visible inbox input — so a lost
        // asker or a stuck goal is reported, not raised.
        self.durable = None;
        let goal_stuck = !self.resume_goal().await;
        let delivered = self
            .answer
            .take()
            .map_or(!live, |answer| answer.send(decision).is_ok());
        Ok(Settled::new(delivered, goal_stuck))
    }

    async fn commit(&mut self, decision: &QuestionDecision, live: bool) -> Result<(), SettleError> {
        let (state, response) = question_state_and_response(decision);
        let request_id = self.request_id.clone();
        match (self.durable.clone(), self.events.clone()) {
            (Some(store), events) if !live && state == HumanRequestState::Answered => {
                // Recovered claim: the answer is model-visible input, so it commits
                // through `answer_with_input` first and the event follows. See
                // `PermissionResolution::commit`.
                let response = response.expect("answered questions carry a response");
                let settled = tokio::task::spawn_blocking(move || {
                    store.answer_with_input(&request_id, response, zuno_db::message::now_millis())
                })
                .await
                .map_err(|error| worker_error(&error))?;
                match settled {
                    Ok(Some(_answered)) => {}
                    Ok(None) => return Err(SettleError::Gone),
                    Err(error) => return Err(SettleError::Durable(error.to_string())),
                }
                if let Some(events) = events {
                    self.announce(events, decision).await;
                }
                Ok(())
            }
            (Some(_store), Some(events)) => {
                let event = question_reply_event(&self.session_id, &self.request_id, decision)
                    .map_err(|error| SettleError::Durable(error.to_string()))?;
                let settle_id = request_id.clone();
                events
                    .publish_with(&self.session_id, event, move |transaction| {
                        settle_pending(transaction, &settle_id, state, response.as_ref())
                            .map(|_settled| ())
                    })
                    .await
                    .map_err(|error| settle_error(&request_id, &error))?;
                Ok(())
            }
            (Some(store), None) => {
                let settled = tokio::task::spawn_blocking(move || {
                    resolve_if_pending(&store, &request_id, state, response.as_ref())
                })
                .await
                .map_err(|error| worker_error(&error))?;
                match settled {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(SettleError::Gone),
                    Err(error) => Err(SettleError::Durable(error.to_string())),
                }
            }
            (None, Some(events)) => {
                self.announce(events, decision).await;
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    async fn announce(&self, events: EventService, decision: &QuestionDecision) {
        let published = match question_reply_event(&self.session_id, &self.request_id, decision) {
            Ok(event) => events
                .publish(&self.session_id, event)
                .await
                .map(|_stored| ()),
            Err(error) => Err(error),
        };
        if let Err(error) = published {
            eprintln!(
                "failed to publish the reply to question `{}`: {error}",
                self.request_id
            );
        }
    }

    /// Wakes the goal this question paused. `false` means it is still waiting.
    async fn resume_goal(&mut self) -> bool {
        let Some(goals) = self.goals.take().filter(|_goals| self.resume_goal) else {
            return true;
        };
        let session_id = self.session_id.clone();
        let resumed = tokio::task::spawn_blocking(move || goals.resume_for_work(&session_id)).await;
        let failure: Option<String> = match resumed {
            Ok(Ok(_resumed)) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(failure) = failure {
            eprintln!(
                "the answer to question `{}` is committed, but its goal did not resume: {failure}",
                self.request_id
            );
            return false;
        }
        true
    }
}

impl Drop for QuestionResolution {
    fn drop(&mut self) {
        if let Some(answer) = self.answer.take() {
            let _delivered = answer.send(QuestionDecision::Failed);
        }
        let pending = Arc::clone(&self.pending);
        let request_id = self.request_id.clone();
        let release = move || release_question_claim(&pending, &request_id);
        match self.durable.take() {
            Some(store) => fail_claim(store, self.request_id.clone(), release),
            None => release(),
        }
    }
}

/// Releases a permission claim marker once its reply's outcome is settled.
///
/// Only a marker is removed. An entry whose asker is still waiting unclaimed belongs to
/// a later ask that happens to reuse the id, and taking it would strand that asker.
fn release_permission_claim(pending: &Mutex<Pending>, request_id: &str) {
    let mut locked = pending.lock().unwrap_or_else(PoisonError::into_inner);
    if locked
        .permissions
        .get(request_id)
        .is_some_and(PendingPermission::claimed)
    {
        locked.permissions.remove(request_id);
    }
}

/// Releases a question claim marker. See [`release_permission_claim`].
fn release_question_claim(pending: &Mutex<Pending>, request_id: &str) {
    let mut locked = pending.lock().unwrap_or_else(PoisonError::into_inner);
    if locked
        .questions
        .get(request_id)
        .is_some_and(PendingQuestion::claimed)
    {
        locked.questions.remove(request_id);
    }
}

fn permission_state_and_response(outcome: PermissionOutcome) -> (HumanRequestState, Option<Value>) {
    match outcome {
        PermissionOutcome::Replied(reply) => {
            (HumanRequestState::Answered, Some(json!({"reply": reply})))
        }
        PermissionOutcome::Cancelled => (HumanRequestState::Cancelled, None),
        PermissionOutcome::Expired => (HumanRequestState::Expired, None),
        PermissionOutcome::Failed => (HumanRequestState::Failed, None),
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

/// Makes a registered question answerable, now that the row it settles exists.
fn mark_question_persisted(pending: &Mutex<Pending>, request_id: &str) {
    if let Some(request) = pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .questions
        .get_mut(request_id)
    {
        request.persisted = true;
    }
}

/// Makes a registered ask answerable, now that the row it settles exists.
fn mark_permission_persisted(pending: &Mutex<Pending>, request_id: &str) {
    if let Some(request) = pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .permissions
        .get_mut(request_id)
    {
        request.persisted = true;
    }
}

/// Writes the durable row one permission ask is recovered from.
fn write_permission_row(
    store: &HumanRequestStore,
    goals: Option<&zuno_goal::GoalStore>,
    request: &PermissionRequest,
) -> Result<(), zuno_error::DbError> {
    // A goal-owned ask lives on the row `request_permission` writes, which also pauses
    // the goal, so it is not duplicated into a free-standing row.
    if let Some(goals) = goals
        && goals
            .request_permission(
                &request.session_id,
                request.id.clone(),
                permission_json(request)?,
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
    create_permission_row(store, request)
}

/// Inserts the `human_request` row one permission ask is recovered from.
fn create_permission_row(
    store: &HumanRequestStore,
    request: &PermissionRequest,
) -> Result<(), zuno_error::DbError> {
    store.create(new_permission_row(request)?)?;
    Ok(())
}

/// The `human_request` row one permission ask is recovered from.
///
/// Separate from [`create_permission_row`] so a caller that must insert and settle in
/// one transaction can hand the row to [`zuno_db::human_request::create_in`].
fn new_permission_row(request: &PermissionRequest) -> Result<NewHumanRequest, zuno_error::DbError> {
    Ok(NewHumanRequest {
        id: request.id.clone(),
        session_id: request.session_id.clone(),
        goal_id: None,
        kind: HumanRequestKind::Permission,
        payload: permission_json(request)?,
        message_id: request
            .source
            .as_ref()
            .map(|source| source.message_id.clone()),
        call_id: request.source.as_ref().map(|source| source.call_id.clone()),
        time_created: zuno_db::message::now_millis(),
    })
}

/// Settles a row that must still be pending, inside a wider transaction.
///
/// [`zuno_db::human_request::resolve_in`] updates `WHERE state = 'pending'` but returns
/// the row either way, so a caller that only checks for `Some` cannot tell its update
/// from a no-op against a row somebody else already settled. Reading the state in the
/// same transaction turns that lost race into a typed `NotFound`, which is what lets a
/// second reply to one request fail without publishing an event for it.
fn settle_pending(
    transaction: &rusqlite::Transaction<'_>,
    request_id: &str,
    state: HumanRequestState,
    response: Option<&Value>,
) -> Result<HumanRequest, zuno_error::DbError> {
    let current = zuno_db::human_request::get_from(transaction, request_id)?;
    if !current.is_some_and(|row| row.state == HumanRequestState::Pending) {
        return Err(not_pending(request_id));
    }
    zuno_db::human_request::resolve_in(
        transaction,
        request_id,
        state,
        response,
        zuno_db::message::now_millis(),
    )?
    .ok_or_else(|| not_pending(request_id))
}

/// Settles a still-pending row in its own transaction.
///
/// The wiring without an event log has no handle on the application pool, so the read
/// and the update are two transactions here. `false` means the row was already settled
/// or is gone, exactly as in [`settle_pending`].
///
/// Two transactions is why the returned row is checked against what this call asked for
/// rather than only against `state`. Another writer on the same database — a second
/// `zuno serve` over one `zuno.db` — can settle the row between the read and the update;
/// `resolve_in` updates `WHERE state = 'pending'` and then re-reads the row either way,
/// so the loser is handed the *winner's* row, already in the state it wanted. Matching
/// the state alone reports that lost race as a success, and a permission reply that
/// never took effect would then be treated as a decision and install its standing
/// `always` grant. Requiring the stored response to be the one this call wrote makes the
/// loser fail closed: no grant, and the caller is told the request is gone.
fn resolve_if_pending(
    store: &HumanRequestStore,
    request_id: &str,
    state: HumanRequestState,
    response: Option<&Value>,
) -> Result<bool, zuno_error::DbError> {
    if !store
        .get(request_id)?
        .is_some_and(|row| row.state == HumanRequestState::Pending)
    {
        return Ok(false);
    }
    let Some(settled) =
        store.resolve(request_id, state, response, zuno_db::message::now_millis())?
    else {
        return Ok(false);
    };
    Ok(settled.state == state && settled.response.as_ref() == response)
}

fn not_pending(request_id: &str) -> zuno_error::DbError {
    zuno_error::DbError::NotFound {
        table: "human_request".to_owned(),
        id: request_id.to_owned(),
    }
}

/// The durable event announcing one permission reply.
fn permission_reply_event(
    session_id: &str,
    request_id: &str,
    reply: ReplyKind,
) -> Result<NewEvent, EventStreamError> {
    NewEvent::new(
        "permission.v2.replied",
        event_properties(&json!({
            "sessionID": session_id,
            "requestID": request_id,
            "reply": reply,
        })),
    )
}

/// The durable event announcing how one question ended.
fn question_reply_event(
    session_id: &str,
    request_id: &str,
    decision: &QuestionDecision,
) -> Result<NewEvent, EventStreamError> {
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
            json!({"sessionID": session_id, "requestID": request_id}),
        ),
        QuestionDecision::Expired => (
            "question.v2.expired",
            json!({"sessionID": session_id, "requestID": request_id}),
        ),
        QuestionDecision::Failed => (
            "question.v2.failed",
            json!({"sessionID": session_id, "requestID": request_id}),
        ),
    };
    NewEvent::new(event_type, event_properties(&properties))
}

fn event_properties(payload: &Value) -> Map<String, Value> {
    payload
        .as_object()
        .cloned()
        .expect("request and reply event payloads are objects")
}

/// Whether a failed settle means the row is no longer answerable.
fn settle_error(request_id: &str, error: &EventStreamError) -> SettleError {
    match error {
        EventStreamError::Database(zuno_error::DbError::NotFound { table, id })
            if table == "human_request" && id == request_id =>
        {
            SettleError::Gone
        }
        other => SettleError::Durable(other.to_string()),
    }
}

/// Marks an abandoned claim failed without blocking the reactor.
///
/// `Drop` cannot await, so the write is handed to the blocking pool when there is a
/// runtime to hand it to. Off a runtime it runs inline, because the alternative is
/// leaving the row pending forever.
fn fail_claim(
    store: HumanRequestStore,
    request_id: String,
    release: impl FnOnce() + Send + 'static,
) {
    let settle = move || {
        // Guarded, because an abandoned claim is not the only way a row leaves
        // `pending`: another process may have answered it, and overwriting that answer
        // with `failed` would destroy a human decision this process merely lost a race
        // to.
        let _settled = resolve_if_pending(&store, &request_id, HumanRequestState::Failed, None);
        // Only now: while the row could still read `pending`, the claim marker is what
        // keeps a second reply out of the recovered branch.
        release();
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        let _worker = tokio::task::spawn_blocking(settle);
    } else {
        settle();
    }
}

fn worker_error(error: &tokio::task::JoinError) -> SettleError {
    SettleError::Durable(error.to_string())
}

/// The stored payload of a permission ask.
fn permission_json(request: &PermissionRequest) -> Result<Value, zuno_error::DbError> {
    serde_json::to_value(permission_payload(request)).map_err(|source| {
        zuno_error::DbError::Decode {
            table: "human_request".to_owned(),
            source,
        }
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
        durable_broker_at(Storage::Memory)
    }

    /// The same fixture on disk, for a test that holds the database's write lock.
    ///
    /// A shared-cache `:memory:` database refuses to *open* a second connection while
    /// another one holds a write (`SQLITE_LOCKED`, "database table is locked"), so a
    /// contended write there fails instead of waiting. `zuno serve` runs on a file, where
    /// the second writer queues on `busy_timeout` — which is the behaviour a test about
    /// what the reactor does while a write waits has to exercise.
    fn durable_broker_on_disk() -> (
        tempfile::TempDir,
        Arc<zuno_db::Pool>,
        Arc<zuno_goal::GoalStore>,
        RequestBroker,
    ) {
        durable_broker_at(Storage::File)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Storage {
        Memory,
        File,
    }

    fn durable_broker_at(
        storage: Storage,
    ) -> (
        tempfile::TempDir,
        Arc<zuno_db::Pool>,
        Arc<zuno_goal::GoalStore>,
        RequestBroker,
    ) {
        let spill = tempfile::tempdir().expect("spill directory");
        let location = match storage {
            Storage::Memory => zuno_paths::DbLocation::Memory,
            Storage::File => zuno_paths::DbLocation::File(spill.path().join("zuno.db")),
        };
        let pool = Arc::new(zuno_db::Pool::open(&location).expect("open database"));
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
        // Events, request store, and goal store over one database, which is the only
        // wiring `zuno serve` builds (`crates/zuno-cli/src/cmd/serve.rs`): the reply and
        // standing-grant paths commit an event and a row in one transaction, so a
        // fixture without an event service would exercise a shape production never has.
        let broker = RequestBroker::with_events(crate::EventService::new(Arc::clone(&pool), 64))
            .with_store(HumanRequestStore::new(Arc::clone(&pool)))
            .with_goal_store(Arc::clone(&goals));
        (spill, pool, goals, broker)
    }

    /// One permission ask against the fixture session.
    fn ask(id: &str, save: Vec<String>) -> PermissionRequest {
        PermissionRequest {
            id: id.to_owned(),
            session_id: "ses_http".to_owned(),
            action: "shell".to_owned(),
            resources: vec!["git push".to_owned()],
            save,
            metadata: Map::new(),
            source: None,
        }
    }

    /// How many `permission.v2.replied` events the fixture session holds, and what they say.
    async fn published_replies(pool: &Arc<zuno_db::Pool>) -> Vec<Value> {
        crate::EventService::new(Arc::clone(pool), 64)
            .replay("ses_http", None)
            .await
            .expect("the durable event log replays")
            .into_iter()
            .filter(|event| event.event_type() == "permission.v2.replied")
            .map(|event| Value::Object(event.properties().clone()))
            .collect()
    }

    /// How many inbox rows one request admitted. `answer_with_input` uses `human_<id>`.
    fn inbox_admissions(pool: &Arc<zuno_db::Pool>, request_id: &str) -> i64 {
        pool.get()
            .expect("connection")
            .query_row(
                "SELECT COUNT(*) FROM session_input WHERE id = ?1",
                [format!("human_{request_id}")],
                |row| row.get(0),
            )
            .expect("the inbox count reads")
    }

    /// Waits until `broker` is asking a human about `request_id`, with a real-clock ceiling.
    ///
    /// The wait sleeps rather than spinning: these tests share a machine with other
    /// tests that run on a paused clock, and a `yield_now` spin starves them.
    async fn wait_for_ask(broker: &RequestBroker, request_id: &str) {
        let ceiling = std::time::Instant::now() + Duration::from_secs(60);
        while !broker
            .permissions(None)
            .iter()
            .any(|request| request.id == request_id)
        {
            assert!(
                std::time::Instant::now() < ceiling,
                "`{request_id}` never reached the human"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// The reviewed input: one *live* permission ask, claimed twice.
    ///
    /// The reviewer asked `per_live2` with `always: ["shell"]`, waited for the ask to be
    /// live, then called `claim_permission("ses_http", "per_live2")` twice and settled the
    /// two claims with `always` and `reject`. The live branch used to *remove* its map
    /// entry, so the second claim found nothing live and fell through to the recovered
    /// branch — whose only filter was `state == Pending`, still true, because the first
    /// claim had not committed yet. Both claims settled one request, and the output was:
    ///
    /// ```text
    /// recovered-claim (always) settle: Ok(())
    /// live-claim (reject) settle: Err(Gone)
    /// the tool call actually saw: Reject
    /// audit row: Answered Some({"reply":"always"})
    /// durable inbox admissions: 1
    /// the NEXT matching call was auto-answered: Once
    /// ```
    ///
    /// The audit row, the event log, the inbox and the standing grant all recorded that
    /// the user approved `git push` with `always`, for a call that was denied.
    ///
    /// The oracle is not "the second claim is rejected" but what the second claim would
    /// have produced: one reply on the audit row, one published event, no inbox input,
    /// and a next matching call that still asks a human.
    #[tokio::test]
    async fn a_live_permission_ask_cannot_be_claimed_twice() {
        let (_spill, pool, _goals, broker) = durable_broker();
        let asker = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_live2", vec!["shell".to_owned()]))
                    .await
            }
        });
        wait_for_ask(&broker, "per_live2").await;

        let live = broker
            .claim_permission("ses_http", "per_live2")
            .expect("the live ask is claimable once");
        assert!(
            broker.claim_permission("ses_http", "per_live2").is_none(),
            "a second claim took the recovered branch for a request a reply already owns, \
             so two contradictory decisions can settle one call"
        );
        assert!(
            !broker
                .permissions(None)
                .iter()
                .any(|request| request.id == "per_live2"),
            "a claimed request must not still be offered as an open prompt"
        );

        let settled = live
            .settle(ReplyKind::Reject)
            .await
            .expect("the surviving claim settles");
        assert!(settled.delivered, "the asker was still waiting");
        assert_eq!(
            asker.await.expect("the asker task does not panic"),
            ReplyKind::Reject,
            "the call must see the decision the record keeps"
        );

        let row = HumanRequestStore::new(Arc::clone(&pool))
            .get("per_live2")
            .expect("the audit row reads")
            .expect("the ask is persisted before it can be answered");
        assert_eq!(row.state, HumanRequestState::Answered);
        assert_eq!(
            row.response
                .as_ref()
                .and_then(|response| response.get("reply")),
            Some(&json!("reject")),
            "the audit row must record the reply the call received"
        );
        let published = published_replies(&pool).await;
        assert_eq!(
            published.len(),
            1,
            "one request may publish one reply: {published:?}"
        );
        assert_eq!(
            published[0].get("reply"),
            Some(&json!("reject")),
            "the event log clients rebuild state from must agree with the audit row"
        );
        assert_eq!(
            inbox_admissions(&pool, "per_live2"),
            0,
            "a live ask hands its reply to the waiting call, so nothing may enter the \
             durable inbox as model-visible input as well"
        );

        // The consequence the reviewer measured, inverted: nothing installed a standing
        // `always`, so the next matching call still asks.
        let next = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_live2_next", vec!["shell".to_owned()]))
                    .await
            }
        });
        wait_for_ask(&broker, "per_live2_next").await;
        assert!(
            !next.is_finished(),
            "a rejected call must not leave an `always` behind that auto-approves the \
             next `git push`"
        );
        next.abort();
    }

    /// The reviewed input: the asker disappears while the reply is committing.
    ///
    /// The reviewer aborted the asking task "the moment the claimed reply starts
    /// committing", and the caller was told `Err(Gone)` — which the reply route turns
    /// into `404`, documented as "the client's reply had no effect either way" — while
    /// the audit row read `Answered {"reply":"always"}` and `permission.v2.replied` was
    /// in the log. A client that believes the 404 retries, and the retry is refused
    /// because the row it wrote is no longer pending.
    ///
    /// Holding the database's write lock is what makes "mid-commit" deterministic: the
    /// settle parks inside its transaction, the asker goes away there, and only then does
    /// the write land.
    #[tokio::test]
    async fn a_reply_whose_asker_disappears_mid_commit_is_recorded_not_refused() {
        let (_spill, pool, _goals, broker) = durable_broker_on_disk();
        let asker = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_gone2", vec!["shell".to_owned()]))
                    .await
            }
        });
        wait_for_ask(&broker, "per_gone2").await;
        let claim = broker
            .claim_permission("ses_http", "per_gone2")
            .expect("the live ask is claimable");

        let blocker = pool.get().expect("blocking connection");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the test holds the write lock");
        let mut settling = std::pin::pin!(claim.settle(ReplyKind::Always));
        for _ in 0..64 {
            assert!(
                futures::poll!(&mut settling).is_pending(),
                "the reply must park on the held write lock"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Exactly the reviewer's moment: the reply is inside its transaction and the call
        // that asked is gone.
        asker.abort();
        let ceiling = std::time::Instant::now() + Duration::from_secs(60);
        while !asker.is_finished() {
            assert!(
                std::time::Instant::now() < ceiling,
                "the asker never went away"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        blocker
            .execute_batch("ROLLBACK")
            .expect("the test releases the write lock");
        drop(blocker);

        let settled = settling
            .await
            .expect("a reply whose row and event committed is not a failed write");
        assert!(
            !settled.delivered,
            "the call that asked was gone, and the caller has to be told which of the two \
             happened"
        );
        let row = HumanRequestStore::new(Arc::clone(&pool))
            .get("per_gone2")
            .expect("the audit row reads")
            .expect("the ask is persisted before it can be answered");
        assert_eq!(
            row.state,
            HumanRequestState::Answered,
            "the reply committed, so the row may not be reported as still pending"
        );
        assert_eq!(
            published_replies(&pool).await.len(),
            1,
            "the published reply describes a write that landed"
        );

        // The reply is on record, but it authorized no call, so it grants nothing further.
        let next = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_gone2_next", vec!["shell".to_owned()]))
                    .await
            }
        });
        wait_for_ask(&broker, "per_gone2_next").await;
        assert!(
            !next.is_finished(),
            "an `always` no call ever received must not auto-approve the next `git push`"
        );
        next.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_permission_ask_is_recorded_as_expired() {
        let (_spill, pool, _goals, broker) = durable_broker();
        let answer = tokio::spawn({
            let broker = broker.clone();
            async move { broker.ask_permission(ask("per_expired", Vec::new())).await }
        });
        // A `yield_now` spin keeps the runtime runnable, so this test's paused clock
        // never auto-advances and the deadline fires only where the test advances it.
        while broker.permissions(None).is_empty() {
            tokio::task::yield_now().await;
        }

        tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;

        // The ceiling is on the *real* clock, not this test's paused one: the asker
        // settles its durable row on the blocking pool, and `tokio::time::timeout` under
        // a paused clock with an idle reactor jumps ahead of that write. The spin keeps
        // the runtime runnable (so the paused clock never auto-advances) while
        // `std::time::Instant` bounds how long a regression can hang the suite, which
        // `cargo test` alone would not.
        let ceiling = std::time::Instant::now() + Duration::from_secs(60);
        let mut answer = std::pin::pin!(answer);
        let outcome = loop {
            if let std::task::Poll::Ready(outcome) = futures::poll!(&mut answer) {
                break outcome.expect("permission asker task does not panic");
            }
            assert!(
                std::time::Instant::now() < ceiling,
                "the elapsed deadline never released the asker"
            );
            tokio::task::yield_now().await;
        };
        assert_eq!(outcome, ReplyKind::Reject);
        let recorded = HumanRequestStore::new(pool)
            .get("per_expired")
            .expect("read the permission row")
            .expect("the ask is persisted before it can be answered");
        assert_eq!(recorded.state, HumanRequestState::Expired);
        assert_eq!(
            recorded.response, None,
            "an elapsed deadline is not a user decision, so it leaves no reply behind"
        );
    }

    #[tokio::test]
    async fn a_standing_grant_records_the_call_it_authorizes() {
        let (_spill, pool, _goals, broker) = durable_broker();
        let mut saved = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_saved", vec!["shell".to_owned()]))
                    .await
            }
        });
        while broker.permissions(None).is_empty() {
            tokio::task::yield_now().await;
        }
        broker
            .claim_permission("ses_http", "per_saved")
            .expect("claim the live permission")
            .settle(ReplyKind::Always)
            .await
            .expect("the reply settles the live permission");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), &mut saved)
                .await
                .expect("the reply releases the asker")
                .expect("permission asker task does not panic"),
            ReplyKind::Always
        );

        let reply = broker
            .ask_permission(ask("per_standing", vec!["shell".to_owned()]))
            .await;

        assert_eq!(reply, ReplyKind::Once);
        let recorded = HumanRequestStore::new(pool)
            .get("per_standing")
            .expect("read the pre-approved permission row")
            .expect("a standing grant still records the call it authorized");
        assert_eq!(recorded.state, HumanRequestState::Answered);
        assert_eq!(
            recorded
                .response
                .as_ref()
                .and_then(|response| response.get("source")),
            Some(&json!("standing")),
            "the history has to show that no human answered this ask"
        );
    }

    /// Saves the `always` grant this session will reuse, then blocks the audit write.
    ///
    /// The `BEFORE UPDATE` trigger is the review's input — the settle half of the audit
    /// write failing while the insert half succeeds — expressed as a deterministic
    /// failure instead of a race.
    async fn broker_with_a_standing_grant_and_no_settles(
        pool: &Arc<zuno_db::Pool>,
        broker: &RequestBroker,
    ) {
        let mut saved = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_saved", vec!["shell".to_owned()]))
                    .await
            }
        });
        loop {
            if let Some(resolution) = broker.claim_permission("ses_http", "per_saved") {
                resolution
                    .settle(ReplyKind::Always)
                    .await
                    .expect("the always reply settles");
                break;
            }
            assert!(!saved.is_finished(), "the first ask must reach a human");
            tokio::task::yield_now().await;
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), &mut saved)
                .await
                .expect("the reply releases the asker")
                .expect("permission asker task does not panic"),
            ReplyKind::Always
        );
        pool.get()
            .expect("connection")
            .execute_batch(
                "CREATE TRIGGER refuse_settle BEFORE UPDATE ON human_request \
                 BEGIN SELECT RAISE(ABORT, 'settle refused'); END",
            )
            .expect("install the settle failure");
    }

    /// An unrecordable standing grant leaves no row and reports no decision.
    ///
    /// With two transactions the insert survived the failed settle as a `pending` row:
    /// [`RequestBroker::permissions`] projected an already-authorized call to clients as
    /// an open prompt, [`RequestBroker::claim_permission`] let one of them answer it, and
    /// after an unclean shutdown the row stayed pending forever. The grant also reported
    /// itself as `Reject`, which is a fabricated user denial for a call the user had
    /// already allowed. One transaction leaves nothing to project, and an authorization
    /// that cannot be recorded is not a decision in either direction.
    #[tokio::test]
    async fn an_unrecordable_standing_grant_leaves_no_row_and_no_decision() {
        // On disk: this test changes the schema under a running broker, and a
        // shared-cache `:memory:` database refuses DDL while any other connection holds
        // the table.
        let (_spill, pool, _goals, broker) = durable_broker_on_disk();
        broker_with_a_standing_grant_and_no_settles(&pool, &broker).await;

        let covered = ask("per_standing", vec!["shell".to_owned()]);
        assert_eq!(
            broker.apply_standing_grant(&covered).await,
            None,
            "an authorization that cannot be recorded is not a decision, and never a denial"
        );

        let store = HumanRequestStore::new(Arc::clone(&pool));
        assert_eq!(
            store.get("per_standing").expect("read the request row"),
            None,
            "the rolled-back grant must leave no row for a restart to find pending"
        );
        assert!(
            broker.permissions(None).is_empty(),
            "a decided call must never be projected to clients as an open prompt"
        );
    }

    /// A call the grant cannot record reaches a human, and that human decides it.
    ///
    /// End to end over [`RequestBroker::ask_permission`]: a transient store failure used
    /// to answer `Reject` on the user's behalf for a call the user had already allowed.
    /// The call now waits for a real answer, and a real `deny` is what decides it.
    #[tokio::test]
    async fn a_call_whose_standing_grant_fails_waits_for_a_human_answer() {
        // On disk: this test changes the schema under a running broker, and a
        // shared-cache `:memory:` database refuses DDL while any other connection holds
        // the table.
        let (_spill, pool, _goals, broker) = durable_broker_on_disk();
        broker_with_a_standing_grant_and_no_settles(&pool, &broker).await;

        let mut asking = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_standing", vec!["shell".to_owned()]))
                    .await
            }
        });
        // Two transactions reached this point by projecting the phantom row the failed
        // settle left behind; one transaction reaches it with a real waiting ask.
        while !broker
            .permissions(None)
            .iter()
            .any(|request| request.id == "per_standing")
        {
            assert!(
                !asking.is_finished(),
                "an unrecordable standing grant must not decide the call itself"
            );
            tokio::task::yield_now().await;
        }
        pool.get()
            .expect("connection")
            .execute_batch("DROP TRIGGER refuse_settle")
            .expect("let replies settle again");

        let resolution = broker
            .claim_permission("ses_http", "per_standing")
            .expect("the projected prompt is claimable");
        assert!(
            resolution.answer.is_some(),
            "the projected prompt must belong to a waiting asker, not to a phantom row \
             left behind by a rolled-back grant"
        );
        resolution
            .settle(ReplyKind::Once)
            .await
            .expect("the human's allow settles");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), &mut asking)
                .await
                .expect("the answer releases the asker")
                .expect("permission asker task does not panic"),
            ReplyKind::Once,
            "the reply the human actually gave is the one the tool sees"
        );
    }

    /// Without an atomic recorder a covered call asks a human rather than guessing.
    ///
    /// A broker with a request store but no event log has no handle on the application
    /// database, so the insert and the settle cannot share a transaction. Pre-approving
    /// anyway would publish exactly the answerable pending row the atomic path exists to
    /// avoid, so this wiring falls through to a prompt.
    #[tokio::test]
    async fn a_standing_grant_without_an_atomic_recorder_asks_a_human() {
        let (_spill, pool, _goals, _events_broker) = durable_broker();
        let broker = RequestBroker::default().with_store(HumanRequestStore::new(Arc::clone(&pool)));
        let mut saved = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_saved", vec!["shell".to_owned()]))
                    .await
            }
        });
        loop {
            if let Some(resolution) = broker.claim_permission("ses_http", "per_saved") {
                resolution
                    .settle(ReplyKind::Always)
                    .await
                    .expect("the always reply settles");
                break;
            }
            assert!(!saved.is_finished(), "the first ask must reach a human");
            tokio::task::yield_now().await;
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), &mut saved)
                .await
                .expect("the reply releases the asker")
                .expect("permission asker task does not panic"),
            ReplyKind::Always
        );

        let mut asking = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_permission(ask("per_standing", vec!["shell".to_owned()]))
                    .await
            }
        });
        while !broker
            .permissions(None)
            .iter()
            .any(|request| request.id == "per_standing")
        {
            assert!(
                !asking.is_finished(),
                "a grant that cannot be recorded atomically must not decide the call"
            );
            tokio::task::yield_now().await;
        }
        let resolution = broker
            .claim_permission("ses_http", "per_standing")
            .expect("the projected prompt is claimable");
        assert!(
            resolution.answer.is_some(),
            "the projected prompt must belong to a waiting asker, not to a row a \
             non-atomic grant created and then resolved on its own"
        );
        resolution
            .settle(ReplyKind::Once)
            .await
            .expect("the human's allow settles");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), &mut asking)
                .await
                .expect("the answer releases the asker")
                .expect("permission asker task does not panic"),
            ReplyKind::Once
        );
    }

    /// A reconnect inside the window leaves the next disconnect its whole window.
    ///
    /// The first stream's timer keeps its original deadline, so without a registration
    /// epoch it fires *inside* the second stream's window and cancels the requests it
    /// captured: a user who reloads twice in one window gets less grace than a user who
    /// reloads once.
    #[tokio::test]
    async fn a_reconnect_does_not_shorten_the_next_disconnects_grace() {
        const GRACE: Duration = Duration::from_millis(500);
        let broker = RequestBroker::default().with_observer_grace(GRACE);
        let mut asking = tokio::spawn({
            let broker = broker.clone();
            async move { broker.ask_permission(ask("per_grace", Vec::new())).await }
        });
        while broker.permissions(None).is_empty() {
            assert!(!asking.is_finished(), "the ask must reach a human");
            tokio::task::yield_now().await;
        }

        drop(broker.observe_session("ses_http"));
        tokio::time::sleep(GRACE / 2).await;
        // The reload: a second stream registers and goes away, which is the window the
        // user is owed from here.
        drop(broker.observe_session("ses_http"));
        tokio::time::sleep(GRACE - GRACE / 4).await;

        assert!(
            !asking.is_finished(),
            "the first stream's timer cancelled the ask inside the second stream's grace \
             window, so reloading twice bought less time than reloading once"
        );
        assert_eq!(
            tokio::time::timeout(GRACE * 8, &mut asking)
                .await
                .expect("the second window still ends in a cancellation")
                .expect("permission asker task does not panic"),
            ReplyKind::Reject,
            "an unobserved request is still cancelled once its own window ends"
        );
    }

    /// Holds the database's write lock for `held`, reporting when it is taken and freed.
    fn hold_the_write_lock(
        pool: &Arc<zuno_db::Pool>,
        held: Duration,
    ) -> (
        std::thread::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let holding = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = std::thread::spawn({
            let pool = Arc::clone(pool);
            let holding = Arc::clone(&holding);
            let released = Arc::clone(&released);
            move || {
                let outcome = pool.transaction(|_transaction| {
                    holding.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(held);
                    released.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                });
                outcome.expect("the blocking writer commits");
            }
        });
        (worker, holding, released)
    }

    /// A permission reply's durable write does not stop the reactor while it waits.
    ///
    /// This is the path a user hits on every approval, and it used to run synchronous
    /// rusqlite straight from the async HTTP handler: on the single-threaded `zuno serve`
    /// runtime one contended reply write froze every SSE stream and every live turn.
    #[tokio::test(flavor = "current_thread")]
    async fn a_permission_replys_durable_write_leaves_the_reactor_running() {
        let (_spill, pool, _goals, broker) = durable_broker_on_disk();
        let asking = tokio::spawn({
            let broker = broker.clone();
            async move { broker.ask_permission(ask("per_reply", Vec::new())).await }
        });
        let resolution = loop {
            if let Some(resolution) = broker.claim_permission("ses_http", "per_reply") {
                break resolution;
            }
            assert!(!asking.is_finished(), "the ask must reach a human");
            tokio::task::yield_now().await;
        };

        let (blocker, holding, released) = hold_the_write_lock(&pool, Duration::from_millis(300));
        while !holding.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let mut settling = std::pin::pin!(resolution.settle(ReplyKind::Once));
        // One poll queues the write; the count below is how much of the process keeps
        // running while it waits for the lock.
        let _started = futures::poll!(&mut settling);
        let mut polls = 0_u32;
        while !released.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
            polls += 1;
        }
        assert!(
            polls > 100,
            "the reactor stalled for the reply's durable write: {polls} polls while the \
             database lock was held"
        );

        blocker.join().expect("the blocking writer finishes");
        settling
            .await
            .expect("the reply settles once the lock frees");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), asking)
                .await
                .expect("the reply releases the asker")
                .expect("permission asker task does not panic"),
            ReplyKind::Once
        );
    }

    /// A question's durable write does not stop the reactor while it waits.
    ///
    /// `ask_question` used to call the synchronous store on the reactor, so on the
    /// single-threaded `zuno serve` runtime a write waiting on the database lock froze
    /// every SSE stream and every live turn. The other thread here holds the write lock
    /// the ask needs; the assertion is that this task keeps being polled while it does.
    #[tokio::test(flavor = "current_thread")]
    async fn a_questions_durable_write_leaves_the_reactor_running() {
        let (_spill, pool, _goals, broker) = durable_broker_on_disk();
        let (blocker, holding, released) = hold_the_write_lock(&pool, Duration::from_millis(300));
        while !holding.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let asking = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .ask_question(QuestionRequest {
                        id: "que_blocked".to_owned(),
                        session_id: "ses_http".to_owned(),
                        questions: vec![json!({"question": "Which channel?"})],
                        tool: None,
                    })
                    .await
            }
        });

        // A write on the reactor yields this loop one pass — the one that discovers the
        // lock was already released. An off-reactor write yields it thousands.
        let mut polls = 0_u32;
        while !released.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
            polls += 1;
        }
        assert!(
            polls > 100,
            "the reactor stalled for the question's durable write: {polls} polls while the \
             database lock was held"
        );

        blocker.join().expect("the blocking writer finishes");
        loop {
            if let Some(resolution) = broker.claim_question("ses_http", "que_blocked") {
                resolution
                    .settle(QuestionDecision::Cancelled)
                    .await
                    .expect("the cancellation settles");
                break;
            }
            assert!(!asking.is_finished(), "the question must reach a human");
            tokio::task::yield_now().await;
        }
        assert!(
            matches!(
                tokio::time::timeout(Duration::from_secs(5), asking)
                    .await
                    .expect("the answer releases the asker")
                    .expect("question asker task does not panic"),
                QuestionDecision::Cancelled
            ),
            "the decision the human gave is the one the tool sees"
        );
    }

    #[tokio::test]
    async fn recovered_http_requests_settle_the_same_durable_rows_and_resume_the_goal() {
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
        broker
            .claim_question("ses_http", "que_http")
            .expect("claim recovered question")
            .settle(QuestionDecision::Answered(vec![vec!["canary".to_owned()]]))
            .await
            .expect("the answer settles the recovered question");
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
        broker
            .claim_permission("ses_http", "per_http")
            .expect("claim recovered permission")
            .settle(ReplyKind::Once)
            .await
            .expect("the reply settles the recovered permission");
        assert_eq!(
            zuno_db::inbox::SessionInbox::new(pool)
                .pending("ses_http")
                .expect("pending input")
                .len(),
            2
        );
    }

    /// A reply that lost a cross-process race must not report itself as the decision.
    ///
    /// The store-only wiring settles in two transactions — read, then update — so a
    /// second writer on the same `zuno.db` can answer the row in between.
    /// `resolve_in` updates `WHERE state = 'pending'` and re-reads the row regardless,
    /// so the loser is handed the winner's already-answered row. Checked against the
    /// requested state alone that reads as success, and this reply's `always` would then
    /// be saved as a standing grant for a decision the database does not carry.
    ///
    /// The race is made deterministic by the database's own write lock rather than by
    /// thread timing: the winner answers the row inside a held `BEGIN IMMEDIATE`, so the
    /// loser's guard read sees `pending` on its WAL snapshot and its update parks until
    /// the winner commits.
    #[test]
    fn a_reply_that_lost_a_cross_process_race_reports_no_resolution() {
        let (_spill, pool, _goals, _broker) = durable_broker_on_disk();
        let store = HumanRequestStore::new(Arc::clone(&pool));
        store
            .create(
                new_permission_row(&ask("per_raced", vec!["shell".to_owned()]))
                    .expect("the row serializes"),
            )
            .expect("the pending permission row is created");

        let winner = json!({"reply": "reject"});
        let loser_reply = json!({"reply": "always"});
        let blocker = pool
            .get()
            .expect("a second connection to the same database");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the winner takes the write lock");
        blocker
            .execute(
                "UPDATE human_request \
                 SET state = 'answered', response = ?1, revision = revision + 1, \
                     time_updated = 2, time_resolved = 2 \
                 WHERE id = 'per_raced' AND state = 'pending'",
                rusqlite::params![winner.to_string()],
            )
            .expect("the winning reply answers the row");

        let losing = std::thread::spawn({
            let store = store.clone();
            let loser_reply = loser_reply.clone();
            move || {
                resolve_if_pending(
                    &store,
                    "per_raced",
                    HumanRequestState::Answered,
                    Some(&loser_reply),
                )
            }
        });
        // Long enough for the loser to read its snapshot and park on the write lock, and
        // well inside the pool's 5 s `busy_timeout`.
        std::thread::sleep(Duration::from_millis(200));
        blocker
            .execute_batch("COMMIT")
            .expect("the winning reply commits");
        drop(blocker);

        let resolved = losing
            .join()
            .expect("the losing reply finishes")
            .expect("a lost race is not a database failure");
        assert!(
            !resolved,
            "a reply whose update changed no row must not report a resolution, or its \
             `always` is saved as a standing grant for a decision that never landed"
        );
        let row = store
            .get("per_raced")
            .expect("the row reads")
            .expect("the row is still there");
        assert_eq!(
            row.response.as_ref(),
            Some(&winner),
            "the winner's decision is the one the database keeps"
        );
    }
}
