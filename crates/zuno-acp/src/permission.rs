use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;
use zuno_db::human_request::{
    HumanRequestKind, HumanRequestState, HumanRequestStore, NewHumanRequest,
};
use zuno_error::ToolError;
use zuno_goal::GoalStore;
use zuno_permission::{PermissionRequest, ReplyKind};
use zuno_tool::{PermissionAsk, PermissionAsker, PermissionOrigin};

use crate::ClientConnection;
use crate::settlement::Settlement;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PermissionGrant {
    session_id: String,
    permission: String,
    patterns: Vec<String>,
}

impl PermissionGrant {
    fn new(session_id: &str, permission: &str, patterns: &[String]) -> Self {
        Self {
            session_id: session_id.to_owned(),
            permission: permission.to_owned(),
            patterns: patterns.to_vec(),
        }
    }
}

/// Process-local ACP grants that live until their ACP session is closed.
#[derive(Debug, Default)]
pub struct AcpPermissionGrants {
    standing: Mutex<BTreeSet<PermissionGrant>>,
}

impl AcpPermissionGrants {
    fn allows(&self, grant: &PermissionGrant) -> bool {
        locked(&self.standing).contains(grant)
    }

    fn remember(&self, grant: PermissionGrant) {
        locked(&self.standing).insert(grant);
    }

    /// Remove every standing grant owned by one closed ACP session.
    pub fn clear_session(&self, session_id: &str) {
        locked(&self.standing).retain(|grant| grant.session_id != session_id);
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct AcpPermissionAsker {
    client: ClientConnection,
    title: String,
    grants: Arc<AcpPermissionGrants>,
    route: Option<Arc<crate::AcpSessionRoute>>,
    durable: Arc<Mutex<Option<DurablePermissions>>>,
}

#[derive(Debug, Clone)]
struct DurablePermissions {
    store: HumanRequestStore,
    goals: Arc<GoalStore>,
}

impl AcpPermissionAsker {
    #[must_use]
    pub fn new(client: ClientConnection, title: impl Into<String>) -> Self {
        Self::with_grants(client, title, Arc::new(AcpPermissionGrants::default()))
    }

    #[must_use]
    pub fn with_grants(
        client: ClientConnection,
        title: impl Into<String>,
        grants: Arc<AcpPermissionGrants>,
    ) -> Self {
        Self {
            client,
            title: title.into(),
            grants,
            route: None,
            durable: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn with_grants_and_route(
        client: ClientConnection,
        title: impl Into<String>,
        grants: Arc<AcpPermissionGrants>,
        route: Arc<crate::AcpSessionRoute>,
    ) -> Self {
        Self {
            client,
            title: title.into(),
            grants,
            route: Some(route),
            durable: Arc::new(Mutex::new(None)),
        }
    }

    pub fn attach_durable(&self, store: HumanRequestStore, goals: Arc<GoalStore>) {
        *locked(&self.durable) = Some(DurablePermissions { store, goals });
    }

    /// Re-present a permission request whose original process disappeared.
    ///
    /// The decision becomes durable input; the abandoned side-effecting call is
    /// not mechanically replayed.
    pub async fn answer_pending(&self, request_id: &str) -> Result<bool, ToolError> {
        let durable = locked(&self.durable)
            .clone()
            .ok_or_else(|| permission_failure("durable permission store is not attached"))?;
        // A row another surface settled between the caller's `pending()` read and this
        // call is not this surface's work. Reporting it as a failure would turn a
        // benign cross-surface race into a failed `session/prompt`.
        let Some(request) = durable
            .store
            .get(request_id)
            .map_err(permission_store_failure)?
            .filter(|request| {
                request.kind == HumanRequestKind::Permission
                    && request.state == HumanRequestState::Pending
            })
        else {
            return Ok(false);
        };
        // A stored payload this build cannot decode is skipped, never settled and
        // never rewritten: the row is the only recovery evidence there is, another
        // surface may still present it, and returning the error instead would make
        // this one row fail every later `session/prompt` in the session.
        let Ok(permission) = serde_json::from_value::<PermissionRequest>(request.payload.clone())
        else {
            return Ok(false);
        };
        let reusable = !permission.always.is_empty();
        let routed = self.route.as_ref().map_or_else(
            || crate::RoutedSession::direct(&permission.session_id),
            |route| route.resolve(&permission.session_id),
        );
        // Same rule for a client that cannot be asked at all. Recovery owns a row it
        // did not create, so an undeliverable re-presentation leaves it pending and
        // answerable elsewhere instead of force-denying it or failing this prompt and
        // every prompt after it.
        let Ok(response) = self
            .client
            .request_permission(permission_payload(
                &routed,
                &self.title,
                &permission.permission,
                &permission,
                reusable,
            ))
            .await
        else {
            return Ok(false);
        };
        let reply = permission_reply(&response, reusable);
        let settled = self.settle(&durable, request_id, reply, Settlement::DurableInput)?;
        if reply.resolution == PermissionResolution::AllowedSession {
            self.grants.remember(PermissionGrant::new(
                routed.grant_session_id(),
                &permission.permission,
                &permission.patterns,
            ));
        }
        Ok(settled)
    }

    /// Record one settled dialog through the crate's single settlement rule.
    ///
    /// The rule — every terminal outcome resolves `answered` and resumes whatever
    /// Goal the settled row itself carries — lives in [`crate::settlement::settle`],
    /// shared with [`crate::AcpQuestionAsker`]. This wrapper only chooses the durable
    /// response and labels a failure with this tool.
    fn settle(
        &self,
        durable: &DurablePermissions,
        request_id: &str,
        reply: PermissionReply,
        settlement: Settlement,
    ) -> Result<bool, ToolError> {
        let settled = crate::settlement::settle(
            &durable.store,
            &durable.goals,
            request_id,
            settled_response(reply),
            settlement,
        )
        .map_err(|source| ToolError::Failed {
            tool: String::from("permission"),
            source,
        })?;
        Ok(settled.is_some())
    }

    fn persist(&self, request: &PermissionRequest) -> Result<(), ToolError> {
        let Some(durable) = locked(&self.durable).clone() else {
            return Ok(());
        };
        let payload =
            serde_json::to_value(request).map_err(|error| permission_failure(error.to_string()))?;
        if durable
            .goals
            .request_permission(
                &request.session_id,
                request.id.clone(),
                payload.clone(),
                request.tool.as_ref().map(|tool| tool.message_id.clone()),
                request.tool.as_ref().map(|tool| tool.call_id.clone()),
            )
            .map_err(|error| permission_failure(error.to_string()))?
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
            .map_err(permission_store_failure)
    }
}

#[async_trait]
impl PermissionAsker for AcpPermissionAsker {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        let routed = self.route.as_ref().map_or_else(
            || crate::RoutedSession::direct(origin.session_id()),
            |route| route.resolve(origin.session_id()),
        );
        let reusable = !ask.manual && !ask.always.is_empty();
        let grant = PermissionGrant::new(
            routed.grant_session_id(),
            &ask.permission,
            ask.patterns.as_slice(),
        );
        if reusable && self.grants.allows(&grant) {
            return Ok(());
        }
        let request_id = format!("per_{}", Uuid::new_v4().simple());
        let durable_request = origin.into_request(request_id.clone(), ask.clone());
        self.persist(&durable_request)?;
        let request = permission_payload(&routed, &self.title, tool, &durable_request, reusable);
        // A dialog that never reached the client is not a grant — but it must not
        // return before the row is settled either. `persist` may have paused the
        // active Goal on this request, so a row left pending here parks that Goal and
        // makes ACP recovery re-present a permission for a call already denied.
        let reply = match self.client.request_permission(request).await {
            Ok(response) => permission_reply(&response, reusable),
            Err(_) => PermissionReply::UNDELIVERABLE,
        };
        if let Some(durable) = locked(&self.durable).clone() {
            let _settled = self.settle(&durable, &request_id, reply, Settlement::ResolveOnly)?;
        }
        match reply.resolution {
            PermissionResolution::AllowedOnce => Ok(()),
            PermissionResolution::AllowedSession => {
                self.grants.remember(grant);
                Ok(())
            }
            PermissionResolution::Denied | PermissionResolution::Cancelled => {
                Err(ToolError::Denied {
                    tool: tool.to_owned(),
                })
            }
        }
    }
}

fn permission_payload(
    routed: &crate::RoutedSession,
    title: &str,
    tool: &str,
    request: &PermissionRequest,
    reusable: bool,
) -> Value {
    let mut payload = json!({
        "sessionId": routed.wire_session_id(),
        "toolCall": {
            "toolCallId": request
                .tool
                .as_ref()
                .map_or(request.id.as_str(), |tool| tool.call_id.as_str()),
            "title": title,
            "kind": tool_kind(tool),
            "status": "pending",
            "rawInput": {
                "permission": request.permission,
                "patterns": request.patterns,
                "metadata": request.metadata,
            },
        },
        "options": permission_options(reusable),
    });
    if let Some(child_session_id) = routed.child_session_id() {
        payload["_meta"] = json!({
            "zuno": {
                "childSessionId": child_session_id,
            },
        });
    }
    payload
}

/// How a client reply was reached, independent of what the reply authorizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionOutcome {
    /// The user chose one of the offered options.
    Selected,
    /// The client withdrew the dialog before the user chose.
    Cancelled,
    /// The client answered with something this agent does not recognize.
    Unrecognized,
    /// The request never reached the client.
    Undeliverable,
}

impl PermissionOutcome {
    /// Stable durable spelling.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Cancelled => "cancelled",
            Self::Unrecognized => "unrecognized",
            Self::Undeliverable => "undeliverable",
        }
    }
}

/// One reply, classified once: what it authorizes and how it was reached.
///
/// The two halves are decided together in [`permission_reply`], from one read of
/// the reply, because deriving them separately let the peer being audited choose
/// its own audit label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PermissionReply {
    resolution: PermissionResolution,
    outcome: PermissionOutcome,
}

impl PermissionReply {
    /// A dialog that never reached the client: no authority, and no reply to read.
    const UNDELIVERABLE: Self = Self {
        resolution: PermissionResolution::Denied,
        outcome: PermissionOutcome::Undeliverable,
    };

    /// A reply this agent could not match to anything it offered.
    const UNRECOGNIZED: Self = Self {
        resolution: PermissionResolution::Denied,
        outcome: PermissionOutcome::Unrecognized,
    };
}

/// Classify one client reply into what it authorizes and how it was reached.
///
/// Both halves come from the same match so they can never disagree, and
/// [`PermissionOutcome::Selected`] is earned only by naming an option this dialog
/// actually offered — decided by [`offered_resolution`] against Zuno's own option
/// list, never by a second string the peer supplies. A peer that echoes
/// `"selected"` with an option id that was never on screen (`allow_always`, the
/// option *kind*; `allow_session` on an ask that offered no standing grant; no
/// `optionId` at all) is recorded as `unrecognized`, because durable history must
/// not claim the user deliberately chose Reject when the user was never asked.
///
/// The authorization half still fails closed: every unmatched shape denies, and no
/// reply can widen an allow, because only the two ids Zuno sends as allow options
/// resolve to one.
fn permission_reply(response: &Value, allow_session: bool) -> PermissionReply {
    match (
        response.pointer("/outcome/outcome").and_then(Value::as_str),
        response
            .pointer("/outcome/optionId")
            .and_then(Value::as_str),
    ) {
        (Some("cancelled"), _) => PermissionReply {
            resolution: PermissionResolution::Cancelled,
            outcome: PermissionOutcome::Cancelled,
        },
        (Some("selected"), Some(option_id)) => offered_resolution(option_id, allow_session).map_or(
            PermissionReply::UNRECOGNIZED,
            |resolution| PermissionReply {
                resolution,
                outcome: PermissionOutcome::Selected,
            },
        ),
        _ => PermissionReply::UNRECOGNIZED,
    }
}

/// What one option id authorizes, or `None` when this dialog never offered it.
///
/// The single place an option id becomes authority. It is keyed on the same
/// `allow_session` flag [`permission_options`] uses to decide what to send, so an
/// option that was not on screen resolves to nothing at all rather than to a
/// silent denial that reads like a decision;
/// `permission_option_ids_resolve_exactly_when_they_are_offered` pins the two lists
/// to each other.
fn offered_resolution(option_id: &str, allow_session: bool) -> Option<PermissionResolution> {
    match option_id {
        "allow_once" => Some(PermissionResolution::AllowedOnce),
        "allow_session" if allow_session => Some(PermissionResolution::AllowedSession),
        "reject_once" => Some(PermissionResolution::Denied),
        _ => None,
    }
}

/// The durable response recorded for one settled dialog.
///
/// A decided dialog keeps the plain `{"reply": …}` shape every other surface
/// writes. Anything else carries the discriminator as well, so durable history
/// never claims a withdrawn, unrecognized, or undelivered dialog was a decision
/// the user made.
fn settled_response(reply: PermissionReply) -> Value {
    let kind = reply_kind(reply.resolution);
    match reply.outcome {
        PermissionOutcome::Selected => json!({"reply": kind}),
        PermissionOutcome::Cancelled
        | PermissionOutcome::Unrecognized
        | PermissionOutcome::Undeliverable => {
            json!({"reply": kind, "outcome": reply.outcome.as_str()})
        }
    }
}

fn reply_kind(resolution: PermissionResolution) -> ReplyKind {
    match resolution {
        PermissionResolution::AllowedOnce => ReplyKind::Once,
        PermissionResolution::AllowedSession => ReplyKind::Always,
        PermissionResolution::Denied | PermissionResolution::Cancelled => ReplyKind::Reject,
    }
}

fn permission_store_failure(error: zuno_error::DbError) -> ToolError {
    ToolError::Failed {
        tool: String::from("permission"),
        source: Box::new(error),
    }
}

fn permission_failure(message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        tool: String::from("permission"),
        source: Box::new(std::io::Error::other(message.into())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionResolution {
    AllowedOnce,
    AllowedSession,
    Denied,
    Cancelled,
}

fn permission_options(session_grant: bool) -> Vec<Value> {
    let mut options = vec![json!({
        "optionId": "allow_once",
        "name": "Allow once",
        "kind": "allow_once",
    })];
    if session_grant {
        options.push(json!({
            "optionId": "allow_session",
            "name": "Allow for session",
            "kind": "allow_always",
        }));
    }
    options.push(json!({
        "optionId": "reject_once",
        "name": "Reject",
        "kind": "reject_once",
    }));
    options
}

fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read" | "glob" => "read",
        "write" | "edit" | "apply_patch" => "edit",
        "delete" => "delete",
        "move" => "move",
        "grep" | "search" => "search",
        "shell" | "execute" => "execute",
        "fetch" | "webfetch" => "fetch",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::test_client::ScriptedClient;
    use zuno_tool::{NeverInterrupted, ToolContext};

    struct Durable {
        _spill: tempfile::TempDir,
        goals: Arc<GoalStore>,
        store: HumanRequestStore,
    }

    fn durable_store() -> Durable {
        let spill = tempfile::tempdir().expect("spill directory");
        let goals = Arc::new(
            GoalStore::open_memory(spill.path().to_path_buf()).expect("in-memory goal store"),
        );
        let store = goals.human_requests();
        Durable {
            _spill: spill,
            goals,
            store,
        }
    }

    /// Insert the `project` and `session` rows the durable inbox has a foreign key on.
    ///
    /// `answer_with_input` admits a `session_input` row in the same transaction, and
    /// that table references `session`. Without these rows a recovery test fails on
    /// the constraint instead of on the behaviour under test.
    fn materialize_session(goals: &GoalStore, session_id: &str) {
        let connection = goals.pool().get().expect("check out a connection");
        connection
            .execute(
                "INSERT OR IGNORE INTO project \
                 (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
                  time_updated,time_initialized,sandboxes,commands) \
                 VALUES ('acp-fixture','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
                (),
            )
            .expect("insert the fixture project");
        connection
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO session \
                     (id,project_id,slug,directory,title,version,time_created,time_updated) \
                     VALUES ('{session_id}','acp-fixture','{session_id}','/tmp',\
                             '{session_id}','test',1,1)"
                ),
                (),
            )
            .expect("insert the fixture session");
    }

    fn option_ids(always: bool) -> Vec<Value> {
        permission_options(always)
            .into_iter()
            .map(|option| option["optionId"].clone())
            .collect()
    }

    #[test]
    fn standing_permission_options_are_symmetric() {
        assert_eq!(
            option_ids(false),
            vec![json!("allow_once"), json!("reject_once")],
        );
        assert_eq!(
            option_ids(true),
            vec![
                json!("allow_once"),
                json!("allow_session"),
                json!("reject_once"),
            ],
        );
    }

    /// One reply, one classification: what it authorizes and how it was reached.
    #[test]
    fn permission_replies_fail_closed_and_name_how_they_were_reached() {
        let selected =
            |option: &str| json!({ "outcome": { "outcome": "selected", "optionId": option } });
        let decided = |resolution| PermissionReply {
            resolution,
            outcome: PermissionOutcome::Selected,
        };
        assert_eq!(
            permission_reply(&selected("allow_once"), false),
            decided(PermissionResolution::AllowedOnce)
        );
        assert_eq!(
            permission_reply(&selected("allow_session"), true),
            decided(PermissionResolution::AllowedSession)
        );
        assert_eq!(
            permission_reply(&selected("reject_once"), true),
            decided(PermissionResolution::Denied),
            "an offered reject is the one denial a user really chose"
        );
        assert_eq!(
            permission_reply(&json!({ "outcome": { "outcome": "cancelled" } }), true),
            PermissionReply {
                resolution: PermissionResolution::Cancelled,
                outcome: PermissionOutcome::Cancelled,
            }
        );
        // Every reply that names nothing this dialog offered denies, and is recorded
        // as the non-decision it is. `allow_always` is Zuno's own option *kind*
        // string, so a client sending it in place of the id is a plausible bug rather
        // than only an attack, and `allow_session` is not on screen at all unless the
        // ask offers a standing grant.
        for (reply, allow_session) in [
            (selected("allow_always"), true),
            (selected("allow_session"), false),
            (selected("reject_always"), true),
            (selected("unknown"), true),
            (json!({ "outcome": { "outcome": "selected" } }), true),
            (json!({}), true),
            (json!({ "outcome": null }), true),
            (
                json!({ "outcome": { "outcome": "SELECTED", "optionId": "allow_once" } }),
                true,
            ),
        ] {
            assert_eq!(
                permission_reply(&reply, allow_session),
                PermissionReply::UNRECOGNIZED,
                "neither a grant nor a decision: {reply}"
            );
        }
    }

    /// Every option this dialog sends resolves, and nothing else does.
    ///
    /// The `selected` label is honest only while the offered list and the resolving
    /// list agree. An option added to the dialog but not to `offered_resolution`
    /// would record a real choice as `unrecognized`; an id that resolves without
    /// being offered would let a peer mint a `selected` record for a choice no user
    /// ever saw.
    #[test]
    fn permission_option_ids_resolve_exactly_when_they_are_offered() {
        for allow_session in [false, true] {
            let offered = permission_options(allow_session)
                .into_iter()
                .map(|option| {
                    option["optionId"]
                        .as_str()
                        .expect("every option carries a string id")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            for option_id in [
                "allow_once",
                "allow_session",
                "reject_once",
                "allow_always",
                "reject_always",
                "ALLOW_ONCE",
                "",
            ] {
                assert_eq!(
                    offered_resolution(option_id, allow_session).is_some(),
                    offered.iter().any(|offered| offered == option_id),
                    "`{option_id}` must resolve exactly when it is offered \
                     (allow_session={allow_session}, offered={offered:?})"
                );
            }
        }
    }

    #[test]
    fn standing_grants_are_exact_and_cleared_with_their_session() {
        let grants = AcpPermissionGrants::default();
        let first = PermissionGrant::new("ses_first", "shell", &["git status".to_owned()]);
        let second = PermissionGrant::new("ses_second", "shell", &["git status".to_owned()]);
        grants.remember(first.clone());
        grants.remember(second.clone());

        assert!(grants.allows(&first));
        assert!(grants.allows(&second));
        assert!(!grants.allows(&PermissionGrant::new(
            "ses_first",
            "shell",
            &["git diff".to_owned()]
        )));

        grants.clear_session("ses_first");
        assert!(!grants.allows(&first));
        assert!(grants.allows(&second));
    }

    /// One presented dialog, driven through the real asker over a real connection.
    ///
    /// The generated `per_*` id is only observable while the dialog is open, so the
    /// responder records it; nothing is asserted on that task — a panic there would
    /// drop the waiter and surface as a bare `Denied` instead of the real message.
    struct Presented {
        durable: Durable,
        asker: Arc<AcpPermissionAsker>,
        client: crate::transport::test_client::ScriptedClient,
        session_id: &'static str,
    }

    impl Presented {
        fn new<F>(session_id: &'static str, respond: F) -> Self
        where
            F: Fn(&str, &Value) -> Result<Value, crate::RpcError> + Send + 'static,
        {
            let durable = durable_store();
            let client = ScriptedClient::new(respond);
            let asker = Arc::new(AcpPermissionAsker::new(client.connection(), "Approve"));
            asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
            Self {
                durable,
                asker,
                client,
                session_id,
            }
        }

        /// An active Goal, so `persist` takes the pausing path a live session takes.
        fn with_active_goal(self, session_id: &str) -> Self {
            self.durable
                .goals
                .create_goal(session_id, "ship the permission fix", None)
                .expect("create an active goal");
            self
        }

        async fn ask(&self, session_id: &str) -> Result<(), ToolError> {
            let context = ToolContext::new(
                session_id,
                "msg_permission",
                "call_permission",
                "build",
                Arc::clone(&self.asker) as Arc<dyn PermissionAsker>,
                Arc::new(NeverInterrupted),
            );
            context
                .ask("shell", PermissionAsk::new("shell", "git status"))
                .await
        }

        /// The one permission row this session holds, whatever state it settled in.
        ///
        /// `ask` generates the `per_*` id, and a settled row leaves `pending()`, so the
        /// id is found by session rather than named — read here, on the test thread,
        /// because a blocking store read inside the responder closure would occupy the
        /// current-thread runtime while the agent side awaits its own reply.
        fn settled(&self) -> zuno_db::human_request::HumanRequest {
            let connection = self
                .durable
                .goals
                .pool()
                .get()
                .expect("check out a connection");
            let request_id = connection
                .query_row(
                    "SELECT id FROM human_request WHERE session_id = ?1",
                    [self.session_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("exactly one permission row exists");
            drop(connection);
            self.durable
                .store
                .get(&request_id)
                .expect("read the settled permission")
                .expect("the row survives")
        }

        fn goal_status(&self, session_id: &str) -> zuno_goal::GoalStatus {
            self.durable
                .goals
                .goal(session_id)
                .expect("read the goal")
                .expect("the goal exists")
                .status
        }
    }

    /// A dismissed dialog denies the call and the Goal keeps running.
    ///
    /// `persist` paused the active Goal on this request, and `resume_for_work` lifts
    /// that pause only for an `answered` request. A withdrawn dialog is a decided
    /// outcome — the call is denied — so it must settle and resume; parking the Goal
    /// here would kill the session on the most ordinary user action there is.
    #[tokio::test]
    async fn a_dismissed_permission_dialog_denies_the_call_and_keeps_the_goal_running() {
        let presented = Presented::new("ses_cancel", |_method, _params| {
            Ok(json!({ "outcome": { "outcome": "cancelled" } }))
        })
        .with_active_goal("ses_cancel");

        let error = presented
            .ask("ses_cancel")
            .await
            .expect_err("a dismissed dialog cannot authorize the call");

        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        // Liveness first: this is the assertion the durable label exists to serve.
        assert_eq!(
            presented.goal_status("ses_cancel"),
            zuno_goal::GoalStatus::Active,
            "a dismissed dialog must not park the Goal"
        );
        assert_eq!(
            presented
                .durable
                .goals
                .pause_state("ses_cancel")
                .expect("read the pause state"),
            None,
            "the permission pause must be consumed"
        );
        assert!(
            presented
                .durable
                .store
                .pending(Some("ses_cancel"))
                .expect("read pending permissions")
                .is_empty()
        );
        let settled = presented.settled();
        assert_eq!(settled.state, HumanRequestState::Answered);
        assert_eq!(
            settled.response,
            Some(json!({"reply": ReplyKind::Reject, "outcome": "cancelled"})),
            "durable history must not claim the user chose Reject"
        );
        assert_eq!(
            presented.client.methods(),
            vec![String::from("session/request_permission")]
        );
    }

    /// A client that never answers `session/request_permission` at all.
    ///
    /// The exact input: a JSON-RPC error instead of a result — a client that does not
    /// implement the method, or rejects the params. The row must not outlive the call
    /// unsettled, or ACP recovery re-presents a permission for a denied call and
    /// `answer_pending` fails every later `session/prompt` in the session.
    #[tokio::test]
    async fn an_undeliverable_permission_dialog_settles_its_row_and_keeps_the_goal_running() {
        let presented = Presented::new("ses_undeliverable", |method, _params| {
            Err(crate::RpcError::method_not_found(method))
        })
        .with_active_goal("ses_undeliverable");

        let error = presented
            .ask("ses_undeliverable")
            .await
            .expect_err("a dialog that never reached the client is not a grant");

        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        assert!(
            presented
                .durable
                .store
                .pending(Some("ses_undeliverable"))
                .expect("read pending permissions")
                .is_empty(),
            "a row left pending here makes every later session/prompt fail on it"
        );
        assert_eq!(
            presented.goal_status("ses_undeliverable"),
            zuno_goal::GoalStatus::Active,
            "an undeliverable dialog must not park the Goal either"
        );
        let settled = presented.settled();
        assert_eq!(settled.state, HumanRequestState::Answered);
        assert_eq!(
            settled.response,
            Some(json!({"reply": ReplyKind::Reject, "outcome": "undeliverable"}))
        );
    }

    /// A reply this agent cannot read is recorded as what it is, not as a decision.
    #[tokio::test]
    async fn an_unreadable_permission_reply_is_recorded_as_unrecognized() {
        let presented = Presented::new("ses_unknown", |_method, _params| Ok(json!({})))
            .with_active_goal("ses_unknown");

        let error = presented
            .ask("ses_unknown")
            .await
            .expect_err("a reply this agent cannot read fails closed");

        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        assert_eq!(
            presented.settled().response,
            Some(json!({"reply": ReplyKind::Reject, "outcome": "unrecognized"}))
        );
        assert_eq!(
            presented.goal_status("ses_unknown"),
            zuno_goal::GoalStatus::Active
        );
    }

    /// A decided rejection keeps the plain reply shape every other surface writes.
    #[tokio::test]
    async fn a_rejected_permission_is_recorded_as_answered_with_a_reject_reply() {
        let presented = Presented::new("ses_reject", |_method, _params| {
            Ok(json!({ "outcome": { "outcome": "selected", "optionId": "reject_once" } }))
        })
        .with_active_goal("ses_reject");

        let error = presented
            .ask("ses_reject")
            .await
            .expect_err("a rejected call is denied");

        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        let settled = presented.settled();
        assert_eq!(settled.state, HumanRequestState::Answered);
        assert_eq!(settled.response, Some(json!({"reply": ReplyKind::Reject})));
        assert_eq!(
            presented.goal_status("ses_reject"),
            zuno_goal::GoalStatus::Active,
            "a denial is a decision, so the Goal continues and reports it"
        );
    }

    fn stored_permission(session_id: &str, request_id: &str) -> PermissionRequest {
        PermissionRequest {
            id: request_id.to_owned(),
            session_id: session_id.to_owned(),
            permission: String::from("shell"),
            patterns: vec![String::from("git push")],
            metadata: serde_json::Map::new(),
            always: Vec::new(),
            tool: None,
        }
    }

    /// A pending permission row against a paused Goal, as a dead process left it.
    fn abandoned_permission(durable: &Durable, session_id: &str, payload: Value) -> String {
        materialize_session(&durable.goals, session_id);
        durable
            .goals
            .create_goal(session_id, "recover an abandoned permission", None)
            .expect("create an active goal");
        durable
            .goals
            .request_permission(
                session_id,
                String::from("per_abandoned"),
                payload,
                None,
                None,
            )
            .expect("persist the permission")
            .expect("an active goal pauses")
            .id
    }

    /// A stored payload this build cannot decode is skipped, not settled.
    ///
    /// Returning the error here made one row fail every later `session/prompt`.
    /// Rewriting the row instead would drop it out of `pending`, so the TUI and the
    /// HTTP broker could no longer answer a request the user is still waiting on.
    #[tokio::test]
    async fn an_undecodable_stored_permission_is_skipped_not_destroyed() {
        let durable = durable_store();
        let client = ScriptedClient::unreachable();
        let asker = AcpPermissionAsker::new(client.connection(), "Approve");
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let payload = json!({"permission": "shell", "always": []});
        let request_id = abandoned_permission(&durable, "ses_undecodable", payload.clone());

        let answered = asker
            .answer_pending(&request_id)
            .await
            .expect("recovery must advance instead of failing the prompt");

        assert!(!answered);
        let stored = durable
            .store
            .get(&request_id)
            .expect("read the stored permission")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Pending);
        assert_eq!(stored.payload, payload, "the payload is recovery evidence");
        assert_eq!(stored.response, None);
        assert!(
            client.methods().is_empty(),
            "a payload that cannot be decoded is never presented: {:?}",
            client.methods()
        );
    }

    /// Recovery that cannot reach the client leaves the row answerable elsewhere.
    #[tokio::test]
    async fn an_undeliverable_recovery_leaves_the_permission_pending() {
        let durable = durable_store();
        let client = ScriptedClient::unreachable();
        let asker = AcpPermissionAsker::new(client.connection(), "Approve");
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let payload = serde_json::to_value(stored_permission("ses_recover", "per_abandoned"))
            .expect("serialize the stored permission");
        let request_id = abandoned_permission(&durable, "ses_recover", payload);

        let answered = asker
            .answer_pending(&request_id)
            .await
            .expect("recovery must advance instead of failing the prompt");

        assert!(!answered);
        assert_eq!(
            durable
                .store
                .get(&request_id)
                .expect("read the stored permission")
                .expect("the row survives")
                .state,
            HumanRequestState::Pending,
            "a client that cannot present it must not force-deny it"
        );
        assert_eq!(
            client.methods(),
            vec![String::from("session/request_permission")]
        );
        assert_eq!(
            durable
                .store
                .pending(Some("ses_recover"))
                .expect("read pending permissions")
                .len(),
            1
        );
    }

    /// A row the TUI already answered is skipped, not reported as a failure.
    ///
    /// `recover_pending_human_requests` reads `pending()` and then calls this; any
    /// other surface may settle the row in between. Failing here turned that benign
    /// race into a `-32603` on the user's `session/prompt`, and would have overwritten
    /// the answer the user actually gave.
    #[tokio::test]
    async fn a_permission_another_surface_already_answered_is_skipped() {
        let durable = durable_store();
        let client = ScriptedClient::unreachable();
        let asker = AcpPermissionAsker::new(client.connection(), "Approve");
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let payload = serde_json::to_value(stored_permission("ses_raced", "per_abandoned"))
            .expect("serialize the stored permission");
        let request_id = abandoned_permission(&durable, "ses_raced", payload);
        let elsewhere = json!({"reply": ReplyKind::Always});
        durable
            .store
            .answer_with_input(
                &request_id,
                elsewhere.clone(),
                zuno_db::message::now_millis(),
            )
            .expect("another surface answers the row")
            .expect("the pending row is settled there");

        let answered = asker
            .answer_pending(&request_id)
            .await
            .expect("a settled row is not this surface's work");

        assert!(!answered);
        let stored = durable
            .store
            .get(&request_id)
            .expect("read the stored permission")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Answered);
        assert_eq!(
            stored.response,
            Some(elsewhere),
            "the answer the user gave elsewhere must not be overwritten"
        );
        assert!(client.methods().is_empty(), "{:?}", client.methods());
    }

    /// A row the released build already resolved still reads, and is left alone.
    ///
    /// 0.6.6 recorded a dismissed or undelivered ACP permission as `cancelled`,
    /// `expired`, or `failed` with no response. Those rows are on disk now, so this
    /// build must read them, skip them, and never rewrite them: a read that refused
    /// would fail the user's `session/prompt` on a row from a previous release, and a
    /// repair pass would overwrite the only evidence of what happened.
    #[tokio::test]
    async fn a_permission_the_released_build_resolved_still_reads_and_is_skipped() {
        for (session_id, state) in [
            ("ses_old_cancelled", HumanRequestState::Cancelled),
            ("ses_old_expired", HumanRequestState::Expired),
            ("ses_old_failed", HumanRequestState::Failed),
        ] {
            let durable = durable_store();
            let client = ScriptedClient::unreachable();
            let asker = AcpPermissionAsker::new(client.connection(), "Approve");
            asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
            let payload = serde_json::to_value(stored_permission(session_id, "per_abandoned"))
                .expect("serialize the stored permission");
            let request_id = abandoned_permission(&durable, session_id, payload.clone());
            durable
                .store
                .resolve(&request_id, state, None, zuno_db::message::now_millis())
                .expect("the released build resolved this row")
                .expect("the pending row is resolved there");

            let answered = asker
                .answer_pending(&request_id)
                .await
                .expect("a row this build did not settle is not an error");

            assert!(!answered);
            let stored = durable
                .store
                .get(&request_id)
                .expect("read the stored permission")
                .expect("the row survives");
            assert_eq!(
                stored.state, state,
                "an already-settled row is not rewritten"
            );
            assert_eq!(stored.response, None, "{stored:?}");
            assert_eq!(stored.payload, payload, "the payload is untouched");
            assert!(client.methods().is_empty(), "{:?}", client.methods());
        }
    }

    /// A recovered decision settles the row, admits durable input, and resumes.
    #[tokio::test]
    async fn a_recovered_permission_decision_resumes_the_paused_goal() {
        let durable = durable_store();
        let client = ScriptedClient::new(|_method, _params| {
            Ok(json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }))
        });
        let asker = AcpPermissionAsker::new(client.connection(), "Approve");
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let payload = serde_json::to_value(stored_permission("ses_decided", "per_abandoned"))
            .expect("serialize the stored permission");
        let request_id = abandoned_permission(&durable, "ses_decided", payload);

        let answered = asker
            .answer_pending(&request_id)
            .await
            .expect("present the recovered permission");

        assert!(answered);
        let stored = durable
            .store
            .get(&request_id)
            .expect("read the stored permission")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Answered);
        assert_eq!(stored.response, Some(json!({"reply": ReplyKind::Once})));
        assert_eq!(
            durable
                .goals
                .goal("ses_decided")
                .expect("read the goal")
                .expect("the goal exists")
                .status,
            zuno_goal::GoalStatus::Active
        );
    }

    /// A withdrawn re-presentation is still a decision the Goal must continue past.
    #[tokio::test]
    async fn a_recovered_permission_dismissal_resumes_the_paused_goal() {
        let durable = durable_store();
        let client = ScriptedClient::new(|_method, _params| {
            Ok(json!({ "outcome": { "outcome": "cancelled" } }))
        });
        let asker = AcpPermissionAsker::new(client.connection(), "Approve");
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let payload = serde_json::to_value(stored_permission("ses_withdrawn", "per_abandoned"))
            .expect("serialize the stored permission");
        let request_id = abandoned_permission(&durable, "ses_withdrawn", payload);

        let answered = asker
            .answer_pending(&request_id)
            .await
            .expect("present the recovered permission");

        assert!(answered);
        let stored = durable
            .store
            .get(&request_id)
            .expect("read the stored permission")
            .expect("the row survives");
        assert_eq!(stored.state, HumanRequestState::Answered);
        assert_eq!(
            stored.response,
            Some(json!({"reply": ReplyKind::Reject, "outcome": "cancelled"}))
        );
        assert_eq!(
            durable
                .goals
                .goal("ses_withdrawn")
                .expect("read the goal")
                .expect("the goal exists")
                .status,
            zuno_goal::GoalStatus::Active,
            "a withdrawn re-presentation must not park the Goal"
        );
    }

    /// A `selected` envelope naming an option this dialog never offered.
    ///
    /// The exact inputs, each through the live `ask` path, which offers no standing
    /// grant: `allow_always` — Zuno's own option *kind* string, so a client sending it
    /// in place of the id is a plausible bug — then `allow_session`, which this ask
    /// never put on screen, then `selected` with no `optionId` at all. Each used to
    /// record a bare `{"reply":"reject"}`, durable history asserting a deliberate user
    /// Reject, because the discriminator was read from the same peer-supplied string
    /// that produced it.
    #[tokio::test]
    async fn a_selected_reply_naming_no_offered_option_is_not_recorded_as_a_decision() {
        for (session_id, reply) in [
            (
                "ses_kind_string",
                json!({ "outcome": { "outcome": "selected", "optionId": "allow_always" } }),
            ),
            (
                "ses_unoffered",
                json!({ "outcome": { "outcome": "selected", "optionId": "allow_session" } }),
            ),
            (
                "ses_no_option",
                json!({ "outcome": { "outcome": "selected" } }),
            ),
        ] {
            let presented = Presented::new(session_id, move |_method, _params| Ok(reply.clone()))
                .with_active_goal(session_id);

            let error = presented
                .ask(session_id)
                .await
                .expect_err("an option that was never offered cannot authorize the call");

            assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
            let settled = presented.settled();
            assert_eq!(settled.state, HumanRequestState::Answered);
            assert_eq!(
                settled.response,
                Some(json!({"reply": ReplyKind::Reject, "outcome": "unrecognized"})),
                "durable history must not claim the user chose Reject: {session_id}"
            );
            assert_eq!(
                presented.goal_status(session_id),
                zuno_goal::GoalStatus::Active
            );
        }
    }

    /// The resume is keyed on the row the decision was read from.
    ///
    /// The pause row, the Goal, and the inbox admission are all keyed on the durable
    /// `session_id` column, but the payload carries a session id of its own. Here the
    /// column says `ses_row` while the payload JSON says `ses_payload` — the
    /// divergence a migration, a cross-session route, an older format, or a
    /// hand-repaired row can produce. Resuming the payload's value left `ses_row`
    /// paused forever with its pause row intact: the same permanent park, reached
    /// through a payload field instead of an outcome string.
    #[tokio::test]
    async fn a_recovered_permission_resumes_the_session_its_row_names() {
        let durable = durable_store();
        let client = ScriptedClient::new(|_method, _params| {
            Ok(json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }))
        });
        let asker = AcpPermissionAsker::new(client.connection(), "Approve");
        asker.attach_durable(durable.store.clone(), Arc::clone(&durable.goals));
        let payload = serde_json::to_value(stored_permission("ses_payload", "per_abandoned"))
            .expect("serialize the stored permission");
        let request_id = abandoned_permission(&durable, "ses_row", payload);
        assert_eq!(
            durable
                .store
                .get(&request_id)
                .expect("read the stored permission")
                .expect("the row survives")
                .session_id,
            "ses_row",
            "the fixture must diverge from the payload for this to mean anything"
        );

        let answered = asker
            .answer_pending(&request_id)
            .await
            .expect("present the recovered permission");

        assert!(answered);
        assert_eq!(
            durable
                .goals
                .goal("ses_row")
                .expect("read the goal")
                .expect("the goal exists")
                .status,
            zuno_goal::GoalStatus::Active,
            "the pause belongs to the row's session, so the resume must too"
        );
        assert_eq!(
            durable
                .goals
                .pause_state("ses_row")
                .expect("read the pause state"),
            None,
            "the permission pause must be consumed"
        );
    }
}
