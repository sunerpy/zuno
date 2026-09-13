use super::*;
#[path = "skill/installation.rs"]
mod installation;

pub fn model(body: &Value) -> Option<Response> {
    if let Some(response) = installation::model(body) {
        return Some(response);
    }
    let messages = body["messages"].as_array()?;
    let system = messages
        .iter()
        .filter(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    if system.contains("Grade the actual attempt and tool trace") {
        let user = messages.iter().find(|m| m["role"] == "user")?["content"].as_str()?;
        let input: Value = serde_json::from_str(user).unwrap();
        let passed = input["actualAnswer"]
            .as_str()
            .unwrap()
            .contains("SKILL-RECORDED-SUCCESS");
        return Some(model_response(
            json!({"role":"assistant","content":json!({
            "score":if passed{100}else{10},"passed":passed,"criticalFailure":false,"explanation":"Checked the recorded attempt."
        }).to_string()}),
            true,
        ));
    }
    if !system.contains("Complete the task using the supplied Skill and available recorded tools.")
    {
        return None;
    }
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["function"]["name"] == "recorded_read")
    );
    if system.contains("BASELINE-SKILL") {
        return Some(model_response(
            json!({"role":"assistant","content":"Baseline omitted the recorded evidence."}),
            true,
        ));
    }
    if let Some(result) = messages.iter().find(|m| m["role"] == "tool") {
        assert_eq!(result["content"], "SKILL-RECORDED-SUCCESS");
        Some(model_response(
            json!({"role":"assistant","content":"SKILL-RECORDED-SUCCESS"}),
            true,
        ))
    } else {
        Some(model_response(
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"skill-recorded","type":"function",
            "function":{"name":"recorded_read","arguments":"{\"path\":\"build.log\"}"}}]}),
            false,
        ))
    }
}

pub async fn verify(
    http: &reqwest::Client,
    control: &str,
    alice: &str,
    bob: &str,
    source_job: &str,
    admin: &sqlx_postgres::PgPool,
) {
    let memory = format!("{control}api/v1/workspaces/workspace/memory");
    let policy:Value=http.post(&memory).bearer_auth(alice)
        .json(&json!({"requestId":"skill-memory-policy","command":{"kind":"policy","sessionId":null}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let disabled:Value=http.post(&memory).bearer_auth(alice)
        .json(&json!({"requestId":"skill-disable-automatic-memory","command":{"kind":"set_automation","sessionId":null,
            "expectedRevision":policy["result"]["Ok"]["policy"]["revision"],"enabled":false}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(
        disabled["result"]["Ok"]["policy"]["automaticPrivate"],
        false
    );
    let before: i64 =
        query_scalar("SELECT count(*) FROM zuno_enterprise_preview.gateway_operation")
            .fetch_one(admin)
            .await
            .unwrap();
    let candidate:Value=http.post(format!("{control}api/v1/jobs/{source_job}/skills")).bearer_auth(alice)
        .json(&json!({"requestId":"skill-proposal","name":"recorded-proof","baselineContent":"BASELINE-SKILL",
            "proposedContent":"CANDIDATE-SKILL: read the recorded proof before answering.",
            "cases":[{"id":"case-one","prompt":"Use the recorded build proof.","expected":"Report SKILL-RECORDED-SUCCESS only from evidence.",
                "kind":"failure","weight":1,"calls":[{"name":"recorded_read","arguments":{"path":"build.log"},
                "output":"SKILL-RECORDED-SUCCESS","isError":false}]}]}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(candidate["state"], "pending_review");
    assert!(
        candidate["jobId"].is_null(),
        "proposing must not launch a model"
    );
    let id = candidate["id"].as_str().unwrap();
    let url = format!("{control}api/v1/skills/{id}");
    assert_eq!(
        http.get(&url)
            .bearer_auth(bob)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let review = json!({"requestId":"skill-review","expectedDigest":candidate["digest"]});
    let queued: Value = http
        .post(format!("{url}/evaluate"))
        .bearer_auth(alice)
        .json(&review)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let duplicate: Value = http
        .post(format!("{url}/evaluate"))
        .bearer_auth(alice)
        .json(&review)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(queued["jobId"], duplicate["jobId"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        let value: Value = http
            .get(&url)
            .bearer_auth(alice)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if value["state"] == "passed" {
            assert_eq!(value["report"]["baselineMetric"], 10);
            assert_eq!(value["report"]["candidateMetric"], 100);
            assert_eq!(
                value["report"]["cases"][0]["candidate"]["details"]["liveTools"],
                false
            );
            break;
        }
        assert_eq!(
            value["state"], "evaluating",
            "Skill evaluation failed: {value}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "Skill evaluation deadline: {value}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.gateway_operation")
            .fetch_one(admin)
            .await
            .unwrap(),
        before,
        "cassette calls must never execute an environment command"
    );
    let requests: i64 = query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.learning_model_request WHERE job_id=$1",
    )
    .bind(queued["jobId"].as_str().unwrap())
    .fetch_one(admin)
    .await
    .unwrap();
    assert_eq!(
        requests, 5,
        "baseline attempt/grade and candidate recorded-tool attempt/final/grade"
    );
    installation::verify(http, control, alice, bob, &candidate).await;
}
