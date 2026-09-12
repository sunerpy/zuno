//! Real Worker/control/gateway Council execution through the public API.
use super::*;
use zuno_orchestration::{
    CouncilPresetDescriptor, CouncilRetryPolicyDescriptor, CouncilSeatDescriptor,
    CouncilSynthesisPolicyDescriptor,
};

pub fn configuration(completion: &Definition) -> zuno_enterprise::config::CouncilDefinition {
    zuno_enterprise::config::CouncilDefinition {
        preset: CouncilPresetDescriptor {
            name: "native-council".to_owned(),
            source_id: "fixture:native-council".to_owned(),
            seats: ["one", "two"]
                .into_iter()
                .map(|id| CouncilSeatDescriptor {
                    id: id.to_owned(),
                    agent: "workspace-helper".to_owned(),
                    instruction: format!("Check the isolated seat {id} workspace"),
                })
                .collect(),
            quorum: 2,
            max_parallel: 2,
            deadline_ms: 60000,
            seat_output_bytes: 8192,
            retry_policy: CouncilRetryPolicyDescriptor { max_retries: 1 },
            synthesis_policy: CouncilSynthesisPolicyDescriptor {
                timeout_ms: 10000,
                max_input_bytes: 65536,
            },
        },
        synthesis: completion.reference(),
        repairs: [("workspace-helper".to_owned(), completion.reference())].into(),
    }
}
fn answer(verdict: &str) -> String {
    json!({"verdict":verdict,"confidence":0.8,"evidence":["confirmed one isolated operation"],"risks":["limited source coverage"],"recommendation":"Preserve dissent"}).to_string()
}
pub fn model(body: &Value) -> Response {
    let messages = body["messages"].as_array().unwrap();
    let text = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .to_string();
    let has = |id: &str| {
        messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == id)
    };
    let no_tools = || {
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        )
    };
    if text.contains("Format the previous completed Council response") {
        no_tools();
        assert!(
            text.contains("COUNCIL-INVALID: seat two confirmed its isolated operation; dissent")
        );
        return model_response(
            json!({"role":"assistant","content":answer("dissent")}),
            true,
        );
    }
    if text.contains("Synthesize the recorded Council result data") {
        no_tools();
        assert!(text.contains("agree") && text.contains("dissent"));
        assert!(text.contains("\\\"attempts\\\":2"));
        return model_response(
            json!({"role":"assistant","content":"COUNCIL-SYNTHESIS: preserve dissent and scope limitations"}),
            true,
        );
    }
    if text.contains("Council question:") {
        if has("council-command") {
            assert!(body.to_string().contains("council-operation-once"));
            let output = if text.contains("Seat `one`") {
                answer("agree")
            } else {
                "COUNCIL-INVALID: seat two confirmed its isolated operation; dissent".to_owned()
            };
            return model_response(json!({"role":"assistant","content":output}), true);
        }
        return model_response(
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":"council-command","type":"function","function":{
                    "name":"environment_command","arguments":json!({"argv":["sh","-c",
                        "test ! -e /workspace/council-once && printf 'council-operation-once\\n' > /workspace/council-once && cat /workspace/council-once"
                    ]}).to_string()
                }
            }]}),
            false,
        );
    }
    if !has("council-run") {
        return model_response(
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":"council-run","type":"function","function":{
                    "name":"council_run","arguments":json!({"preset":"native-council","question":"COUNCIL-PROBE alice","description":"Council root"}).to_string()
                }
            }]}),
            false,
        );
    }
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "council-run")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(result.contains("COUNCIL-SYNTHESIS"), "{result}");
    assert!(result.contains("\"status\":\"completed\""));
    model_response(
        json!({"role":"assistant","content":"COUNCIL-COMPLETE"}),
        true,
    )
}

pub async fn verify(http: &reqwest::Client, control: &str, alice: &str, bob: &str) {
    let session:Value=http.post(format!("{control}api/v1/sessions")).bearer_auth(alice)
        .json(&json!({"requestId":"council-session","workspaceId":"workspace","title":"Council root"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let job:Value=http.post(format!("{control}api/v1/sessions/{}/turns",session["id"].as_str().unwrap())).bearer_auth(alice)
        .json(&json!({"requestId":"council-root","expectedInputVersion":"0","text":"COUNCIL-PROBE alice"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut approved = std::collections::BTreeSet::new();
    let mut group = None;
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
            "Council root failed: {current}"
        );
        for wait in current["waits"].as_array().unwrap() {
            if wait["target"]["kind"] != "child" {
                continue;
            }
            let id = wait["target"]["job_id"].as_str().unwrap();
            let url = format!("{control}api/v1/jobs/{id}/workflow");
            if group.is_none() {
                assert_eq!(
                    http.get(&url)
                        .bearer_auth(bob)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    reqwest::StatusCode::NOT_FOUND
                );
                group = Some(id.to_owned());
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
            assert_eq!(view["nodes"].as_array().unwrap().len(), 3);
            assert_eq!(view["council"]["preset"], "native-council");
            for node in view["nodes"].as_array().unwrap() {
                for wait in node["waits"].as_array().unwrap() {
                    if wait["target"]["kind"] != "approval" {
                        continue;
                    }
                    let approval = wait["target"]["approval_id"].as_str().unwrap();
                    if approved.insert(approval.to_owned()) {
                        http.post(format!("{control}api/v1/approvals/{approval}/answer")).bearer_auth(alice)
                            .json(&json!({"requestId":format!("answer-{approval}"),"answer":"approve"}))
                            .send().await.unwrap().error_for_status().unwrap();
                    }
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Council did not complete: {current}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        approved.len(),
        2,
        "format correction and synthesis must have no external operations"
    );
    let view: Value = http
        .get(format!("{control}api/v1/jobs/{}/workflow", group.unwrap()))
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
    assert_eq!(view["council"]["phase"], "completed");
    assert_eq!(view["council"]["seats"][0]["attempts"], 1);
    assert_eq!(view["council"]["seats"][1]["attempts"], 2);
}
