//! Real HITL and durable waiting around a gateway-owned workspace merge.
use super::*;
pub fn model(body: &Value) -> Response {
    let messages = body["messages"].as_array().unwrap();
    let has = |id: &str| {
        messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == id)
    };
    let child = messages
        .iter()
        .filter(|message| message["role"] == "system" || message["role"] == "developer")
        .any(|message| message.to_string().contains("CHILD-EXECUTOR"));
    let call = |id: &str, name: &str, args: Value| {
        model_response(
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}
            }]}),
            false,
        )
    };
    if child {
        if has("merge-child-command") {
            return model_response(
                json!({"role":"assistant","content":"MERGE-CHILD-COMPLETE"}),
                true,
            );
        }
        return call(
            "merge-child-command",
            "environment_command",
            json!({"argv":["sh","-c","set -eu; test \"$(cat /workspace/two)\" = base; printf child > /workspace/two; printf '\\000\\377' > /workspace/new.bin"]}),
        );
    }
    if !has("merge-seed") {
        return call(
            "merge-seed",
            "environment_command",
            json!({"argv":["sh","-c","printf base > /workspace/one; printf base > /workspace/two"]}),
        );
    }
    if !has("merge-task") {
        return call(
            "merge-task",
            "task",
            json!({"agent":"workspace-helper","objective":"MERGE-PROBE alice",
            "deliverable":"A completed child workspace change","instructions":"MERGE-PROBE alice: change only file two.",
            "success_evidence":"The inherited base exists and the child change is complete."}),
        );
    }
    if !has("merge-parent-edit") {
        return call(
            "merge-parent-edit",
            "environment_command",
            json!({"argv":["sh","-c","printf parent > /workspace/one"]}),
        );
    }
    if !has("merge-apply") {
        let child = messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "merge-task")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(child.contains("MERGE-CHILD-COMPLETE"));
        let id = child
            .split_once(" job=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap();
        return call("merge-apply", "workspace_merge", json!({"childJobId":id}));
    }
    if !has("merge-verify") {
        let output = messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "merge-apply")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(output.contains("\"state\":\"committed\""), "{output}");
        return call(
            "merge-verify",
            "environment_command",
            json!({"argv":["sh","-c","set -eu; test \"$(cat /workspace/one)\" = parent; test \"$(cat /workspace/two)\" = child; printf merge-verified"]}),
        );
    }
    assert!(body.to_string().contains("merge-verified"));
    model_response(json!({"role":"assistant","content":"MERGE-COMPLETE"}), true)
}

pub async fn verify(http: &reqwest::Client, control: &str, alice: &str, bob: &str) {
    let session: Value = http
        .post(format!("{control}api/v1/sessions"))
        .bearer_auth(alice)
        .json(&json!({"requestId":"merge-session","workspaceId":"workspace","title":"Merge root"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let root:Value=http.post(format!("{control}api/v1/sessions/{}/turns",session["id"].as_str().unwrap())).bearer_auth(alice)
        .json(&json!({"requestId":"merge-root","expectedInputVersion":"0","text":"MERGE-PROBE alice"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let mut approved = std::collections::BTreeSet::new();
    let mut merge_approval = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let current: Value = http
            .get(format!(
                "{control}api/v1/jobs/{}",
                root["id"].as_str().unwrap()
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
            "merge root failed: {current}"
        );
        let mut jobs = vec![current.clone()];
        for wait in current["waits"].as_array().unwrap() {
            if wait["target"]["kind"] == "child" {
                let child: Value = http
                    .get(format!(
                        "{control}api/v1/jobs/{}",
                        wait["target"]["job_id"].as_str().unwrap()
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
                jobs.push(child);
            }
        }
        for job in jobs {
            for wait in job["waits"].as_array().unwrap() {
                if wait["target"]["kind"] != "approval" {
                    continue;
                }
                let id = wait["target"]["approval_id"].as_str().unwrap();
                if !approved.insert(id.to_owned()) {
                    continue;
                }
                let approval: Value = http
                    .get(format!("{control}api/v1/approvals/{id}"))
                    .bearer_auth(alice)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if approval["presentation"]["kind"] == "workspace_merge" {
                    merge_approval = true;
                    assert_eq!(approval["state"], "pending");
                    assert_eq!(approval["presentation"]["changeCount"], 2);
                    let review_url = format!("{control}api/v1/approvals/{id}/merge");
                    let review: Value = http
                        .get(&review_url)
                        .bearer_auth(alice)
                        .send()
                        .await
                        .unwrap()
                        .error_for_status()
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    assert_eq!(
                        review["plan"]["changes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|change| change["path"].as_str().unwrap())
                            .collect::<Vec<_>>(),
                        ["new.bin", "two"]
                    );
                    assert_eq!(
                        review["admitted"], false,
                        "content is reviewable before execution admission"
                    );
                    let binary = http
                        .get(format!("{review_url}/content"))
                        .query(&[("side", "child"), ("path", "new.bin")])
                        .bearer_auth(alice)
                        .send()
                        .await
                        .unwrap()
                        .error_for_status()
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap();
                    assert_eq!(binary.as_ref(), &[0, 255]);
                    for (side, expected) in [("parent", "base"), ("child", "child")] {
                        let response = http
                            .get(format!("{review_url}/content"))
                            .query(&[("side", side), ("path", "two")])
                            .bearer_auth(alice)
                            .send()
                            .await
                            .unwrap()
                            .error_for_status()
                            .unwrap();
                        assert_eq!(
                            response.headers()["content-type"],
                            "application/octet-stream"
                        );
                        assert_eq!(response.headers()["cache-control"], "no-store");
                        assert_eq!(response.text().await.unwrap(), expected);
                    }
                    assert_eq!(
                        http.get(format!("{review_url}/content"))
                            .query(&[("side", "child"), ("path", "one")])
                            .bearer_auth(alice)
                            .send()
                            .await
                            .unwrap()
                            .status(),
                        reqwest::StatusCode::NOT_FOUND,
                        "unchanged files are outside this read capability"
                    );
                    assert_eq!(
                        http.get(format!("{review_url}/content"))
                            .query(&[("side", "child"), ("path", "two")])
                            .bearer_auth(bob)
                            .send()
                            .await
                            .unwrap()
                            .status(),
                        reqwest::StatusCode::NOT_FOUND
                    );
                    assert_eq!(
                        http.get(format!("{control}api/v1/approvals/{id}"))
                            .bearer_auth(bob)
                            .send()
                            .await
                            .unwrap()
                            .status(),
                        reqwest::StatusCode::NOT_FOUND
                    );
                }
                http.post(format!("{control}api/v1/approvals/{id}/answer"))
                    .bearer_auth(alice)
                    .json(&json!({"requestId":format!("answer-{id}"),"answer":"approve"}))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "merge did not complete: {current}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(merge_approval);
    assert_eq!(approved.len(), 5);
}
