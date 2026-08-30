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
        let request = durable
            .store
            .get(request_id)
            .map_err(permission_store_failure)?
            .filter(|request| {
                request.kind == HumanRequestKind::Permission
                    && request.state == HumanRequestState::Pending
            })
            .ok_or_else(|| {
                permission_failure(format!("permission `{request_id}` is not pending"))
            })?;
        let permission = serde_json::from_value::<PermissionRequest>(request.payload.clone())
            .map_err(|error| permission_failure(error.to_string()))?;
        let reusable = !permission.always.is_empty();
        let routed = self.route.as_ref().map_or_else(
            || crate::RoutedSession::direct(&permission.session_id),
            |route| route.resolve(&permission.session_id),
        );
        let response = self
            .client
            .request_permission(permission_payload(
                &routed,
                &self.title,
                &permission.permission,
                &permission,
                reusable,
            ))
            .await
            .map_err(|_| permission_failure("ACP permission request failed"))?;
        let resolution = permission_resolution(&response, reusable);
        let now = zuno_db::message::now_millis();
        let answered = match resolution {
            PermissionResolution::Cancelled => durable
                .store
                .resolve(request_id, HumanRequestState::Cancelled, None, now)
                .map_err(permission_store_failure)?
                .is_some(),
            PermissionResolution::AllowedOnce
            | PermissionResolution::AllowedSession
            | PermissionResolution::Denied => {
                let reply = reply_kind(resolution);
                durable
                    .store
                    .answer_with_input(request_id, json!({"reply": reply}), now)
                    .map_err(permission_store_failure)?
                    .is_some()
            }
        };
        if answered && resolution != PermissionResolution::Cancelled && request.goal_id.is_some() {
            durable
                .goals
                .resume_for_work(&permission.session_id)
                .map_err(|error| permission_failure(error.to_string()))?;
        }
        if resolution == PermissionResolution::AllowedSession {
            self.grants.remember(PermissionGrant::new(
                routed.grant_session_id(),
                &permission.permission,
                &permission.patterns,
            ));
        }
        Ok(answered && resolution != PermissionResolution::Cancelled)
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
        let response =
            self.client
                .request_permission(request)
                .await
                .map_err(|_| ToolError::Denied {
                    tool: tool.to_owned(),
                })?;
        let resolution = permission_resolution(&response, reusable);
        if let Some(durable) = locked(&self.durable).clone() {
            let reply = reply_kind(resolution);
            durable
                .store
                .resolve(
                    &request_id,
                    HumanRequestState::Answered,
                    Some(&json!({"reply": reply})),
                    zuno_db::message::now_millis(),
                )
                .map_err(permission_store_failure)?;
            if durable
                .store
                .get(&request_id)
                .map_err(permission_store_failure)?
                .is_some_and(|request| request.goal_id.is_some())
            {
                durable
                    .goals
                    .resume_for_work(origin.session_id())
                    .map_err(|error| permission_failure(error.to_string()))?;
            }
        }
        match resolution {
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

fn permission_resolution(response: &Value, allow_session: bool) -> PermissionResolution {
    match (
        response.pointer("/outcome/outcome").and_then(Value::as_str),
        response
            .pointer("/outcome/optionId")
            .and_then(Value::as_str),
    ) {
        (Some("cancelled"), _) => PermissionResolution::Cancelled,
        (Some("selected"), Some("allow_once")) => PermissionResolution::AllowedOnce,
        (Some("selected"), Some("allow_session")) if allow_session => {
            PermissionResolution::AllowedSession
        }
        _ => PermissionResolution::Denied,
    }
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

    #[test]
    fn permission_responses_fail_closed_and_recognize_cancellation() {
        let selected =
            |option: &str| json!({ "outcome": { "outcome": "selected", "optionId": option } });
        assert_eq!(
            permission_resolution(&selected("allow_once"), false),
            PermissionResolution::AllowedOnce
        );
        assert_eq!(
            permission_resolution(&selected("allow_session"), true),
            PermissionResolution::AllowedSession
        );
        assert_eq!(
            permission_resolution(&selected("allow_session"), false),
            PermissionResolution::Denied,
            "a client cannot select an option that was not offered"
        );
        for option in ["allow_always", "reject_once", "reject_always", "unknown"] {
            assert_eq!(
                permission_resolution(&selected(option), true),
                PermissionResolution::Denied
            );
        }
        assert_eq!(
            permission_resolution(&json!({ "outcome": { "outcome": "cancelled" } }), true),
            PermissionResolution::Cancelled
        );
        for malformed in [
            json!({}),
            json!({ "outcome": null }),
            json!({ "outcome": { "outcome": "selected" } }),
        ] {
            assert_eq!(
                permission_resolution(&malformed, true),
                PermissionResolution::Denied
            );
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
}
