//! Session-owned durable question presentation and idle inbox recovery.
//!
//! Subscriptions are installed before replay. Each native form is shown once per
//! open-session lifetime: a defer or partial answer remains available through the
//! list/respond API and must not immediately reopen the same modal. Restarted
//! sessions reconstruct pending forms from the port. Automatic input delivery
//! checks the shared scheduling gate and never manufactures an explicit resume.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use zuno_acp::{AcpQuestionPresenter, AcpSessionRoute, ClientConnection, RpcError};
use zuno_db::inbox::{SessionInbox, SessionInput};
use zuno_session_control::QuestionService;
use zuno_tool::question::{QuestionError, QuestionPort};
use zuno_types::execution::WakeAdmission;
use zuno_types::question::{QuestionCommand, QuestionReceipt, QuestionView};

use super::{AcpDurableInput, AcpSession, AcpState, DurableInputScope, ProductionAcpAgent};

pub(super) const LIST_METHOD: &str = "questions/list";
pub(super) const RESPOND_METHOD: &str = "questions/respond";
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

pub(super) fn capabilities() -> Value {
    json!({
        "version": 1,
        "listMethod": LIST_METHOD,
        "respondMethod": RESPOND_METHOD,
        "requiresExpectedRevision": true,
        "supportsPartialAnswers": true,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RespondParams {
    session_id: String,
    request_id: String,
    command: QuestionCommand,
}

pub(super) async fn list(port: &dyn QuestionPort, params: &Value) -> Result<Value, RpcError> {
    let params: ListParams = serde_json::from_value(params.clone())
        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    require_id(&params.session_id, "sessionId")?;
    let questions = port
        .pending(&params.session_id)
        .await
        .map_err(question_error)?;
    Ok(json!({"questions": questions}))
}

pub(super) async fn respond(port: &dyn QuestionPort, params: &Value) -> Result<Value, RpcError> {
    let params: RespondParams = serde_json::from_value(params.clone())
        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    require_id(&params.session_id, "sessionId")?;
    require_id(&params.request_id, "requestId")?;
    params
        .command
        .validate()
        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    let receipt = port
        .apply(&params.session_id, &params.request_id, params.command)
        .await
        .map_err(question_error)?;
    serde_json::to_value(receipt).map_err(|error| RpcError::internal(error.to_string()))
}

fn require_id(value: &str, field: &str) -> Result<(), RpcError> {
    if value.trim().is_empty() {
        Err(RpcError::invalid_params(format!(
            "{field} must be non-empty"
        )))
    } else {
        Ok(())
    }
}

fn question_error(error: QuestionError) -> RpcError {
    let detail = match &error {
        QuestionError::Conflict {
            request_id,
            expected,
            actual,
        } => Some(json!({
            "kind": "question_revision_conflict",
            "requestId": request_id, "expectedRevision": expected, "actualRevision": actual,
        })),
        QuestionError::CommandConflict { command_id } => Some(json!({
            "kind": "question_command_conflict", "commandId": command_id,
        })),
        QuestionError::Closed { request_id, state } => Some(json!({
            "kind": "question_closed", "requestId": request_id, "state": state,
        })),
        _ => None,
    };
    if let Some(detail) = detail {
        return RpcError {
            code: -32003,
            message: error.to_string(),
            data: Some(detail),
        };
    }
    match error {
        QuestionError::Invalid(_)
        | QuestionError::NotFound { .. }
        | QuestionError::Rejected { .. } => RpcError::invalid_params(error.to_string()),
        _ => RpcError::internal(error.to_string()),
    }
}

impl ProductionAcpAgent {
    /// Native subagent sessions share the root connection's question port.
    pub(super) async fn question_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<AcpSession>, RpcError> {
        let mut current = session_id.to_owned();
        let mut seen = BTreeSet::new();
        while seen.insert(current.clone()) {
            if let Some(session) = self.state.registry.get(&current).await {
                if session.closed.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(session.closed_error());
                }
                return Ok(session);
            }
            let stored = zuno_db::session::Store::new(self.state.question_pool.as_ref())
                .get(&current)
                .map_err(|error| super::map_session_lookup(&current, error))?;
            let Some(parent) = stored.parent_id else {
                break;
            };
            current = parent;
        }
        Err(RpcError::invalid_params(format!(
            "session {session_id} is not open in this ACP connection"
        )))
    }
}

/// One bounded scan per wake. Rejected rows stay untouched and cannot make the
/// caller spin by repeatedly selecting the same ineligible head row.
pub(super) fn next_input(
    inbox: &SessionInbox,
    session_id: &str,
    scope: DurableInputScope,
) -> Result<Option<(SessionInput, AcpDurableInput)>, RpcError> {
    for input in inbox.pending(session_id).map_err(db_error)? {
        let Some(drivable) = scope.admits(&input) else {
            continue;
        };
        if inbox.wake_admission(&input).map_err(db_error)? != WakeAdmission::Reject {
            return Ok(Some((input, drivable)));
        }
    }
    Ok(None)
}

pub(super) fn next_input_scope(
    inbox: &SessionInbox,
    session_id: &str,
) -> Result<Option<DurableInputScope>, RpcError> {
    for scope in [
        DurableInputScope::Controls,
        DurableInputScope::Automatic,
        DurableInputScope::Prompts,
    ] {
        if next_input(inbox, session_id, scope)?.is_some() {
            return Ok(Some(scope));
        }
    }
    Ok(None)
}

fn db_error(error: zuno_error::DbError) -> RpcError {
    RpcError::internal(error.to_string())
}

#[derive(Default)]
pub(super) struct PresentationLedger {
    seen: BTreeSet<String>,
}

impl PresentationLedger {
    pub(super) fn claim(&mut self, view: &QuestionView) -> bool {
        !view.state.is_terminal() && self.seen.insert(view.id.clone())
    }
}

pub(super) struct SessionQuestions {
    tasks: Vec<JoinHandle<()>>,
    input_pump: Option<JoinHandle<()>>,
}

impl SessionQuestions {
    pub(super) fn start(
        session: &Arc<AcpSession>,
        state: &Arc<AcpState>,
        client: ClientConnection,
    ) -> Self {
        let mut tasks = Vec::new();
        let root = session.id.clone();
        let service = Arc::clone(&session.questions);
        let client = client.session_scoped();
        if state
            .elicitation_form
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let changes = service.subscribe();
            let route = Arc::new(AcpSessionRoute::new(
                state
                    .native_subagents
                    .load(std::sync::atomic::Ordering::Acquire),
            ));
            route
                .bind_root(&root)
                .expect("open ACP session has a stable root ID");
            let presenter =
                AcpQuestionPresenter::new(service.clone(), client.clone()).with_route(route);
            tasks.push(tokio::spawn(present_pending(
                Arc::clone(&state.question_pool),
                root,
                service.clone(),
                changes,
                presenter,
            )));
        }
        // Install the receiver before scanning, so a commit racing startup is
        // either in replay or buffered in this subscription.
        let changes = service.subscribe();
        let input_pump = tokio::spawn(pump_inputs(
            Arc::downgrade(session),
            Arc::downgrade(state),
            client,
            changes,
        ));
        Self {
            tasks,
            input_pump: Some(input_pump),
        }
    }

    pub(super) fn is_finished(&self) -> bool {
        self.tasks.iter().all(JoinHandle::is_finished)
            && self.input_pump.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) async fn shutdown(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "ACP question session task failed");
            }
        }
        // The session already closed admission and signalled native cancellation.
        // Unlike presentation, the pump may own an accepted logical turn. Let its
        // host finish receipt settlement before dropping the drive future.
        if let Some(task) = self.input_pump.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "ACP native input pump failed during shutdown");
        }
    }
}

impl Drop for SessionQuestions {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some(task) = &self.input_pump {
            task.abort();
        }
    }
}

async fn pending_subtree(
    pool: &zuno_db::Pool,
    service: &dyn QuestionPort,
    root: &str,
) -> Result<Vec<QuestionView>, RpcError> {
    let sessions = {
        let connection = pool.get().map_err(db_error)?;
        zuno_db::session::subtree(&connection, root).map_err(db_error)?
    };
    let mut pending = Vec::new();
    for session in sessions {
        pending.extend(service.pending(&session).await.map_err(question_error)?);
    }
    Ok(pending)
}

async fn present_pending(
    pool: Arc<zuno_db::Pool>,
    root: String,
    service: Arc<QuestionService>,
    mut changes: broadcast::Receiver<QuestionReceipt>,
    presenter: AcpQuestionPresenter,
) {
    let mut seen = PresentationLedger::default();
    let mut active: BTreeMap<String, (i64, AbortHandle)> = BTreeMap::new();
    let mut tasks = JoinSet::new();
    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reconcile.tick().await;
    loop {
        match pending_subtree(&pool, service.as_ref(), &root).await {
            Ok(pending) => {
                // Superseded forms cannot apply a stale reply. A defer remains
                // seen even though its new revision is still pending.
                active.retain(|id, (revision, task)| {
                    let current = pending
                        .iter()
                        .any(|view| &view.id == id && view.revision == *revision);
                    if !current {
                        task.abort();
                    }
                    current
                });
                for view in pending {
                    if !seen.claim(&view) {
                        continue;
                    }
                    let id = view.id.clone();
                    let active_id = id.clone();
                    let revision = view.revision;
                    let presenter = presenter.clone();
                    let task = tasks.spawn(async move { (id, presenter.present(view).await) });
                    active.insert(active_id, (revision, task));
                }
            }
            Err(error) => {
                tracing::warn!(session_id = %root, %error, "ACP pending question replay deferred")
            }
        }
        tokio::select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                match result {
                    Some(Ok((id, result))) => {
                        active.remove(&id);
                        if let Err(error) = result {
                            tracing::warn!(request_id = %id, %error, "ACP question response was not applied");
                        }
                    }
                    Some(Err(error)) => {
                        active.retain(|_, (_, handle)| handle.id() != error.id());
                        if !error.is_cancelled() {
                            tracing::warn!(%error, "ACP question presentation failed");
                        }
                    }
                    None => {}
                }
            }
            changed = changes.recv() => {
                if matches!(changed, Err(broadcast::error::RecvError::Closed)) { break; }
            }
            _ = reconcile.tick() => {}
        }
    }
}

async fn pump_inputs(
    session: Weak<AcpSession>,
    state: Weak<AcpState>,
    client: ClientConnection,
    mut changes: broadcast::Receiver<QuestionReceipt>,
) {
    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reconcile.tick().await;
    loop {
        {
            let (Some(session), Some(state)) = (session.upgrade(), state.upgrade()) else {
                break;
            };
            if session.closed.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            if let Err(error) = session.pump_pending_input(state.as_ref(), &client).await {
                tracing::warn!(session_id = %session.id, %error, "ACP pending input remains durable");
            }
            if session.closed.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
        }
        // Even Reject and failed delivery park here. Neither is an instruction
        // to retry the same durable row in a tight loop.
        tokio::select! {
            changed = changes.recv() => {
                if matches!(changed, Err(broadcast::error::RecvError::Closed)) { break; }
            }
            _ = reconcile.tick() => {}
        }
    }
}

impl AcpSession {
    async fn pump_pending_input(
        &self,
        state: &AcpState,
        client: &ClientConnection,
    ) -> Result<(), RpcError> {
        if self.has_native_driver_in_flight() {
            return Ok(());
        }
        let inbox = SessionInbox::new(Arc::clone(&state.question_pool));
        let Some(scope) = next_input_scope(&inbox, &self.id)? else {
            return Ok(());
        };
        self.ensure_active(state, client.clone()).await?;
        if scope == DurableInputScope::Controls {
            let configuration = self
                .reconfigure_from_prompt(
                    super::SessionReconfiguration::Mode("build".to_owned()),
                    state,
                    client.clone(),
                )
                .await?;
            super::publish_configuration_updates(client, &self.id, &configuration).await?;
        }
        if self.has_native_driver_in_flight() {
            return Ok(());
        }
        let Ok(guard) = self.runs.begin_turn(self.id.clone()) else {
            return Ok(());
        };
        let next = self.drive_next_durable_input(client, &guard, scope).await?;
        drop(guard);
        if let Some((driven, projected)) = next {
            self.settle_turn(driven, projected, false, client).await?;
        }
        Ok(())
    }
}
