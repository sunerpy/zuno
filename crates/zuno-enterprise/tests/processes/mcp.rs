use super::*;
use zuno_application::mcp::McpToolBinding;
use zuno_types::activity::ActivityName;

pub fn binding(endpoint: &str) -> McpToolBinding {
    McpToolBinding {
        endpoint: endpoint.to_owned(),
        connection: ActivityName::new("fixture").unwrap(),
        server: ActivityName::new("external").unwrap(),
        tool: ActivityName::new("apply").unwrap(),
        revision: 1,
        definition: json!({"name":"apply","description":"Return the authenticated external owner",
            "inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]},
            "annotations":{"readOnlyHint":true}}),
    }
}
pub async fn endpoint(
    State(issuer): State<Arc<Issuer>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let owner = match headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some("Bearer mcp-alice") => "alice",
        Some("Bearer mcp-bob") => "bob",
        _ => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    if body["method"] != "initialize" {
        assert_eq!(headers["mcp-session-id"], format!("mcp-{owner}"));
    }
    let result = match body["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
        }
        "notifications/initialized" => return axum::http::StatusCode::ACCEPTED.into_response(),
        "tools/list" => json!({"tools":[binding(&format!("{}/mcp",issuer.origin)).definition]}),
        "tools/call" => {
            assert_eq!(body["params"]["name"], "apply");
            assert_eq!(body["params"]["arguments"]["value"], owner);
            issuer.mcp_calls.fetch_add(1, Ordering::SeqCst);
            json!({"content":[{"type":"text","text":format!("MCP-OWNER-{owner}")}],"isError":false})
        }
        _ => panic!("unsupported MCP fixture method"),
    };
    let mut response =
        Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result})).into_response();
    response
        .headers_mut()
        .insert("mcp-session-id", format!("mcp-{owner}").parse().unwrap());
    response
}
pub fn model(body: &Value, name: &str, issuer: &Issuer) -> Response {
    if let Some(result) = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "mcp-call")
    {
        assert!(
            result["content"]
                .as_str()
                .unwrap()
                .contains(&format!("MCP-OWNER-{name}")),
            "MCP result: {}",
            result["content"]
        );
        return model_response(json!({"role":"assistant","content":"MCP-VERIFIED"}), true);
    }
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"]
                == binding(&format!("{}/mcp", issuer.origin)).wire_name())
    );
    model_response(
        json!({"role":"assistant","tool_calls":[{"index":0,"id":"mcp-call","type":"function",
        "function":{"name":binding(&format!("{}/mcp",issuer.origin)).wire_name(),"arguments":json!({"value":name}).to_string()}}]}),
        false,
    )
}
pub async fn verify(
    http: &reqwest::Client,
    control: &str,
    alice: &str,
    bob: &str,
    issuer: &Issuer,
) {
    for (name, token, other) in [("alice", alice, bob), ("bob", bob, alice)] {
        let session:Value=http.post(format!("{control}api/v1/sessions")).bearer_auth(token)
            .json(&json!({"requestId":format!("mcp-{name}"),"workspaceId":"workspace","title":"MCP proof"}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let job:Value=http.post(format!("{control}api/v1/sessions/{}/turns",session["id"].as_str().unwrap()))
            .bearer_auth(token).json(&json!({"requestId":"mcp-turn","expectedInputVersion":"0","text":format!("MCP-PROBE {name}")}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let job = job["id"].as_str().unwrap();
        let before = issuer.mcp_calls.load(Ordering::SeqCst);
        let mut approved = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
        loop {
            let current: Value = http
                .get(format!("{control}api/v1/jobs/{job}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if current["phase"] == "completed" {
                break;
            }
            assert!(
                matches!(
                    current["phase"].as_str(),
                    Some("ready" | "running" | "waiting")
                ),
                "MCP job failed: {current}"
            );
            for wait in current["waits"].as_array().unwrap() {
                if wait["target"]["kind"] != "approval" || approved {
                    continue;
                }
                assert_eq!(
                    issuer.mcp_calls.load(Ordering::SeqCst),
                    before,
                    "readOnlyHint must not bypass human approval"
                );
                let id = wait["target"]["approval_id"].as_str().unwrap();
                let url = format!("{control}api/v1/approvals/{id}/mcp");
                assert_eq!(
                    http.get(&url)
                        .bearer_auth(other)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    if name == "alice" {
                        reqwest::StatusCode::NOT_FOUND
                    } else {
                        reqwest::StatusCode::OK
                    }
                );
                if name == "bob" {
                    // Alice is the fixture's organization administrator. Audit
                    // visibility does not authorize answering a requester-only approval.
                    assert_eq!(
                        http.post(format!("{control}api/v1/approvals/{id}/answer"))
                            .bearer_auth(other)
                            .json(&json!({"requestId":"admin-cannot-answer","answer":"approve"}))
                            .send()
                            .await
                            .unwrap()
                            .status(),
                        reqwest::StatusCode::FORBIDDEN
                    );
                }
                let review: Value = http
                    .get(&url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(review["arguments"], json!({"value":name}));
                assert_eq!(
                    review["definition"],
                    binding(&format!("{}/mcp", issuer.origin)).definition
                );
                assert!(!review.to_string().contains("lease"));
                http.post(format!("{control}api/v1/approvals/{id}/answer"))
                    .bearer_auth(token)
                    .json(&json!({"requestId":"approve-mcp","answer":"approve"}))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                approved = true;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "MCP deadline: {current}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(approved);
        assert_eq!(issuer.mcp_calls.load(Ordering::SeqCst), before + 1);
    }
}

pub async fn verify_provider(fixture: &Fixture, root: &Path, issuer: &Issuer, owner: PrincipalKey) {
    use zuno_application::mcp::{McpConnectionProvider, McpFailure};
    let provider = zuno_enterprise::mcp::ConfiguredMcpConnections::new(vec![
        zuno_enterprise::mcp::McpConnectionConfig {
            owner: owner.clone(),
            connection: ActivityName::new("fixture").unwrap(),
            revision: 1,
            endpoint: format!("{}/mcp", issuer.origin),
            access_token_file: root.join("mcp-alice.key"),
            root_certificate: Some(fixture.root_certificate.clone()),
            timeout_millis: 10000,
        },
    ])
    .await
    .unwrap();
    let before = issuer.mcp_calls.load(Ordering::SeqCst);
    let target = binding(&format!("{}/mcp", issuer.origin));
    let mut changed = target.clone();
    changed.definition["description"] = json!("a different reviewed declaration");
    assert!(matches!(
        provider.prepare(&owner, &changed).await,
        Err(McpFailure::DefinitionChanged)
    ));
    changed = target.clone();
    changed.endpoint.push_str("/changed");
    assert!(matches!(
        provider.prepare(&owner, &changed).await,
        Err(McpFailure::AuthorizationRevoked)
    ));
    changed = target.clone();
    changed.revision += 1;
    assert!(matches!(
        provider.prepare(&owner, &changed).await,
        Err(McpFailure::AuthorizationRevoked)
    ));
    let mut foreign = owner.clone();
    foreign.principal_id = PrincipalId::new("foreign").unwrap();
    assert!(matches!(
        provider.prepare(&foreign, &target).await,
        Err(McpFailure::AuthorizationRevoked)
    ));
    assert_eq!(
        issuer.mcp_calls.load(Ordering::SeqCst),
        before,
        "declaration/owner/target mismatches must never invoke a tool"
    );
}
