use serde_json::{Map, Value, json};

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
    ("/api/integration/{integrationID}/connect/key", "post"),
    ("/api/integration/{integrationID}/connect/oauth", "post"),
    ("/api/integration/attempt/{attemptID}", "get"),
    ("/api/integration/attempt/{attemptID}/complete", "post"),
    ("/api/integration/attempt/{attemptID}", "delete"),
    ("/api/credential/{credentialID}", "patch"),
    ("/api/credential/{credentialID}", "delete"),
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
    ("/api/permission/saved", "get"),
    ("/api/permission/saved/{id}", "delete"),
    ("/api/session/{sessionID}/permission", "post"),
    ("/api/session/{sessionID}/permission", "get"),
    ("/api/session/{sessionID}/permission/{requestID}", "get"),
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
    ("/api/session/{sessionID}/message/{messageID}", "get"),
    ("/api/session/{sessionID}/message", "get"),
];

#[must_use]
pub fn document() -> Value {
    let mut paths = Map::new();
    for (path, method) in OPERATIONS {
        let item = paths
            .entry((*path).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(methods) = item {
            methods.insert(
                (*method).to_owned(),
                json!({
                    "operationId": operation_id(method, path),
                    "responses": {
                        "200": {"description": "Success"},
                        "503": {"description": "Operation is known but its local backend is explicitly unavailable"}
                    }
                }),
            );
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {"title": "opencode Rust API", "version": env!("CARGO_PKG_VERSION")},
        "paths": paths,
        "components": {
            "schemas": {
                "Session": schemars::schema_for!(super::session::SessionInfo),
                "SessionCreate": schemars::schema_for!(super::session::CreateSessionBody),
                "SessionListResponse": schemars::schema_for!(super::session::SessionListResponse),
                "SessionActive": schemars::schema_for!(super::session::SessionActive),
                "SessionActiveResponse": schemars::schema_for!(super::session::SessionActiveResponse),
                "SessionPruneMutation": schemars::schema_for!(super::maintenance::MutationBody)
            }
        }
    })
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
