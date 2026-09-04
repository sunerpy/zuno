use serde_json::{Map, Value, json};

type BodySchemaGap = (&'static str, &'static str, &'static str);

pub const OPERATIONS: &[(&str, &str)] = &[
    ("/api/health", "get"),
    ("/api/location", "get"),
    ("/api/event", "get"),
    ("/api/agent", "get"),
    ("/api/model", "get"),
    ("/api/command", "get"),
    ("/api/skill", "get"),
    ("/api/reference", "get"),
    ("/api/provider", "get"),
    ("/api/provider/{providerID}", "get"),
    ("/api/integration", "get"),
    ("/api/integration/{integrationID}", "get"),
    ("/api/fs/read/*", "get"),
    ("/api/fs/list", "get"),
    ("/api/fs/find", "get"),
    ("/api/pty", "get"),
    ("/api/pty", "post"),
    ("/api/pty/{ptyID}", "get"),
    ("/api/pty/{ptyID}", "put"),
    ("/api/pty/{ptyID}", "delete"),
    ("/api/pty/{ptyID}/connect-token", "post"),
    ("/api/pty/{ptyID}/connect", "get"),
    ("/api/permission/request", "get"),
    ("/api/session/{sessionID}/permission", "get"),
    (
        "/api/session/{sessionID}/permission/{requestID}/reply",
        "post",
    ),
    ("/api/question/request", "get"),
    ("/api/session/{sessionID}/question", "get"),
    (
        "/api/session/{sessionID}/question/{requestID}/reply",
        "post",
    ),
    (
        "/api/session/{sessionID}/question/{requestID}/reject",
        "post",
    ),
    ("/api/session", "get"),
    ("/api/session", "post"),
    ("/api/session/prune", "get"),
    ("/api/session/prune", "post"),
    ("/api/session/active", "get"),
    ("/api/session/{sessionID}", "get"),
    ("/api/session/{sessionID}/learning", "get"),
    ("/api/session/{sessionID}/event", "get"),
    ("/api/session/{sessionID}/agent", "post"),
    ("/api/session/{sessionID}/model", "post"),
    ("/api/session/{sessionID}/prompt", "post"),
    ("/api/session/{sessionID}/compact", "post"),
    ("/api/session/{sessionID}/wait", "post"),
    ("/api/session/{sessionID}/revert/stage", "post"),
    ("/api/session/{sessionID}/revert/clear", "post"),
    ("/api/session/{sessionID}/revert/commit", "post"),
    ("/api/session/{sessionID}/context", "get"),
    ("/api/session/{sessionID}/history", "get"),
    ("/api/session/{sessionID}/interrupt", "post"),
    ("/api/session/{sessionID}/message", "get"),
];

const BODY_SCHEMA_GAPS: &[BodySchemaGap] = &[
    (
        "/api/health",
        "get",
        "the successful response is an untyped Json<Value>",
    ),
    (
        "/api/location",
        "get",
        "LocationInfo does not derive JsonSchema",
    ),
    (
        "/api/event",
        "get",
        "the successful response is an SSE stream, not a modeled JSON body",
    ),
    (
        "/api/agent",
        "get",
        "LocationEnvelope<Vec<AgentInfo>> and its nested catalog types do not derive JsonSchema",
    ),
    (
        "/api/model",
        "get",
        "LocationEnvelope<Vec<ModelInfo>> and its nested provider types do not derive JsonSchema",
    ),
    (
        "/api/command",
        "get",
        "LocationEnvelope<Vec<CommandInfo>> does not derive JsonSchema",
    ),
    (
        "/api/skill",
        "get",
        "LocationEnvelope<Vec<SkillInfo>> does not derive JsonSchema",
    ),
    (
        "/api/reference",
        "get",
        "LocationEnvelope<Vec<ReferenceInfo>> does not derive JsonSchema",
    ),
    (
        "/api/provider",
        "get",
        "LocationEnvelope<Vec<ProviderInfo>> and its nested types do not derive JsonSchema",
    ),
    (
        "/api/provider/{providerID}",
        "get",
        "LocationEnvelope<ProviderInfo> and its nested types do not derive JsonSchema",
    ),
    (
        "/api/integration",
        "get",
        "LocationEnvelope<Vec<IntegrationInfo>> and its nested types do not derive JsonSchema",
    ),
    (
        "/api/integration/{integrationID}",
        "get",
        "OptionalEnvelope<IntegrationInfo> and its nested types do not derive JsonSchema",
    ),
    (
        "/api/fs/read/*",
        "get",
        "the response is content-type-dependent raw bytes with no schema type",
    ),
    (
        "/api/fs/list",
        "get",
        "LocationEnvelope<Vec<Entry>> does not derive JsonSchema",
    ),
    (
        "/api/fs/find",
        "get",
        "FindEnvelope does not derive JsonSchema",
    ),
    (
        "/api/pty",
        "get",
        "PtyInfo is imported without a JsonSchema implementation",
    ),
    (
        "/api/pty",
        "post",
        "CreateInput and PtyInfo are imported without JsonSchema implementations",
    ),
    (
        "/api/pty/{ptyID}",
        "get",
        "PtyInfo is imported without a JsonSchema implementation",
    ),
    (
        "/api/pty/{ptyID}",
        "put",
        "UpdateInput and PtyInfo are imported without JsonSchema implementations",
    ),
    (
        "/api/pty/{ptyID}/connect-token",
        "post",
        "ConnectTokenResponse and its nested types do not derive JsonSchema",
    ),
    (
        "/api/pty/{ptyID}/connect",
        "get",
        "the response upgrades to WebSocket frames and has no JSON body model",
    ),
    (
        "/api/session/prune",
        "get",
        "SessionPruneReport does not derive JsonSchema",
    ),
    (
        "/api/session/prune",
        "post",
        "the request is bound, but SessionPruneReport does not derive JsonSchema for the response",
    ),
    (
        "/api/session/{sessionID}/event",
        "get",
        "the successful response is an SSE stream, not a modeled JSON body",
    ),
    (
        "/api/session/{sessionID}/learning",
        "get",
        "LearningStateProjection serializes the shared durable projection but does not derive JsonSchema",
    ),
    (
        "/api/session/{sessionID}/agent",
        "post",
        "AgentBody does not derive JsonSchema",
    ),
    (
        "/api/session/{sessionID}/model",
        "post",
        "ModelBody and ModelRefBody do not derive JsonSchema",
    ),
    (
        "/api/session/{sessionID}/prompt",
        "post",
        "PromptBody, PromptAdmitted, and their nested types do not derive JsonSchema",
    ),
    (
        "/api/session/{sessionID}/revert/stage",
        "post",
        "RevertStageBody does not derive JsonSchema and Data<Value> leaves the response untyped",
    ),
    (
        "/api/session/{sessionID}/context",
        "get",
        "Data<Vec<Value>> leaves context items untyped",
    ),
    (
        "/api/session/{sessionID}/history",
        "get",
        "HistoryResponse does not derive JsonSchema",
    ),
    (
        "/api/session/{sessionID}/message",
        "get",
        "MessagesResponse and MessageCursor do not derive JsonSchema",
    ),
];

pub(crate) const fn body_schema_gaps() -> &'static [BodySchemaGap] {
    BODY_SCHEMA_GAPS
}

#[cfg(test)]
const BODYLESS_OPERATIONS: &[(&str, &str)] = &[
    ("/api/pty/{ptyID}", "delete"),
    (
        "/api/session/{sessionID}/question/{requestID}/reject",
        "post",
    ),
    ("/api/session/{sessionID}/compact", "post"),
    ("/api/session/{sessionID}/wait", "post"),
    ("/api/session/{sessionID}/revert/clear", "post"),
    ("/api/session/{sessionID}/revert/commit", "post"),
    ("/api/session/{sessionID}/interrupt", "post"),
];

#[must_use]
pub fn document() -> Value {
    let mut paths = Map::new();
    for (path, method) in OPERATIONS {
        let item = paths
            .entry((*path).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(methods) = item {
            let mut operation = json!({
                "operationId": operation_id(method, path),
                "responses": {
                    "200": {"description": "Success"},
                    "503": {"description": "Operation is known but its local backend is explicitly unavailable"}
                }
            });
            if let Some(description) = operation_description(method, path) {
                operation["description"] = Value::String(description.to_owned());
            }
            bind_existing_body_schemas(&mut operation, method, path);
            methods.insert((*method).to_owned(), operation);
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {"title": "Zuno API", "version": env!("CARGO_PKG_VERSION")},
        "paths": paths,
        "components": {
            "schemas": {
                "Session": schemars::schema_for!(super::session::SessionInfo),
                "SessionCreate": schemars::schema_for!(super::session::CreateSessionBody),
                "SessionResponse": schemars::schema_for!(super::Data<super::session::SessionInfo>),
                "SessionListResponse": schemars::schema_for!(super::session::SessionListResponse),
                "SessionActive": schemars::schema_for!(super::session::SessionActive),
                "SessionActiveResponse": schemars::schema_for!(super::session::SessionActiveResponse),
                "SessionPruneMutation": schemars::schema_for!(super::maintenance::MutationBody),
                "PermissionRequestListResponse": schemars::schema_for!(
                    super::request::LocationResponse<crate::PermissionRequest>
                ),
                "SessionPermissionResponse": schemars::schema_for!(
                    super::Data<Vec<crate::PermissionRequest>>
                ),
                "PermissionReply": schemars::schema_for!(
                    super::request::PermissionReplyBody
                ),
                "QuestionRequestListResponse": schemars::schema_for!(
                    super::request::LocationResponse<crate::QuestionRequest>
                ),
                "SessionQuestionResponse": schemars::schema_for!(
                    super::Data<Vec<crate::QuestionRequest>>
                ),
                "QuestionReply": schemars::schema_for!(
                    super::request::QuestionReplyBody
                )
            }
        }
    })
}

fn bind_existing_body_schemas(operation: &mut Value, method: &str, path: &str) {
    match (method, path) {
        ("get", "/api/session") => bind_response(operation, "SessionListResponse"),
        ("post", "/api/session") => {
            bind_request(operation, "SessionCreate");
            bind_response(operation, "SessionResponse");
        }
        ("post", "/api/session/prune") => {
            bind_request(operation, "SessionPruneMutation");
        }
        ("get", "/api/session/active") => bind_response(operation, "SessionActiveResponse"),
        ("get", "/api/session/{sessionID}") => bind_response(operation, "SessionResponse"),
        ("get", "/api/permission/request") => {
            bind_response(operation, "PermissionRequestListResponse");
        }
        ("get", "/api/session/{sessionID}/permission") => {
            bind_response(operation, "SessionPermissionResponse");
        }
        ("post", "/api/session/{sessionID}/permission/{requestID}/reply") => {
            bind_request(operation, "PermissionReply");
        }
        ("get", "/api/question/request") => {
            bind_response(operation, "QuestionRequestListResponse");
        }
        ("get", "/api/session/{sessionID}/question") => {
            bind_response(operation, "SessionQuestionResponse");
        }
        ("post", "/api/session/{sessionID}/question/{requestID}/reply") => {
            bind_request(operation, "QuestionReply");
        }
        _ => {}
    }
}

fn operation_description(method: &str, path: &str) -> Option<&'static str> {
    match (method, path) {
        ("get", "/api/permission/request") | ("get", "/api/session/{sessionID}/permission") => {
            Some(
                "Lists pending durable permission requests. Requests survive process restart; live channels only wake consumers.",
            )
        }
        ("post", "/api/session/{sessionID}/permission/{requestID}/reply") => Some(
            "Settles one durable permission request and admits its answer before Goal continuation resumes.",
        ),
        ("get", "/api/question/request") | ("get", "/api/session/{sessionID}/question") => {
            Some("Lists pending durable human-input requests in deterministic creation order.")
        }
        ("post", "/api/session/{sessionID}/question/{requestID}/reply") => Some(
            "Atomically settles one durable question and admits the model-visible answer to the session inbox.",
        ),
        _ => None,
    }
}

fn bind_request(operation: &mut Value, schema: &str) {
    operation["requestBody"] = json!({
        "required": true,
        "content": {
            "application/json": {
                "schema": {"$ref": format!("#/components/schemas/{schema}")}
            }
        }
    });
}

fn bind_response(operation: &mut Value, schema: &str) {
    operation["responses"]["200"]["content"] = json!({
        "application/json": {
            "schema": {"$ref": format!("#/components/schemas/{schema}")}
        }
    });
}

fn operation_id(method: &str, path: &str) -> String {
    format!(
        "{}_{}",
        method,
        path.trim_matches('/')
            .replace(['/', '{', '}', '*'], "_")
            .trim_matches('_')
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_operation_is_bound_bodyless_or_a_reasoned_frozen_gap() {
        assert_eq!(
            BODY_SCHEMA_GAPS.len(),
            32,
            "review and re-freeze every gap change"
        );
        assert_eq!(
            BODYLESS_OPERATIONS.len(),
            7,
            "review and re-freeze every bodyless change"
        );

        let operations = OPERATIONS.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            operations.len(),
            OPERATIONS.len(),
            "duplicate OpenAPI operation"
        );
        let gaps = BODY_SCHEMA_GAPS
            .iter()
            .map(|(path, method, reason)| {
                assert!(
                    !reason.trim().is_empty(),
                    "{method} {path} has no gap reason"
                );
                (*path, *method)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            gaps.len(),
            BODY_SCHEMA_GAPS.len(),
            "duplicate body schema gap"
        );
        let bodyless = BODYLESS_OPERATIONS.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            bodyless.len(),
            BODYLESS_OPERATIONS.len(),
            "duplicate bodyless operation"
        );
        assert!(
            gaps.is_disjoint(&bodyless),
            "an operation cannot be both bodyless and a body-schema gap"
        );

        let document = document();
        for (path, method) in OPERATIONS {
            let operation = &document["paths"][path][method];
            let bound = operation.get("requestBody").is_some()
                || operation["responses"]["200"].get("content").is_some();
            assert!(
                bound || gaps.contains(&(*path, *method)) || bodyless.contains(&(*path, *method)),
                "{method} {path} is neither bound, intentionally bodyless, nor frozen as a gap"
            );
        }
        for key in gaps.union(&bodyless) {
            assert!(
                operations.contains(key),
                "inventory names unregistered operation {} {}",
                key.1,
                key.0
            );
        }
    }
}
