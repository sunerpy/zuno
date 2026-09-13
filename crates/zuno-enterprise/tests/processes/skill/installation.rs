use super::*;

pub(super) fn model(body: &Value) -> Option<Response> {
    let messages = body["messages"].as_array()?;
    let user = messages.iter().find(|m| m["role"] == "user")?["content"].as_str()?;
    if !user.contains("SKILL-LIBRARY") {
        return None;
    }
    let prompt = messages
        .iter()
        .filter(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let result = |id: &str| {
        messages
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
    };
    let revoked = result("skill-wait").is_some();
    let enabled = user.contains("enabled") && !revoked;
    assert_eq!(
        prompt.contains("recorded-proof"),
        enabled,
        "Skill index: {prompt}"
    );
    assert!(
        !prompt.contains("CANDIDATE-SKILL"),
        "index must not inline the Skill body"
    );
    if !enabled && !revoked {
        return Some(model_response(
            json!({"role":"assistant","content":"SKILL-LIBRARY-INACTIVE"}),
            true,
        ));
    }
    let tool = |id: &str, name: &str, args: Value| {
        Some(model_response(
            json!({
        "role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function",
        "function":{"name":name,"arguments":args.to_string()}}]}),
            false,
        ))
    };
    if revoked {
        if let Some(message) = result("skill-after-revoke") {
            assert!(
                !message["content"]
                    .as_str()
                    .unwrap()
                    .contains("CANDIDATE-SKILL")
            );
            return Some(model_response(
                json!({"role":"assistant","content":"SKILL-LIBRARY-REVOKED"}),
                true,
            ));
        }
        return tool(
            "skill-after-revoke",
            "skill",
            json!({"action":"load","name":"recorded-proof"}),
        );
    }
    if result("skill-search").is_none() {
        return tool(
            "skill-search",
            "skill",
            json!({"action":"search","query":"recorded proof"}),
        );
    }
    assert!(
        result("skill-search").unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("recorded-proof")
    );
    if result("skill-load").is_none() {
        return tool(
            "skill-load",
            "skill",
            json!({"action":"load","name":"recorded-proof"}),
        );
    }
    assert!(
        result("skill-load").unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("CANDIDATE-SKILL")
    );
    tool(
        "skill-wait",
        "environment_command",
        json!({"argv":["/bin/echo","skill-activation-checkpoint"]}),
    )
}

pub(super) async fn verify(
    http: &reqwest::Client,
    control: &str,
    alice: &str,
    bob: &str,
    candidate: &Value,
) {
    let request = json!({"requestId":"skill-install","expectedDigest":candidate["digest"],
        "expectedRevision":"0","description":"Read the recorded proof before answering."});
    let install = format!(
        "{control}api/v1/skills/{}/install",
        candidate["id"].as_str().unwrap()
    );
    assert_eq!(
        http.post(&install)
            .bearer_auth(bob)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let installed: Value = http
        .post(&install)
        .bearer_auth(alice)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(installed["active"], false);
    let duplicate: Value = http
        .post(&install)
        .bearer_auth(alice)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(installed, duplicate);
    let id = installed["id"].as_str().unwrap();
    let url = format!("{control}api/v1/installed-skills/{id}");
    assert_eq!(
        http.get(&url)
            .bearer_auth(bob)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let page: Value = http
        .get(format!("{control}api/v1/workspaces/workspace/skills"))
        .bearer_auth(bob)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["items"], json!([]));
    let inactive = shared_memory::start(
        http,
        control,
        alice,
        "SKILL-LIBRARY inactive alice",
        "skill-inactive",
    )
    .await;
    shared_memory::complete(http, &inactive, alice).await;
    let active:Value=http.post(format!("{url}/activation")).bearer_auth(alice)
        .json(&json!({"requestId":"skill-activate","expectedRevision":installed["revision"],"active":true}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(active["active"], true);
    assert_ne!(active["source"], installed["source"]);
    let job = shared_memory::start(
        http,
        control,
        alice,
        "SKILL-LIBRARY enabled alice",
        "skill-active",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    let approval = loop {
        let current: Value = http
            .get(&job)
            .bearer_auth(alice)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(id) = current["waits"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|wait| wait["target"]["approval_id"].as_str().map(str::to_owned))
        {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Skill tool did not reach checkpoint: {current}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    let disabled:Value=http.post(format!("{url}/activation")).bearer_auth(alice)
        .json(&json!({"requestId":"skill-deactivate","expectedRevision":active["revision"],"active":false}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(disabled["active"], false);
    http.post(format!("{control}api/v1/approvals/{approval}/answer"))
        .bearer_auth(alice)
        .json(&json!({"requestId":"resume-skill-checkpoint","answer":"approve"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    shared_memory::complete(http, &job, alice).await;
    let rolled: Value = http
        .post(format!("{url}/rollback"))
        .bearer_auth(alice)
        .json(
            &json!({"requestId":"skill-rollback","expectedRevision":disabled["revision"],
            "targetRevision":active["revision"]}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        rolled["active"], false,
        "rollback restores content, not an old activation decision"
    );
    assert_eq!(rolled["contentDigest"], installed["contentDigest"]);
}
