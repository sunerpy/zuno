use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

const TEST_CONFIG: &str = r#"{"formatter":false,"lsp":false,"model":"test/test-model","provider":{"test":{"name":"test","id":"test","env":[],"transport":"openai-compatible","models":{"test-model":{"id":"test-model","name":"Test model","attachment":false,"reasoning":false,"temperature":false,"tool_call":true,"release_date":"2025-01-01","limit":{"context":100000,"output":10000},"cost":{"input":0,"output":0},"options":{}}},"options":{"apiKey":"acp-probe","baseURL":"https://example.invalid/v1"}}}}"#;

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zuno"))
}

fn isolated_command(root: &std::path::Path) -> Command {
    isolated_command_with_config(root, TEST_CONFIG)
}

fn isolated_command_with_config(root: &std::path::Path, config: &str) -> Command {
    let mut command = binary();
    command
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("ZUNO_DB", root.join("zuno-acp.db"))
        .env("ZUNO_AUTH_CONTENT", "{}")
        .env("ZUNO_CONFIG_CONTENT", config)
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true");
    command
}

fn config_with_second_model(base_url: &str) -> String {
    let mut config: Value = serde_json::from_str(TEST_CONFIG).expect("test config JSON");
    config["provider"]["test"]["models"]["test-model-2"] = json!({
        "id": "test-model-2",
        "name": "Second test model",
        "attachment": false,
        "reasoning": false,
        "temperature": false,
        "tool_call": true,
        "release_date": "2025-01-01",
        "limit": {"context": 120000, "output": 10000},
        "cost": {"input": 0, "output": 0},
        "options": {}
    });
    config["provider"]["test"]["options"]["baseURL"] = json!(format!("{base_url}/v1"));
    serde_json::to_string(&config).expect("encode test config")
}

fn strict_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(base_url)).expect("test config JSON");
    config["permission"] = json!({"mode":"strict","rules":{}});
    serde_json::to_string(&config).expect("encode strict test config")
}

fn danger_full_access_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&strict_config(base_url)).expect("strict test config JSON");
    config["sandbox"] = json!({"mode":"danger-full-access"});
    serde_json::to_string(&config).expect("encode danger-full-access test config")
}

fn manual_compaction_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(base_url)).expect("test config JSON");
    config["small_model"] = json!("test/test-model");
    config["compaction"] = json!({
        "auto": false,
        "tail_turns": 1,
        "preserve_recent_tokens": 1_000
    });
    serde_json::to_string(&config).expect("encode compaction test config")
}

fn config_with_remote_mcp(mcp_url: &str, provider_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(provider_url)).expect("test config JSON");
    config["mcp"] = json!({
        "lifecycle": {
            "type": "remote",
            "url": mcp_url,
            "oauth": false
        }
    });
    serde_json::to_string(&config).expect("encode remote MCP test config")
}

async fn mount_remote_mcp_fixture(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("initialize body");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("mcp-session-id", "acp-lifecycle-session")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "acp-lifecycle-fixture", "version": "1.0.0"}
                    }
                }))
        })
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("tools/list body");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {"tools": []}
                }))
        })
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

fn reasoning_config() -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model("https://example.invalid"))
            .expect("test config JSON");
    let model = &mut config["provider"]["test"]["models"]["test-model"];
    model["reasoning"] = json!(true);
    model["variants"] = json!({
        "low": {"reasoningEffort": "low"},
        "xhigh": {"reasoningEffort": "xhigh"},
        "max": {"reasoningEffort": "max"}
    });
    serde_json::to_string(&config).expect("encode reasoning test config")
}

fn rich_prompt_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(base_url)).expect("test config JSON");
    let model = &mut config["provider"]["test"]["models"]["test-model"];
    model["attachment"] = json!(true);
    model["modalities"] = json!({
        "input": ["text", "image"],
        "output": ["text"]
    });
    serde_json::to_string(&config).expect("encode rich prompt test config")
}

fn put_durable_message(
    connection: &zuno_db::Connection,
    session_id: &str,
    id: &str,
    role: &str,
    created: i64,
    extra: Value,
) {
    let mut payload = json!({
        "id": id,
        "sessionID": session_id,
        "role": role,
        "time": { "created": created },
    });
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            payload[key] = value;
        }
    }
    let record = zuno_db::message::MessageRecord::from_json(payload).expect("durable message");
    zuno_db::message::MessageStore::new(connection)
        .put_message_at(&record, created)
        .expect("persist durable message");
}

fn put_durable_part(
    connection: &zuno_db::Connection,
    session_id: &str,
    message_id: &str,
    id: &str,
    created: i64,
    body: Value,
) {
    let mut payload = json!({
        "id": id,
        "sessionID": session_id,
        "messageID": message_id,
    });
    if let Value::Object(body) = body {
        for (key, value) in body {
            payload[key] = value;
        }
    }
    let record = zuno_db::message::PartRecord::from_json(payload, created).expect("durable part");
    zuno_db::message::MessageStore::new(connection)
        .put_part_at(&record, created)
        .expect("persist durable part");
}

fn seed_durable_replay(root: &std::path::Path, session_id: &str) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let connection = zuno_db::open::open(&location).expect("open ACP database");
    let user_id = "msg_acp_load_user";
    put_durable_message(&connection, session_id, user_id, "user", 100, Value::Null);
    put_durable_part(
        &connection,
        session_id,
        user_id,
        "prt_acp_load_user_01_text",
        100,
        json!({"type":"text","text":"replay this durable session"}),
    );
    put_durable_part(
        &connection,
        session_id,
        user_id,
        "prt_acp_load_user_02_image",
        101,
        json!({
            "type": "file",
            "filename": "pixel.png",
            "mime": "image/png",
            "data": "aGVsbG8=",
            "url": "data:image/png;base64,aGVsbG8=",
        }),
    );

    let assistant_id = "msg_acp_load_assistant";
    put_durable_message(
        &connection,
        session_id,
        assistant_id,
        "assistant",
        200,
        json!({
            "cost": 1.25,
            "finish": "stop",
            "tokens": {
                "input": 100,
                "output": 25,
                "reasoning": 0,
                "cache": {"read": 40, "write": 10},
                "accounting": "cache-beside-input"
            }
        }),
    );
    put_durable_part(
        &connection,
        session_id,
        assistant_id,
        "prt_acp_load_assistant_01_thought",
        200,
        json!({"type":"reasoning","text":"inspect durable state"}),
    );
    let edited_path = root.join("src/lib.rs");
    std::fs::create_dir_all(edited_path.parent().expect("edited path parent"))
        .expect("create replay source directory");
    std::fs::write(&edited_path, "new\n").expect("write replay source file");
    let edited_path = edited_path.to_string_lossy().into_owned();
    put_durable_part(
        &connection,
        session_id,
        assistant_id,
        "prt_acp_load_assistant_02_tool",
        201,
        json!({
            "type": "tool",
            "callID": "call_acp_load_edit",
            "tool": "edit",
            "displayName": "Edit lib.rs",
            "state": {
                "status": "completed",
                "raw": serde_json::to_string(&json!({"filePath": edited_path})).expect("raw input"),
                "input": {"filePath": edited_path},
                "title": "Updated lib.rs",
                "output": "ok",
                "metadata": {
                    "diff": "@@ -1 +1 @@\n-old\n+new\n",
                    "fileDiffs": [{"path": edited_path, "oldText": "old\n", "newText": "new\n"}],
                    "writtenPaths": [edited_path]
                },
                "attachments": [{
                    "mime": "image/png",
                    "filename": "preview.png",
                    "url": "data:image/png;base64,cHJldmlldw=="
                }]
            }
        }),
    );
    put_durable_part(
        &connection,
        session_id,
        assistant_id,
        "prt_acp_load_assistant_03_text",
        202,
        json!({"type":"text","text":"durable replay complete"}),
    );
    connection
        .execute(
            "UPDATE session SET cost = ?2 WHERE id = ?1",
            rusqlite::params![session_id, 1.25_f64],
        )
        .expect("persist session cost");
    drop(connection);

    let pool = Arc::new(zuno_db::Pool::open(&location).expect("open ACP work-state pool"));
    zuno_tools::work_state::WorkStateStore::new(pool)
        .update_plan(
            session_id,
            zuno_tools::work_state::PlanUpdateParams {
                expected_revision: None,
                goal_id: None,
                title: "Verify ACP load".to_owned(),
                steps: vec![
                    zuno_tools::work_state::PlanStep {
                        id: "replay".to_owned(),
                        title: "Replay durable state".to_owned(),
                        status: zuno_tools::work_state::PlanStepStatus::InProgress,
                    },
                    zuno_tools::work_state::PlanStep {
                        id: "verify".to_owned(),
                        title: "Verify Zed projection".to_owned(),
                        status: zuno_tools::work_state::PlanStepStatus::Pending,
                    },
                ],
            },
        )
        .expect("persist ACP work plan");
}

fn seed_durable_text_replay(root: &std::path::Path, session_id: &str) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let connection = zuno_db::open::open(&location).expect("open ACP database");
    put_durable_message(
        &connection,
        session_id,
        "msg_acp_cold_load",
        "user",
        100,
        Value::Null,
    );
    put_durable_part(
        &connection,
        session_id,
        "msg_acp_cold_load",
        "prt_acp_cold_load",
        100,
        json!({"type":"text","text":"cold-load durable history"}),
    );
}

#[test]
fn acp_initialize_uses_stable_v1_without_fake_authentication() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": {
                "name": "zuno-test",
                "title": "Zuno ACP integration test",
                "version": "0"
            },
            "clientCapabilities": {
                "fs": {
                    "readTextFile": true,
                    "writeTextFile": true
                },
                "terminal": true
            }
        }
    });
    writeln!(
        child.stdin.as_mut().expect("ACP stdin"),
        "{}",
        serde_json::to_string(&request).expect("encode initialize")
    )
    .expect("write initialize");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for zuno acp");
    assert!(
        output.status.success(),
        "ACP process failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("ACP stdout is UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "stdout must contain exactly one JSON-RPC frame, not banners or logs: {stdout:?}"
    );
    let response: Value = serde_json::from_str(lines[0]).expect("ACP response JSON");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert_eq!(response["result"]["agentInfo"]["name"], "zuno");
    assert!(
        response["result"]["authMethods"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "Zuno must not advertise a synthetic ACP login: {response}"
    );
    assert_eq!(response["result"]["agentCapabilities"]["loadSession"], true);
    assert!(response["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
    assert!(response["result"]["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
}

fn seed_compactable_history(root: &std::path::Path, session_id: &str) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let connection = zuno_db::open::open(&location).expect("open ACP database");
    for turn in 0_i64..4 {
        let created = 1_000 + turn * 10;
        let user_id = format!("msg_compact_user_{turn}");
        put_durable_message(
            &connection,
            session_id,
            &user_id,
            "user",
            created,
            Value::Null,
        );
        put_durable_part(
            &connection,
            session_id,
            &user_id,
            &format!("prt_compact_user_{turn}"),
            created,
            json!({"type":"text","text":format!("user request {turn}")}),
        );

        let assistant_id = format!("msg_compact_assistant_{turn}");
        put_durable_message(
            &connection,
            session_id,
            &assistant_id,
            "assistant",
            created + 1,
            json!({
                "providerID": "test",
                "modelID": "test-model",
                "finish": "stop",
                "cost": 0.0,
                "tokens": {
                    "input": 20,
                    "output": 5,
                    "reasoning": 0,
                    "cache": {"read": 0, "write": 0},
                    "accounting": "cache-inside-input"
                }
            }),
        );
        put_durable_part(
            &connection,
            session_id,
            &assistant_id,
            &format!("prt_compact_assistant_{turn}"),
            created + 1,
            json!({"type":"text","text":format!("assistant response {turn}")}),
        );
    }
}

fn request(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP response");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return response["result"].clone();
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected ACP frame while waiting for {method}: {response}"
        );
    }
}

fn request_failure(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP response");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            return response.get("error").cloned().unwrap_or_else(|| {
                panic!("ACP request {method} unexpectedly succeeded: {response}")
            });
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected ACP frame while waiting for {method}: {response}"
        );
    }
}

fn read_session_update(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("read ACP session update");
    assert!(
        !line.is_empty(),
        "ACP closed before publishing a session update"
    );
    let frame: Value = serde_json::from_str(&line).expect("ACP session update JSON");
    assert_eq!(
        frame.get("method").and_then(Value::as_str),
        Some("session/update"),
        "expected an ACP session update: {frame}"
    );
    frame["params"].clone()
}

fn request_with_updates(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (response["result"].clone(), updates);
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected frame while waiting for {method}: {response}"
        );
        let update = response["params"]["update"].clone();
        if update["sessionUpdate"] != "available_commands_update" {
            updates.push(update);
        }
    }
}

fn request_with_elicitation(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
    expected_session_id: &str,
) -> (Value, Vec<Value>, Value) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    let mut elicitation = None;
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (
                response["result"].clone(),
                updates,
                elicitation.expect("session/prompt completed without elicitation/create"),
            );
        }

        match response.get("method").and_then(Value::as_str) {
            Some("session/update") => updates.push(response["params"]["update"].clone()),
            Some("elicitation/create") => {
                assert!(
                    elicitation.is_none(),
                    "duplicate elicitation request: {response}"
                );
                assert_eq!(response["jsonrpc"], "2.0");
                let request_id = response
                    .get("id")
                    .filter(|request_id| request_id.is_string())
                    .cloned()
                    .expect("elicitation request id must be a string");
                let request = response["params"].clone();
                assert_eq!(request["sessionId"], expected_session_id);
                assert_eq!(request["toolCallId"], "call_question");
                assert_eq!(request["mode"], "form");
                assert_eq!(request["message"], "Which database?");

                let schema = &request["requestedSchema"];
                assert_eq!(schema["type"], "object");
                assert_eq!(schema["title"], "Questions");
                assert_eq!(schema["required"], json!(["q0"]));
                assert_eq!(
                    schema["properties"]
                        .as_object()
                        .expect("form properties")
                        .len(),
                    1
                );
                let q0 = &schema["properties"]["q0"];
                assert_eq!(q0["type"], "string");
                assert_eq!(q0["title"], "Database");
                assert_eq!(q0["minLength"], 1);
                let description = q0["description"].as_str().expect("q0 description");
                assert!(description.contains("Which database?"));
                assert!(description.contains("Postgres: Relational database"));
                assert!(description.contains("SQLite: Embedded database"));

                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "action": "accept",
                        "content": {"q0": "SQLite"}
                    }
                });
                writeln!(
                    stdin,
                    "{}",
                    serde_json::to_string(&reply).expect("encode elicitation response")
                )
                .expect("write elicitation response");
                stdin.flush().expect("flush elicitation response");
                elicitation = Some(request);
            }
            _ => panic!("unexpected frame while waiting for {method}: {response}"),
        }
    }
}

fn request_with_permissions(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>, Vec<Value>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    let mut permissions = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (response["result"].clone(), updates, permissions);
        }
        match response.get("method").and_then(Value::as_str) {
            Some("session/update") => updates.push(response["params"]["update"].clone()),
            Some("session/request_permission") => {
                let request_id = response.get("id").cloned().expect("permission request id");
                let request = response["params"].clone();
                permissions.push(request);
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "outcome": {
                            "outcome": "selected",
                            "optionId": "allow_once"
                        }
                    }
                });
                writeln!(
                    stdin,
                    "{}",
                    serde_json::to_string(&reply).expect("encode permission response")
                )
                .expect("write permission response");
                stdin.flush().expect("flush permission response");
            }
            _ => panic!("unexpected frame while waiting for {method}: {response}"),
        }
    }
}

#[derive(Clone)]
struct TextTurnResponder;

impl Respond for TextTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        compatible_text_response(if has_tools { "ACP reply" } else { "ACP title" })
    }
}

fn compatible_text_response(text: &str) -> ResponseTemplate {
    let chunk = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    let finish = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

fn compatible_tool_response(call_id: &str, name: &str, arguments: Value) -> ResponseTemplate {
    let arguments = serde_json::to_string(&arguments).expect("serialize tool arguments");
    let call = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "function": {"name": name, "arguments": arguments}
                }]
            },
            "finish_reason": null
        }]
    });
    let finish = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

#[derive(Clone)]
struct QuestionTurnResponder;

impl Respond for QuestionTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP question title");
        }

        let has_question_result =
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "call_question"
                    })
                });
        if has_question_result {
            compatible_text_response("Configured SQLite")
        } else {
            compatible_tool_response(
                "call_question",
                "question",
                json!({
                    "questions": [{
                        "question": "Which database?",
                        "header": "Database",
                        "options": [
                            {"label": "Postgres", "description": "Relational database"},
                            {"label": "SQLite", "description": "Embedded database"}
                        ]
                    }]
                }),
            )
        }
    }
}

#[derive(Clone)]
struct PlanTurnResponder;

impl Respond for PlanTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP plan title");
        }
        let has_plan_result =
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "call_plan"
                    })
                });
        if has_plan_result {
            compatible_text_response("Plan recorded")
        } else {
            compatible_tool_response(
                "call_plan",
                "plan_update",
                json!({
                    "title": "Verify ACP projection",
                    "steps": [
                        {"id":"implement","title":"Implement plan projection","status":"in_progress"},
                        {"id":"verify","title":"Verify Zed update","status":"pending"}
                    ]
                }),
            )
        }
    }
}

#[derive(Clone)]
struct WriteTurnResponder {
    path: String,
}

impl Respond for WriteTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP write title");
        }
        let has_write_result =
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "call_write"
                    })
                });
        if has_write_result {
            compatible_text_response("File created")
        } else {
            compatible_tool_response(
                "call_write",
                "write",
                json!({"filePath":self.path,"content":"created through ACP\n"}),
            )
        }
    }
}

#[test]
fn acp_session_lifecycle_uses_the_durable_zuno_store() {
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("acp-check.md"),
        "Inspect the ACP integration for $ARGUMENTS.",
    )
    .expect("write project command");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let cwd = root.path().to_string_lossy().into_owned();
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("new session id")
        .to_owned();
    assert_eq!(created["modes"]["currentModeId"], "build");
    assert!(
        created["configOptions"]
            .as_array()
            .is_some_and(|options| options.iter().any(|option| option["id"] == "model")),
        "new session must expose its resolved model selector: {created}"
    );
    let commands = read_session_update(&mut stdout);
    assert_eq!(commands["sessionId"], session_id);
    let command_update = &commands["update"];
    assert_eq!(command_update["sessionUpdate"], "available_commands_update");
    assert!(
        command_update["availableCommands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command["name"] == "acp-check")),
        "project commands must be published after session/new: {command_update}"
    );
    let planned = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "plan"}),
    );
    assert_eq!(planned, json!({}));
    let building = request(
        &mut stdin,
        &mut stdout,
        4,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "build"}),
    );
    assert_eq!(building, json!({}));
    let configured = request(
        &mut stdin,
        &mut stdout,
        5,
        "session/set_config_option",
        json!({"sessionId": session_id, "configId": "agent", "value": "deep"}),
    );
    assert!(
        configured["configOptions"]
            .as_array()
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "deep")
            }),
        "agent selection did not rebuild the ACP session: {configured}"
    );

    let listed = request(
        &mut stdin,
        &mut stdout,
        6,
        "session/list",
        json!({"cwd": root.path()}),
    );
    assert!(
        listed["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions
                .iter()
                .any(|session| { session["sessionId"] == session_id && session["cwd"] == cwd })),
        "materialized ACP session missing from durable list: {listed}"
    );

    request(
        &mut stdin,
        &mut stdout,
        7,
        "session/close",
        json!({"sessionId": session_id}),
    );
    let after_close = request(
        &mut stdin,
        &mut stdout,
        8,
        "session/list",
        json!({"cwd": root.path()}),
    );
    assert!(after_close["sessions"].as_array().is_some_and(|sessions| {
        sessions
            .iter()
            .any(|session| session["sessionId"] == session_id)
    }));

    request(
        &mut stdin,
        &mut stdout,
        9,
        "session/delete",
        json!({"sessionId": session_id}),
    );
    let after_delete = request(
        &mut stdin,
        &mut stdout,
        10,
        "session/list",
        json!({"cwd": root.path()}),
    );
    assert!(after_delete["sessions"].as_array().is_some_and(|sessions| {
        sessions
            .iter()
            .all(|session| session["sessionId"] != session_id)
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP lifecycle process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP lifecycle process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_load_is_cold_and_duplicate_load_does_not_replay_again() {
    let mcp = MockServer::start().await;
    mount_remote_mcp_fixture(&mcp).await;
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("cold-check.md"),
        "Inspect the restored ACP session for $ARGUMENTS.",
    )
    .expect("write project command");
    let config = config_with_remote_mcp(&format!("{}/mcp", mcp.uri()), &provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    seed_durable_text_replay(root.path(), &session_id);

    let (_loaded, first_replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let cold_commands = read_session_update(&mut stdout);
    assert_eq!(cold_commands["sessionId"], session_id);
    assert_eq!(
        cold_commands["update"]["sessionUpdate"],
        "available_commands_update"
    );
    assert!(
        cold_commands["update"]["availableCommands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command["name"] == "cold-check")),
        "a dormant load must publish project commands without activating MCP: {cold_commands}"
    );
    let initialize_after_load = mcp
        .received_requests()
        .await
        .expect("remote MCP requests")
        .into_iter()
        .filter(is_mcp_initialize)
        .count();

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/set_mode",
        json!({"sessionId": &session_id, "modeId": "plan"}),
    );
    request(
        &mut stdin,
        &mut stdout,
        6,
        "session/set_mode",
        json!({"sessionId": &session_id, "modeId": "build"}),
    );
    let initialize_after_reconfiguration = mcp
        .received_requests()
        .await
        .expect("remote MCP requests after reconfiguration")
        .into_iter()
        .filter(is_mcp_initialize)
        .count();

    let (_loaded_again, second_replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let (prompted, prompt_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        8,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"Activate the loaded ACP session."}]
        }),
    );
    let initialize_after_prompt = mcp
        .received_requests()
        .await
        .expect("remote MCP requests after prompt")
        .into_iter()
        .filter(is_mcp_initialize)
        .count();
    request(
        &mut stdin,
        &mut stdout,
        9,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP cold-load process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP cold-load process failed: {stderr}");
    }

    assert!(
        !first_replay.is_empty(),
        "the first session/load must reconstruct durable history"
    );
    assert!(
        second_replay.is_empty(),
        "a duplicate session/load on the same ACP connection must be idempotent: \
         {second_replay:#?}"
    );
    assert_eq!(
        initialize_after_load, 1,
        "loading durable history must not reconnect configured MCP servers"
    );
    assert_eq!(
        initialize_after_reconfiguration, 1,
        "restored-session mode changes must remain dormant until a real prompt"
    );
    assert_eq!(prompted["stopReason"], "end_turn");
    assert!(
        prompt_updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "ACP reply"
        }),
        "the first prompt after a cold load did not activate and drive the session"
    );
    assert_eq!(
        initialize_after_prompt, 2,
        "the first real prompt must activate the dormant session and reconnect MCP exactly once"
    );
}

fn is_mcp_initialize(request: &Request) -> bool {
    request.method.as_str() == "POST"
        && serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body["method"].as_str().map(str::to_owned))
            .as_deref()
            == Some("initialize")
}

#[test]
fn acp_connection_bounds_open_sessions_and_releases_capacity_on_close() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let mut sessions = Vec::new();
    for index in 0_u64..32 {
        let created = request(
            &mut stdin,
            &mut stdout,
            index + 2,
            "session/new",
            json!({"cwd": root.path(), "mcpServers": []}),
        );
        sessions.push(
            created["sessionId"]
                .as_str()
                .expect("session id")
                .to_owned(),
        );
    }

    let capacity = request_failure(
        &mut stdin,
        &mut stdout,
        34,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    assert_eq!(capacity["code"], -32602);
    assert!(
        capacity["message"]
            .as_str()
            .is_some_and(|message| message.contains("32 open sessions")),
        "capacity failure must explain how to recover: {capacity}"
    );

    request(
        &mut stdin,
        &mut stdout,
        35,
        "session/close",
        json!({"sessionId": &sessions[0]}),
    );
    let replacement = request(
        &mut stdin,
        &mut stdout,
        36,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    assert!(replacement["sessionId"].as_str().is_some());

    drop(stdin);
    let status = child.wait().expect("wait for ACP capacity process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP capacity process failed: {stderr}");
    }
}

#[test]
fn acp_reasoning_levels_follow_the_active_models_declared_variants() {
    let root = tempfile::tempdir().expect("ACP test root");
    let config = reasoning_config();
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("new session id")
        .to_owned();
    let reasoning = created["configOptions"]
        .as_array()
        .and_then(|options| {
            options
                .iter()
                .find(|option| option["id"] == "reasoning_effort")
        })
        .expect("reasoning selector");
    assert_eq!(reasoning["category"], "thought_level");
    assert_eq!(
        reasoning["options"]
            .as_array()
            .expect("reasoning options")
            .iter()
            .filter_map(|option| option["value"].as_str())
            .collect::<Vec<_>>(),
        vec!["default", "low", "xhigh", "max"]
    );

    let selected = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_config_option",
        json!({
            "sessionId": &session_id,
            "configId": "reasoning_effort",
            "value": "xhigh"
        }),
    );
    assert!(selected["configOptions"].as_array().is_some_and(|options| {
        options
            .iter()
            .any(|option| option["id"] == "reasoning_effort" && option["currentValue"] == "xhigh")
    }));

    let plain = request(
        &mut stdin,
        &mut stdout,
        4,
        "session/set_config_option",
        json!({
            "sessionId": &session_id,
            "configId": "model",
            "value": "test/test-model-2"
        }),
    );
    assert!(
        plain["configOptions"].as_array().is_some_and(|options| {
            options
                .iter()
                .all(|option| option["id"] != "reasoning_effort")
        }),
        "a non-reasoning model must remove the stale thought-level selector: {plain}"
    );

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP reasoning process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP reasoning process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_prompt_drives_the_durable_turn_and_streams_updates() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("acp-check.md"),
        "Inspect the ACP integration for $ARGUMENTS.",
    )
    .expect("write project command");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let configured = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "model",
            "value": "test/test-model-2"
        }),
    );
    assert!(
        configured["configOptions"]
            .as_array()
            .is_some_and(|options| {
                options.iter().any(|option| {
                    option["id"] == "model" && option["currentValue"] == "test/test-model-2"
                })
            }),
        "model selection did not rebuild the ACP session: {configured}"
    );
    let (command_completed, command_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "/acp-check src/lib.rs"}]
        }),
    );
    assert_eq!(command_completed["stopReason"], "end_turn");
    assert!(
        command_updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "ACP reply"
        }),
        "configured slash command did not execute a real turn: {command_updates:?}"
    );
    let resource_path = root.path().join("notes.md");
    std::fs::write(&resource_path, "# Design notes\n").expect("write ACP resource link target");
    let resource_uri = format!("file://{}", resource_path.display());
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [
                {"type": "text", "text": "Answer the ACP prompt."},
                {
                    "type": "resource_link",
                    "name": "notes.md",
                    "title": "Design notes",
                    "description": "ACP design context",
                    "mimeType": "text/markdown",
                    "size": 42,
                    "uri": resource_uri
                }
            ]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "ACP reply"
        }),
        "missing streamed assistant text: {updates:?}"
    );
    assert!(
        updates.iter().any(|update| {
            update["sessionUpdate"] == "usage_update"
                && update["used"] == 11
                && update["size"] == 120_000
        }),
        "missing ACP usage projection: {updates:?}"
    );

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(
        received
            .iter()
            .any(|request| { String::from_utf8_lossy(&request.body).contains(&resource_uri) }),
        "resource link was not preserved in the provider request"
    );
    assert!(
        received.iter().any(|request| {
            String::from_utf8_lossy(&request.body)
                .contains("Inspect the ACP integration for src/lib.rs.")
        }),
        "configured slash command was not expanded through the shared command driver"
    );
    assert!(
        received.iter().any(|request| {
            let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
            body["model"] == "test-model-2"
        }),
        "the selected ACP model was not used for the provider request: {received:?}"
    );

    request(
        &mut stdin,
        &mut stdout,
        6,
        "session/close",
        json!({"sessionId": session_id}),
    );
    let (_loaded, replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/load",
        json!({
            "sessionId": session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let resource = replay
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["type"] == "resource_link"
        })
        .expect("session/load did not replay the typed resource link");
    assert_eq!(resource["content"]["name"], "notes.md");
    assert_eq!(resource["content"]["title"], "Design notes");
    assert_eq!(resource["content"]["description"], "ACP design context");
    assert_eq!(resource["content"]["mimeType"], "text/markdown");
    assert_eq!(resource["content"]["size"], 42);
    assert_eq!(resource["content"]["uri"], resource_uri);

    drop(stdin);
    let status = child.wait().expect("wait for ACP prompt process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP prompt process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_images_and_embedded_context_reach_the_provider_and_durable_replay() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let config = rich_prompt_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let selection_uri = "zed://selection/src/lib.rs#L1-L3";
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "data": "iVBORw0KGgo=",
                    "uri": "file:///tmp/screenshot.png"
                },
                {
                    "type": "resource",
                    "resource": {
                        "uri": selection_uri,
                        "mimeType": "text/rust",
                        "text": "fn selected() {}"
                    }
                },
                {"type": "text", "text": "Review this context."}
            ]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk" && update["content"]["text"] == "ACP reply"
    }));

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(received.iter().any(|request| {
        let body = String::from_utf8_lossy(&request.body);
        body.contains("data:image/png;base64,iVBORw0KGgo=")
            && body.contains(selection_uri)
            && body.contains("fn selected() {}")
    }));

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    let (_loaded, replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert!(replay.iter().any(|update| {
        update["sessionUpdate"] == "user_message_chunk"
            && update["content"]["type"] == "image"
            && update["content"]["mimeType"] == "image/png"
    }));
    assert!(replay.iter().any(|update| {
        update["sessionUpdate"] == "user_message_chunk"
            && update["content"]["type"] == "text"
            && update["content"]["text"].as_str().is_some_and(|text| {
                text.contains(selection_uri) && text.contains("fn selected() {}")
            })
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP rich prompt process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP rich prompt process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_prompt_round_trips_question_tool_through_stable_elicitation() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(QuestionTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "elicitation": {
                    "form": {}
                }
            }
        }),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let (completed, updates, elicitation) = request_with_elicitation(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type": "text", "text": "Choose the database."}]
        }),
        &session_id,
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert_eq!(elicitation["toolCallId"], "call_question");
    assert!(
        updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "Configured SQLite"
        }),
        "missing streamed assistant text after elicitation: {updates:?}"
    );

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(
        received.iter().any(|request| {
            let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "tool"
                        && message["tool_call_id"] == "call_question"
                        && message["content"].as_str().is_some_and(|content| {
                            content.contains(r#""Which database?"="SQLite""#)
                        })
                })
            })
        }),
        "provider never received the accepted question answer: {received:?}"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP HITL process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP HITL process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_prompt_projects_the_final_durable_plan_before_responding() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(PlanTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Create a durable plan."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|update| {
            update["toolCallId"] == "call_plan"
                && update["rawInput"]["title"] == "Verify ACP projection"
        }),
        "plan tool input was not projected: {updates:#?}"
    );
    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan")
        .expect("session/prompt returned without the durable plan snapshot");
    assert_eq!(plan["entries"].as_array().map(Vec::len), Some(2));
    assert_eq!(plan["entries"][0]["content"], "Implement plan projection");
    assert_eq!(plan["entries"][0]["status"], "in_progress");
    assert_eq!(plan["entries"][1]["status"], "pending");
    assert_eq!(plan["_meta"]["zuno"]["title"], "Verify ACP projection");

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(received.iter().any(|request| {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        body["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call_plan")
        })
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP plan process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP plan process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_write_round_trips_strict_permission_and_native_creation_diff() {
    let root = tempfile::tempdir().expect("ACP test root");
    let target = root.path().join("created.txt");
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(WriteTurnResponder {
            path: target.to_string_lossy().into_owned(),
        })
        .mount(&provider)
        .await;
    let config = strict_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let (completed, updates, permissions) = request_with_permissions(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"Create the requested file."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert_eq!(
        permissions.len(),
        1,
        "unexpected permission flow: {permissions:#?}"
    );
    let permission = &permissions[0];
    assert_eq!(permission["sessionId"], session_id);
    assert_eq!(permission["toolCall"]["toolCallId"], "call_write");
    assert_eq!(permission["toolCall"]["kind"], "edit");
    assert_eq!(permission["toolCall"]["rawInput"]["permission"], "edit");
    assert!(permission["options"].as_array().is_some_and(|options| {
        options
            .iter()
            .any(|option| option["optionId"] == "allow_once")
    }));
    assert_eq!(
        std::fs::read_to_string(&target).expect("created file"),
        "created through ACP\n"
    );
    assert!(updates.iter().any(|update| {
        update["toolCallId"] == "call_write"
            && update["rawInput"]["filePath"] == target.to_string_lossy().as_ref()
    }));
    let written = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call_write"
                && update["status"] == "completed"
                && update["content"].is_array()
        })
        .expect("completed write projection");
    let diff = written["content"]
        .as_array()
        .expect("write content")
        .iter()
        .find(|content| content["type"] == "diff")
        .expect("native creation diff");
    assert_eq!(diff["path"], target.to_string_lossy().as_ref());
    assert_eq!(diff["oldText"], Value::Null);
    assert_eq!(diff["newText"], "created through ACP\n");
    assert!(written["locations"].as_array().is_some_and(|locations| {
        locations
            .iter()
            .any(|location| location["path"] == target.to_string_lossy().as_ref())
    }));
    assert!(updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"] == "File created"
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP write process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP write process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_danger_full_access_emits_no_tool_approval_request() {
    let root = tempfile::tempdir().expect("ACP test root");
    let target = root.path().join("created-without-approval.txt");
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(WriteTurnResponder {
            path: target.to_string_lossy().into_owned(),
        })
        .mount(&provider)
        .await;
    let config = danger_full_access_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let (completed, _updates, permissions) = request_with_permissions(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Create the requested file."}]
        }),
    );

    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        permissions.is_empty(),
        "danger-full-access must not emit ACP approval requests: {permissions:#?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("created file"),
        "created through ACP\n"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_compact_is_native_and_persists_a_summary_without_model_prompt_dispatch() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("compact.md"),
        "This user command must never shadow native compaction.",
    )
    .expect("write colliding project command");
    let config = manual_compaction_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let commands = read_session_update(&mut stdout);
    let compact = commands["update"]["availableCommands"]
        .as_array()
        .expect("available commands")
        .iter()
        .filter(|command| command["name"] == "compact")
        .collect::<Vec<_>>();
    assert_eq!(compact.len(), 1);
    assert_eq!(
        compact[0]["description"],
        "Summarize older context and keep the recent turn tail"
    );

    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    seed_compactable_history(root.path(), &session_id);
    let (_loaded, _replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let (completed, _updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/compact"}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert_eq!(
        received.len(),
        1,
        "native compaction must make exactly its summary request"
    );
    let request_body = String::from_utf8_lossy(&received[0].body);
    assert!(
        !request_body.contains("/compact"),
        "native command leaked into the model prompt: {request_body}"
    );
    assert!(
        request_body.contains("## Objective"),
        "compaction summary prompt was not used: {request_body}"
    );

    let connection = zuno_db::open::open(&zuno_paths::DbLocation::File(
        root.path().join("zuno-acp.db"),
    ))
    .expect("open compacted ACP database");
    let history = zuno_db::message::MessageStore::new(&connection)
        .hydrate_session(&session_id)
        .expect("hydrate compacted session");
    assert!(
        history
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| { part.kind == zuno_db::message::PartKind::Compaction })
    );
    assert!(history.iter().any(|message| {
        message.info.data.get("summary") == Some(&json!(true))
            && message.info.data.get("finish") == Some(&json!("stop"))
            && message
                .parts
                .iter()
                .any(|part| part.data.get("text") == Some(&json!("ACP title")))
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP compact process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP compact process failed: {stderr}");
    }
}

#[test]
fn acp_goal_and_plan_commands_are_native_and_do_not_enter_model_input() {
    let root = tempfile::tempdir().expect("temporary ACP root");
    let config = danger_full_access_config("https://example.invalid");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let commands = read_session_update(&mut stdout);
    let native = commands["update"]["availableCommands"]
        .as_array()
        .expect("available commands")
        .iter()
        .take(5)
        .map(|command| command["name"].as_str().expect("command name"))
        .collect::<Vec<_>>();
    assert_eq!(
        native,
        ["compact", "goal", "plan", "start-plan", "start-work"]
    );

    let (goal, goal_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/goal create ship ACP commands"}]
        }),
    );
    assert_eq!(goal["stopReason"], "end_turn");
    assert!(goal_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("ship ACP commands"))
    }));

    let (plan, plan_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/plan"}]
        }),
    );
    assert_eq!(plan["stopReason"], "end_turn");
    assert!(plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "current_mode_update" && update["currentModeId"] == "plan"
    }));
    assert!(plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "config_option_update" && update["configOptions"].is_array()
    }));

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP native command process failed: {stderr}");
    }
}

#[test]
fn acp_load_replays_durable_content_tools_plan_and_usage() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    seed_durable_replay(root.path(), &session_id);

    let (loaded, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert_eq!(loaded["modes"]["currentModeId"], "build");
    let kinds = updates
        .iter()
        .filter_map(|update| update["sessionUpdate"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "user_message_chunk",
            "user_message_chunk",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "plan",
            "usage_update",
        ],
        "session/load must replay every durable client-visible projection in order: {updates:#?}"
    );
    assert_eq!(updates[0]["messageId"], "msg_acp_load_user");
    assert_eq!(updates[0]["content"]["text"], "replay this durable session");
    assert_eq!(updates[1]["content"]["type"], "image");
    assert_eq!(updates[1]["content"]["mimeType"], "image/png");
    assert_eq!(updates[2]["messageId"], "msg_acp_load_assistant");
    let completed = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("completed tool replay");
    let content = completed["content"].as_array().expect("tool content");
    let diff = content
        .iter()
        .find(|item| item["type"] == "diff")
        .expect("typed ACP diff");
    assert_eq!(diff["oldText"], "old\n");
    assert_eq!(diff["newText"], "new\n");
    assert!(
        diff["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("/src/lib.rs")),
        "typed diff path must be absolute: {diff}"
    );
    assert!(content.iter().any(|item| {
        item["type"] == "content"
            && item["content"]["type"] == "image"
            && item["content"]["mimeType"] == "image/png"
    }));
    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan")
        .expect("plan replay");
    assert_eq!(plan["entries"][0]["status"], "in_progress");
    assert_eq!(plan["entries"][1]["status"], "pending");
    let usage = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "usage_update")
        .expect("usage replay");
    assert_eq!(usage["used"], 175);
    assert_eq!(usage["size"], 100_000);
    assert_eq!(usage["cost"], json!({"amount":1.25,"currency":"USD"}));

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    let (resumed, resume_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        6,
        "session/resume",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert_eq!(resumed["modes"]["currentModeId"], "build");
    assert!(
        resume_updates.is_empty(),
        "session/resume must not duplicate durable history: {resume_updates:#?}"
    );
    let (_loaded_after_resume, replay_after_resume) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert!(
        replay_after_resume.is_empty(),
        "a resumed client already owns the transcript and a later load must not duplicate it: \
         {replay_after_resume:#?}"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP load process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP load process failed: {stderr}");
    }
}
