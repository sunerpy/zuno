//! A real CLI, provider and MCP transport prove first-request schema exposure.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use zuno_testkit::{DbChoice, ScriptedEnv};

fn completion(delta: Value, finish: &str) -> ResponseTemplate {
    let chunk = json!({
        "id": "mcp-exposure", "object": "chat.completion.chunk", "created": 1,
        "model": "test-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
    });
    let end = json!({
        "id": "mcp-exposure", "object": "chat.completion.chunk", "created": 1,
        "model": "test-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
    });
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n"))
}

#[tokio::test]
async fn deep_uses_small_host_mcp_on_its_first_request_without_tool_search() {
    let server = MockServer::start().await;
    let wire_name = zuno_mcp::tool_name("aws-knowledge-mcp-server", "search_documentation");
    let called_name = wire_name.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("provider JSON");
            let has_tools = body["tools"].as_array().is_some_and(|tools| !tools.is_empty());
            let has_result = body["messages"].as_array().is_some_and(|messages| messages.iter()
                .any(|message| message["role"] == "tool" && message["tool_call_id"] == "mcp-direct-call"));
            if has_tools && !has_result {
                completion(json!({"role": "assistant", "tool_calls": [{
                    "index": 0, "id": "mcp-direct-call", "type": "function",
                    "function": {"name": called_name, "arguments": "{\"query\":\"Amazon DCV TCP\"}"}
                }]}), "tool_calls")
            } else {
                completion(json!({"role": "assistant", "content":
                    if has_result { "MCP_DIRECT_DONE" } else { "DCV documentation" }
                }), "stop")
            }
        })
        .mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("initialize");
            ResponseTemplate::new(200)
                .insert_header("mcp-session-id", "native-mcp-session")
                .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": {
                    "protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "AWS documentation fixture", "version": "1"}
                }}))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/mcp"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("list");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": body["id"], "result": {"tools": [{
                    "name": "search_documentation", "description": "Search official AWS documentation",
                    "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}},
                        "required": ["query"], "additionalProperties": false}
                }]}
            }))
        }).expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            json!({"method": "tools/call", "params": {
                "name": "search_documentation", "arguments": {"query": "Amazon DCV TCP"}
            }}),
        ))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("call");
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": body["id"],
                "result": {"content": [{"type": "text", "text": "Native MCP documentation result"}]}
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let config = json!({
        "formatter": false, "lsp": false, "memory": false, "snapshot": false,
        "model": "test/test-model", "small_model": "test/test-model",
        "permission": {"mode": "allow_all"}, "sandbox": {"mode": "danger-full-access"},
        "provider": {"test": {
            "name": "MCP fixture", "id": "test", "env": [], "transport": "openai-compatible",
            "models": {"test-model": {
                "id": "test-model", "name": "Test", "attachment": false, "reasoning": false,
                "temperature": false, "tool_call": true, "release_date": "2026-01-01",
                "limit": {"context": 100000, "output": 10000},
                "cost": {"input": 0, "output": 0}, "options": {}
            }},
            "options": {"apiKey": "fixture", "baseURL": format!("{}/v1", server.uri())}
        }},
        "mcp": {"aws-knowledge-mcp-server": {
            "type": "remote", "url": format!("{}/mcp", server.uri()), "oauth": false, "enabled": true
        }}
    });
    let mut variables = env.env_vars().into_iter().collect::<BTreeMap<_, _>>();
    variables.extend([
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), config.to_string()),
        ("NO_COLOR".to_owned(), "1".to_owned()),
    ]);
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_zuno"));
    command
        .args([
            "run",
            "--agent",
            "deep",
            "Find official guidance for Amazon DCV TCP.",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("bounded CLI")
        .expect("CLI starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("MCP_DIRECT_DONE"));
    let requests = server.received_requests().await.expect("captured traffic");
    let first = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .map(|request| serde_json::from_slice::<Value>(&request.body).expect("request"))
        .find(|body| {
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        })
        .expect("first main provider request");
    let names = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&wire_name.as_str()), "{names:?}");
    assert!(
        !names.contains(&"tool_search"),
        "one small service must not require a discovery round-trip"
    );
}
