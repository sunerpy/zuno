//! Public-API HITL control of the real distributed DAG; providers remain fixtures.
use super::*;

pub fn template() -> zuno_orchestration::WorkflowTemplateDescriptor {
    zuno_orchestration::WorkflowTemplateDescriptor {
        name: "inspection".to_owned(),
        source_id: "fixture:inspection".to_owned(),
        max_parallel: 2,
        max_agents: 3,
        nodes: [
            ("fast", vec![]),
            ("slow", vec![]),
            ("after-fast", vec!["fast".to_owned()]),
        ]
        .into_iter()
        .map(
            |(id, depends_on)| zuno_orchestration::WorkflowNodeDescriptor {
                id: id.to_owned(),
                agent: "workspace-helper".to_owned(),
                prompt: Some(format!("Execute node {id}")),
                description: None,
                depends_on,
            },
        )
        .collect(),
    }
}

pub fn model(body: &Value) -> Response {
    let messages = body["messages"].as_array().unwrap();
    let text = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .to_string();
    let child = messages
        .iter()
        .filter(|message| message["role"] == "system" || message["role"] == "developer")
        .any(|message| message.to_string().contains("CHILD-EXECUTOR"));
    let has = |id: &str| {
        messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == id)
    };
    let (delta, completed) = if child {
        let node = if text.contains("Execute node after-fast") {
            "AFTER-FAST"
        } else if text.contains("Execute node slow") {
            "SLOW"
        } else {
            "FAST"
        };
        if node == "AFTER-FAST" {
            assert!(
                text.contains("NODE-FAST-RESULT"),
                "dependent input lacks its predecessor's result"
            );
            assert!(text.contains("Workflow dependency results"));
        }
        if has("workflow-node-command") {
            (
                json!({"role":"assistant","content":format!("NODE-{node}-RESULT")}),
                true,
            )
        } else {
            (
                json!({"role":"assistant","tool_calls":[{
                    "index":0,"id":"workflow-node-command","type":"function","function":{
                        "name":"environment_command","arguments":json!({"argv":["sh","-c",
                            format!("test \"$(cat /workspace/workflow-base)\" = workflow-base && printf '{node}' > /workspace/node-output && cat /workspace/node-output")
                        ]}).to_string()
                    }
                }]}),
                false,
            )
        }
    } else if !has("workflow-base-command") {
        (
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":"workflow-base-command","type":"function","function":{
                    "name":"environment_command","arguments":json!({"argv":["sh","-c","printf 'workflow-base\\n' > /workspace/workflow-base"]}).to_string()
                }
            }]}),
            false,
        )
    } else if !has("workflow-run") {
        (
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":"workflow-run","type":"function","function":{
                    "name":"workflow","arguments":json!({"workflow":"inspection","prompt":"WORKFLOW-PROBE alice","description":"Native workflow"}).to_string()
                }
            }]}),
            false,
        )
    } else {
        let output = messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "workflow-run")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(
            output.contains("NODE-AFTER-FAST-RESULT"),
            "unexpected original workflow result: {output}"
        );
        assert!(output.contains("\"status\":\"completed\""));
        (
            json!({"role":"assistant","content":"WORKFLOW-COMPLETE"}),
            true,
        )
    };
    super::model_response(delta, completed)
}

pub async fn verify(http: &reqwest::Client, control: &str, alice: &str, bob: &str) {
    let session: Value = http.post(format!("{control}api/v1/sessions")).bearer_auth(alice)
        .json(&json!({"requestId":"workflow-session","workspaceId":"workspace","title":"Workflow root"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let job: Value = http.post(format!("{control}api/v1/sessions/{}/turns",session["id"].as_str().unwrap())).bearer_auth(alice)
        .json(&json!({"requestId":"workflow-root","expectedInputVersion":"0","text":"WORKFLOW-PROBE alice"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut approved = std::collections::BTreeSet::new();
    let mut after_started = false;
    let mut workflow_id = None;
    loop {
        let current: Value = http
            .get(format!(
                "{control}api/v1/jobs/{}",
                job["id"].as_str().unwrap()
            ))
            .bearer_auth(alice)
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
            "workflow root failed: {current}"
        );
        let mut approvals = Vec::new();
        for wait in current["waits"].as_array().unwrap() {
            match wait["target"]["kind"].as_str().unwrap() {
                "approval" => {
                    approvals.push(wait["target"]["approval_id"].as_str().unwrap().to_owned())
                }
                "child" => {
                    let group = wait["target"]["job_id"].as_str().unwrap();
                    let url = format!("{control}api/v1/jobs/{group}/workflow");
                    if workflow_id.is_none() {
                        assert_eq!(
                            http.get(&url)
                                .bearer_auth(bob)
                                .send()
                                .await
                                .unwrap()
                                .status(),
                            reqwest::StatusCode::NOT_FOUND
                        );
                        workflow_id = Some(group.to_owned());
                    }
                    let view: Value = http
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
                    let nodes = view["nodes"].as_array().unwrap();
                    assert_eq!(nodes.len(), 3);
                    assert_eq!(nodes[2]["dependsOn"], json!(["fast"]));
                    for node in nodes {
                        if node["nodeId"] == "after-fast" && node["state"] != "queued" {
                            assert_eq!(nodes[0]["state"], "succeeded");
                            after_started = true;
                        }
                        if node["nodeId"] == "slow" && !after_started {
                            continue;
                        }
                        for wait in node["waits"].as_array().unwrap() {
                            if wait["target"]["kind"] == "approval" {
                                approvals.push(
                                    wait["target"]["approval_id"].as_str().unwrap().to_owned(),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for approval in approvals {
            if approved.insert(approval.clone()) {
                http.post(format!("{control}api/v1/approvals/{approval}/answer"))
                    .bearer_auth(alice)
                    .json(&json!({"requestId":format!("answer-{approval}"),"answer":"approve"}))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "workflow did not complete; root={current}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        after_started,
        "one completed branch must refill while the slow node waits for approval"
    );
    assert_eq!(approved.len(), 4);
    let group = workflow_id.unwrap();
    let view: Value = http
        .get(format!("{control}api/v1/jobs/{group}/workflow"))
        .bearer_auth(alice)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["state"], "completed");
    assert!(
        view["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node["state"] == "succeeded")
    );
}
