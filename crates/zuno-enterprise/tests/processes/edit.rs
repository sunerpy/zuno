use super::*;
pub fn model(body: &Value) -> Response {
    let messages = body["messages"].as_array().unwrap();
    let tool = |id: &str| {
        messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
    };
    let (id, name, args) = if tool("edit-create").is_none() {
        (
            "edit-create",
            "workspace_edit",
            json!({"edits":[{"path":"edit-proof.txt","expected":{"kind":"absent"},"content":"before\n"}]}),
        )
    } else if tool("edit-read").is_none() {
        assert!(
            tool("edit-create").unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("committed")
        );
        (
            "edit-read",
            "workspace_read",
            json!({"path":"edit-proof.txt"}),
        )
    } else if tool("edit-replace").is_none() {
        let text = tool("edit-read").unwrap()["content"].as_str().unwrap();
        let header: Value = serde_json::from_str(text.split("\n\n").next().unwrap()).unwrap();
        assert!(text.ends_with("before\n"));
        (
            "edit-replace",
            "workspace_edit",
            json!({"edits":[{"path":"edit-proof.txt",
            "expected":{"kind":"file","sha256":header["file"]["entry"]["sha256"]},"content":"after\n"}]}),
        )
    } else if tool("edit-verify").is_none() {
        assert!(
            tool("edit-replace").unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("committed")
        );
        (
            "edit-verify",
            "workspace_read",
            json!({"path":"edit-proof.txt"}),
        )
    } else {
        assert!(
            tool("edit-verify").unwrap()["content"]
                .as_str()
                .unwrap()
                .ends_with("after\n")
        );
        return model_response(json!({"role":"assistant","content":"EDIT-VERIFIED"}), true);
    };
    model_response(
        json!({"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function",
        "function":{"name":name,"arguments":args.to_string()}}]}),
        false,
    )
}
pub async fn verify(http: &reqwest::Client, control: &str, alice: &str, bob: &str) {
    let session: Value = http
        .post(format!("{control}api/v1/sessions"))
        .bearer_auth(alice)
        .json(&json!({"requestId":"edit-session","workspaceId":"workspace","title":"Edit proof"}))
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
        .bearer_auth(alice)
        .json(
            &json!({"requestId":"edit-turn","expectedInputVersion":"0","text":"EDIT-PROBE alice"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let job = job["id"].as_str().unwrap();
    let mut approved = std::collections::BTreeSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        let current: Value = http
            .get(format!("{control}api/v1/jobs/{job}"))
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
            "edit failed: {current}"
        );
        for wait in current["waits"].as_array().unwrap() {
            if wait["target"]["kind"] != "approval" {
                continue;
            }
            let id = wait["target"]["approval_id"].as_str().unwrap();
            if !approved.insert(id.to_owned()) {
                continue;
            }
            let url = format!("{control}api/v1/approvals/{id}/edit");
            assert_eq!(
                http.get(&url)
                    .bearer_auth(bob)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NOT_FOUND
            );
            let review: Value = http
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
            assert_eq!(review["review"].as_array().unwrap().len(), 1);
            assert_eq!(review["review"][0]["path"], "edit-proof.txt");
            assert!(!review.to_string().contains("lease"));
            if approved.len() == 1 {
                assert!(review["review"][0]["before"].is_null());
                assert_eq!(review["review"][0]["after"], "before\n");
            } else {
                assert_eq!(review["review"][0]["before"], "before\n");
                assert_eq!(review["review"][0]["after"], "after\n");
            }
            http.post(format!("{control}api/v1/approvals/{id}/answer"))
                .bearer_auth(alice)
                .json(&json!({"requestId":format!("approve-{id}"),"answer":"approve"}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "edit did not complete: {current}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        approved.len(),
        2,
        "both file writes require explicit human approval"
    );
}
