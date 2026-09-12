use super::*;

pub fn definition(parent: &Definition) -> Definition {
    let mut value = parent.clone();
    value.id = ConfigurationId::new("completion").unwrap();
    value.workspace.id = WorkspaceId::new("completion-workspace").unwrap();
    value.agent.name = "completion".to_owned();
    value.agent.mode = AgentExecutionMode::Completion;
    value.agent.system_prompt = "Answer only from the supplied input.".to_owned();
    value.environment = None;
    value.delegation = None;
    value.workflows.clear();
    value
}

pub async fn verify(http: &reqwest::Client, control: &str, token: &str) {
    let memory = format!("{control}api/v1/workspaces/completion-workspace/memory");
    let proposal: Value = http.post(&memory).bearer_auth(token).json(&json!({
        "requestId":"completion-memory-proposal","command":{"kind":"propose","change":{
            "scope":"project","action":"add","content":"MEMORY-NOT-FOR-COMPLETION",
            "oldText":null,"reason":"Test completion isolation","expectedRevision":null,"confidence":1.0
        }}
    })).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let candidate = &proposal["result"]["Ok"];
    let policy: Value = http.post(&memory).bearer_auth(token)
        .json(&json!({"requestId":"completion-policy-read","command":{"kind":"policy","sessionId":null}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let revision = policy["result"]["Ok"]["policy"]["revision"]
        .as_u64()
        .unwrap();
    for (id, command) in [
        (
            "completion-memory-apply",
            json!({"kind":"apply","candidateId":candidate["candidate"]["id"],"expectedState":candidate["stateDigest"]}),
        ),
        (
            "completion-memory-use",
            json!({"kind":"set_policy","sessionId":null,"expectedRevision":revision,"useMemories":true,"generatePrivate":false}),
        ),
    ] {
        let response: Value = http
            .post(&memory)
            .bearer_auth(token)
            .json(&json!({"requestId":id,"command":command}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response["result"]["Ok"].is_object(), "{response}");
    }
    let session: Value = http.post(format!("{control}api/v1/sessions")).bearer_auth(token)
        .json(&json!({"requestId":"completion-session","workspaceId":"completion-workspace","title":"Internal completion"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let job: Value = http.post(format!("{control}api/v1/sessions/{}/turns",session["id"].as_str().unwrap())).bearer_auth(token)
        .json(&json!({"requestId":"completion-turn","expectedInputVersion":"0","text":"COMPLETION-PROBE alice"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let state: Value = http
            .get(format!(
                "{control}api/v1/jobs/{}",
                job["id"].as_str().unwrap()
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if state["phase"] == "completed" {
            break;
        }
        assert!(
            matches!(state["phase"].as_str(), Some("ready" | "running")),
            "completion failed: {state}"
        );
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
