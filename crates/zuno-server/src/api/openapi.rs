use schemars::JsonSchema;
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
    ("/api/session", "get"),
    ("/api/session", "post"),
    ("/api/session/prune", "get"),
    ("/api/session/prune", "post"),
    ("/api/session/active", "get"),
    ("/api/session/{sessionID}", "get"),
    ("/api/session/{sessionID}/learning", "get"),
    ("/api/session/{sessionID}/memory-policy", "get"),
    ("/api/session/{sessionID}/memory-policy", "put"),
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

/// Operations mounted only when the application supplies a question provider.
const QUESTION_OPERATIONS: &[(&str, &str)] = &[
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
    (
        "/api/session/{sessionID}/question/{requestID}/defer",
        "post",
    ),
];

const CONTROL_OPERATIONS: &[(&str, &str)] = &[("/api/session/{sessionID}/resume", "post")];

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
    ("/api/session/{sessionID}/compact", "post"),
    ("/api/session/{sessionID}/wait", "post"),
    ("/api/session/{sessionID}/revert/clear", "post"),
    ("/api/session/{sessionID}/revert/commit", "post"),
    ("/api/session/{sessionID}/interrupt", "post"),
];

#[must_use]
pub fn document() -> Value {
    document_for(false, false)
}

#[must_use]
pub fn document_with_questions() -> Value {
    document_for(true, false)
}

pub(super) fn document_for(has_questions: bool, has_controls: bool) -> Value {
    let mut paths = Map::new();
    let question_operations = if has_questions {
        QUESTION_OPERATIONS
    } else {
        &[]
    };
    let control_operations = if has_controls {
        CONTROL_OPERATIONS
    } else {
        &[]
    };
    for (path, method) in OPERATIONS
        .iter()
        .chain(question_operations)
        .chain(control_operations)
    {
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
    let mut document = json!({
        "openapi": "3.1.0",
        "info": {"title": "Zuno API", "version": env!("CARGO_PKG_VERSION")},
        "paths": paths,
        "components": {
            "schemas": {
                "Session": schemars::schema_for!(super::session::SessionInfo),
                "SessionCreate": schemars::schema_for!(super::session::CreateSessionBody),
                "SessionResponse": schemars::schema_for!(super::Data<super::session::SessionInfo>),
                "LearningStateResponse": schemars::schema_for!(super::Data<zuno_types::LearningStateProjection>),
                "SessionListResponse": schemars::schema_for!(super::session::SessionListResponse),
                "SessionActive": schemars::schema_for!(super::session::SessionActive),
                "SessionActiveResponse": schemars::schema_for!(super::session::SessionActiveResponse),
                "MemoryPolicyResponse": schemars::schema_for!(
                    super::Data<super::session::MemoryPolicyBody>
                ),
                "MemoryPolicyUpdate": schemars::schema_for!(
                    super::session::UpdateMemoryPolicyBody
                ),
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
            }
        }
    });
    if has_questions {
        let schemas = document["components"]["schemas"]
            .as_object_mut()
            .expect("schema object");
        schemas.extend(
            json!({
                "QuestionRequestListResponse": question_schema::<
                    super::request::LocationResponse<zuno_types::question::QuestionView>
                >("QuestionRequestListResponse"),
                "SessionQuestionResponse": question_schema::<
                    super::Data<Vec<zuno_types::question::QuestionView>>
                >("SessionQuestionResponse"),
                "QuestionCommand": question_schema::<zuno_types::question::QuestionCommand>("QuestionCommand"),
                "QuestionReceiptResponse": question_schema::<
                    super::Data<zuno_types::question::QuestionReceipt>
                >("QuestionReceiptResponse"),
                "QuestionErrorResponse": question_schema::<super::request::QuestionErrorResponse>("QuestionErrorResponse")
            })
            .as_object()
            .expect("question schemas")
            .clone(),
        );
    }
    if has_controls {
        document["components"]["schemas"]["SessionResumeRequest"] =
            question_schema::<super::session::ResumeBody>("SessionResumeRequest");
        document["components"]["schemas"]["SessionResumeResponse"] =
            question_schema::<super::Data<super::session::ResumeAdmitted>>("SessionResumeResponse");
    }
    document
}

/// Schemars' local definition references must resolve inside the OpenAPI document.
fn question_schema<T: JsonSchema>(name: &str) -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    rebase_schema_refs(&mut schema, &format!("#/components/schemas/{name}"));
    schema
}

fn rebase_schema_refs(value: &mut Value, base: &str) {
    match value {
        Value::Object(fields) => {
            if let Some(Value::String(reference)) = fields.get_mut("$ref")
                && let Some(local) = reference.strip_prefix('#')
            {
                *reference = format!("{base}{local}");
            }
            for child in fields.values_mut() {
                rebase_schema_refs(child, base);
            }
        }
        Value::Array(values) => {
            for child in values {
                rebase_schema_refs(child, base);
            }
        }
        _ => {}
    }
}

fn bind_existing_body_schemas(operation: &mut Value, method: &str, path: &str) {
    match (method, path) {
        ("get", "/api/session") => bind_response(operation, "SessionListResponse"),
        ("post", "/api/session") => {
            bind_request(operation, "SessionCreate");
            bind_response(operation, "SessionResponse");
        }
        ("post", "/api/session/{sessionID}/resume") => {
            bind_request(operation, "SessionResumeRequest");
            bind_response(operation, "SessionResumeResponse");
            operation["responses"]["400"] =
                json!({"description":"Invalid request or expectedRevision"});
            operation["responses"]["404"] = json!({"description":"Session does not exist"});
            operation["responses"]["409"] = json!({"description":"Stale revision, Plan mode, exact wait, inactive Goal, or Work is not explicitly paused/completed"});
        }
        ("post", "/api/session/prune") => {
            bind_request(operation, "SessionPruneMutation");
        }
        ("get", "/api/session/active") => bind_response(operation, "SessionActiveResponse"),
        ("get", "/api/session/{sessionID}") => bind_response(operation, "SessionResponse"),
        ("get", "/api/session/{sessionID}/learning") => {
            bind_response(operation, "LearningStateResponse");
            operation["parameters"] = json!([
                {"name":"sessionID","in":"path","required":true,"schema":{"type":"string"}},
                {"name":"offset","in":"query","required":false,"schema":{"type":"integer","minimum":0,"default":0}},
                {"name":"limit","in":"query","required":false,"description":"Clamped to 1..100",
                    "schema":{"type":"integer","minimum":0,"default":100}}
            ]);
        }
        ("get", "/api/session/{sessionID}/memory-policy") => {
            bind_response(operation, "MemoryPolicyResponse");
        }
        ("put", "/api/session/{sessionID}/memory-policy") => {
            bind_request(operation, "MemoryPolicyUpdate");
            bind_response(operation, "MemoryPolicyResponse");
            operation["responses"]["400"] = json!({"description": "The requested policy exceeds the active configuration or is malformed"});
            operation["responses"]["404"] = json!({"description": "The session does not exist"});
            operation["responses"]["409"] = json!({
                "description": "The expectedRevision is stale, the session is excluded, or a live turn owns the session"
            });
        }
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
            bind_question_errors(operation);
        }
        ("get", "/api/session/{sessionID}/question") => {
            bind_response(operation, "SessionQuestionResponse");
            bind_question_errors(operation);
        }
        ("post", "/api/session/{sessionID}/question/{requestID}/reply")
        | ("post", "/api/session/{sessionID}/question/{requestID}/reject")
        | ("post", "/api/session/{sessionID}/question/{requestID}/defer") => {
            bind_request(operation, "QuestionCommand");
            bind_response(operation, "QuestionReceiptResponse");
            bind_question_errors(operation);
        }
        _ => {}
    }
}

fn bind_question_errors(operation: &mut Value) {
    for (status, description) in [
        (
            "400",
            "Invalid path, body, command, item ID, answer, or route action; the question remains unchanged",
        ),
        ("404", "The session or question does not exist"),
        (
            "409",
            "Stale revision, reused command ID with different input, closed question, or rejected Plan/Goal transition",
        ),
        ("500", "Question storage or worker failed"),
        (
            "503",
            "The injected question provider is temporarily unavailable",
        ),
    ] {
        operation["responses"][status] = json!({
            "description": description,
            "content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/QuestionErrorResponse"}
            }}
        });
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
            "Applies a revision-checked, idempotent QuestionCommand through the configured provider and returns its committed receipt. Answers use stable item IDs. Empty answers do not enter model input. Plan approval requires an explicit plan_decision action.",
        ),
        ("post", "/api/session/{sessionID}/question/{requestID}/reject") => Some(
            "Applies an explicit cancel QuestionCommand. commandId and expectedRevision are required; an absent body is not consent or cancellation.",
        ),
        ("post", "/api/session/{sessionID}/question/{requestID}/defer") => Some(
            "Applies an explicit defer QuestionCommand with optional draftAnswers and returns its committed receipt. Draft values remain client-only. Deferral leaves the question pending, creates no model inbox input, and does not authorize Work.",
        ),
        ("post", "/api/session/{sessionID}/resume") => Some(
            "Explicitly resumes the exact paused or completed Work revision through native session control. It does not authorize a Plan or waive an exact wait. The response identifies the committed control input, not completed execution.",
        ),
        ("put", "/api/session/{sessionID}/memory-policy") => Some(
            "Updates useMemories and enabled|disabled generation through the session's active TurnHost. expectedRevision is a compare-and-set guard; excluded is host-owned and cannot be requested.",
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
            31,
            "review and re-freeze every gap change"
        );
        assert_eq!(
            BODYLESS_OPERATIONS.len(),
            6,
            "review and re-freeze every bodyless change"
        );

        let operations = OPERATIONS
            .iter()
            .chain(QUESTION_OPERATIONS)
            .chain(CONTROL_OPERATIONS)
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            operations.len(),
            OPERATIONS.len() + QUESTION_OPERATIONS.len() + CONTROL_OPERATIONS.len(),
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

        let document = document_for(true, true);
        for (path, method) in OPERATIONS
            .iter()
            .chain(QUESTION_OPERATIONS)
            .chain(CONTROL_OPERATIONS)
        {
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

    #[test]
    fn question_operations_require_a_configured_provider() {
        let without = document();
        let with = document_with_questions();
        for (path, method) in QUESTION_OPERATIONS {
            assert!(without["paths"].get(*path).is_none());
            assert!(with["paths"][path][method].is_object());
        }
        assert!(
            without["components"]["schemas"]
                .get("QuestionCommand")
                .is_none()
        );
    }
}
