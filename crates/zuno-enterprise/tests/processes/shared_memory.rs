use super::*;
pub fn model(body: &Value, user: &str) -> Response {
    let checkpoint = user.contains("SHARED-CHECKPOINT");
    let resumed =
        body["messages"].as_array().unwrap().iter().any(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "shared-checkpoint"
        });
    let prompt = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|value| matches!(value["role"].as_str(), Some("system" | "developer")))
        .map(|value| value["content"].to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        prompt.contains("SHARED-RUNBOOK-APPROVED"),
        user.contains("SHARED-EXPECTED") || (checkpoint && !resumed),
        "shared prompt: {prompt}"
    );
    if checkpoint && !resumed {
        return model_response(
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"shared-checkpoint","type":"function",
            "function":{"name":"environment_command","arguments":json!({"argv":["/bin/echo","shared-checkpoint"]}).to_string()}}]}),
            false,
        );
    }
    model_response(
        json!({"role":"assistant","content":"SHARED-MEMORY-VERIFIED"}),
        true,
    )
}
pub async fn verify(
    http: &reqwest::Client,
    control: &str,
    alice: &str,
    bob: &str,
    identities: &BTreeMap<&str, Value>,
) {
    let space_url = format!("{control}api/v1/memory/spaces/runbooks");
    let mut configuration = json!({"requestId":"shared-space","expectedRevision":"0","title":"Deployment runbooks",
        "workspaceId":"workspace","enabled":true,"characterLimit":3000,"members":[
            {"principalId":identities["alice"]["principalId"],"role":"contributor"},
            {"principalId":identities["bob"]["principalId"],"role":"reviewer"}]});
    let response = http
        .put(&space_url)
        .bearer_auth(alice)
        .json(&configuration)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "configure shared: {}",
        response.text().await.unwrap()
    );
    let candidate:Value=http.post(format!("{space_url}/changes")).bearer_auth(alice)
        .json(&json!({"requestId":"shared-proposal","expectedRevision":"1","reason":"Publish the approved runbook.",
            "edits":[{"kind":"add","content":"SHARED-RUNBOOK-APPROVED"}]}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let decision = json!({"requestId":"shared-approve","changeId":candidate["id"],"expectedState":candidate["stateDigest"],"decision":"apply"});
    assert_eq!(
        http.post(format!("{space_url}/review"))
            .bearer_auth(alice)
            .json(&decision)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    http.post(format!("{space_url}/review"))
        .bearer_auth(bob)
        .json(&decision)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    // Earlier fixture phases disabled Bob's Memory. Sharing must honor that
    // choice until the user explicitly enables recall.
    turn(http, control, bob, "SHARED-REVOKED bob", "memory-disabled").await;
    let memory_url = format!("{control}api/v1/workspaces/workspace/memory");
    let policy: Value = http
        .post(&memory_url)
        .bearer_auth(bob)
        .json(
            &json!({"requestId":"shared-read-policy","command":{"kind":"policy","sessionId":null}}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let enabled:Value=http.post(&memory_url).bearer_auth(bob)
        .json(&json!({"requestId":"shared-enable-use","command":{"kind":"set_policy","sessionId":null,
            "expectedRevision":policy["result"]["Ok"]["policy"]["revision"],"useMemories":true,"generatePrivate":false}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert!(enabled["result"]["Ok"].is_object(), "{enabled}");
    turn(http, control, bob, "SHARED-EXPECTED bob", "before-revoke").await;
    let waiting = start(http, control, bob, "SHARED-CHECKPOINT bob", "checkpoint").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let approval = loop {
        let current: Value = http
            .get(&waiting)
            .bearer_auth(bob)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(approval) = current["waits"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|wait| wait["target"]["approval_id"].as_str().map(str::to_owned))
        {
            break approval;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no persisted approval wait: {current}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    configuration["requestId"] = json!("shared-revoke");
    configuration["expectedRevision"] = json!("1");
    configuration["members"] =
        json!([{"principalId":identities["alice"]["principalId"],"role":"reviewer"}]);
    http.put(&space_url)
        .bearer_auth(alice)
        .json(&configuration)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        http.get(&space_url)
            .bearer_auth(bob)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    http.post(format!("{control}api/v1/approvals/{approval}/answer"))
        .bearer_auth(bob)
        .json(&json!({"requestId":"resume-shared-checkpoint","answer":"approve"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    complete(http, &waiting, bob).await;
    turn(http, control, bob, "SHARED-REVOKED bob", "after-revoke").await;
}
async fn turn(http: &reqwest::Client, control: &str, token: &str, text: &str, key: &str) {
    let url = start(http, control, token, text, key).await;
    complete(http, &url, token).await;
}
pub(super) async fn start(
    http: &reqwest::Client,
    control: &str,
    token: &str,
    text: &str,
    key: &str,
) -> String {
    let session: Value = http
        .post(format!("{control}api/v1/sessions"))
        .bearer_auth(token)
        .json(&json!({"requestId":key,"workspaceId":"workspace","title":"Shared Memory proof"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let job: Value = http
        .post(format!(
            "{control}api/v1/sessions/{}/turns",
            session["id"].as_str().unwrap()
        ))
        .bearer_auth(token)
        .json(&json!({"requestId":"turn","expectedInputVersion":"0","text":text}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    format!("{control}api/v1/jobs/{}", job["id"].as_str().unwrap())
}
pub(super) async fn complete(http: &reqwest::Client, url: &str, token: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let current: Value = http
            .get(url)
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
                Some("running" | "ready" | "waiting")
            ),
            "shared Memory job: {current}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "shared Memory deadline"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}
