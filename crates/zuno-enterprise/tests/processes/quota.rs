use super::*;
pub(super) async fn verify(http: &reqwest::Client, control: &str, alice: &str, bob: &str) {
    let url = format!("{control}api/v1/quotas");
    let original: Value = http
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
    let count = |resource: &str| {
        original["usage"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["resource"] == resource)
            .unwrap()["used"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    let mut limits = original["policy"]["limits"].clone();
    limits["rootSessions"] = json!(count("root_sessions"));
    limits["rootJobs"] = json!(1);
    limits["executions"] = json!(1);
    let update = json!({"requestId":"native-quotas","expectedRevision":original["policy"]["revision"],"limits":limits});
    assert_eq!(
        http.put(&url)
            .bearer_auth(bob)
            .json(&update)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let applied: Value = http
        .put(&url)
        .bearer_auth(alice)
        .json(&update)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let rejected=http.post(format!("{control}api/v1/sessions")).bearer_auth(alice)
        .json(&json!({"requestId":"over-session-cap","workspaceId":"workspace","title":"must not exist"}))
        .send().await.unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"],
        "quota_exceeded"
    );
    let mut restored = original["policy"]["limits"].clone();
    restored["rootJobs"] = json!(1);
    restored["executions"] = json!(1);
    let next:Value=http.put(&url).bearer_auth(alice).json(&json!({"requestId":"restore-session-quota","expectedRevision":applied["revision"],"limits":restored}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let active = super::shared_memory::start(
        http,
        control,
        alice,
        "ACP-PROBE alice quota",
        "quota-running",
    )
    .await;
    let job: Value = http
        .get(&active)
        .bearer_auth(alice)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second:Value=http.post(format!("{control}api/v1/sessions")).bearer_auth(alice)
        .json(&json!({"requestId":"quota-second","workspaceId":"workspace","title":"queued quota probe"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let session = second["id"].as_str().unwrap();
    let prompt = json!({"requestId":"blocked-turn","expectedInputVersion":"0","text":"ACP-PROBE alice quota second"});
    assert_eq!(
        http.post(format!("{control}api/v1/sessions/{session}/turns"))
            .bearer_auth(alice)
            .json(&prompt)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    let version: Value = http
        .get(format!("{control}api/v1/sessions/{session}/input-version"))
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
        version["version"], "0",
        "rejected quota admission cannot consume input CAS"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let current: Value = http
            .get(&active)
            .bearer_auth(alice)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if current["phase"] == "waiting" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "Job did not wait");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let observed: Value = http
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
    assert_eq!(
        observed["usage"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["resource"] == "executions")
            .unwrap()["used"],
        "0"
    );
    let cancel = json!({"requestId":"quota-cancel","expectedTurnId":job["turnId"],"reason":"quota fixture complete"});
    http.post(format!("{active}/cancel"))
        .bearer_auth(alice)
        .json(&cancel)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let accepted: Value = http
        .post(format!("{control}api/v1/sessions/{session}/turns"))
        .bearer_auth(alice)
        .json(&prompt)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    http.post(format!("{control}api/v1/jobs/{}/cancel",accepted["id"].as_str().unwrap())).bearer_auth(alice)
        .json(&json!({"requestId":"quota-cancel-second","expectedTurnId":accepted["turnId"],"reason":"quota fixture complete"}))
        .send().await.unwrap().error_for_status().unwrap();
    http.put(&url).bearer_auth(alice).json(&json!({"requestId":"restore-native-quotas","expectedRevision":next["revision"],"limits":original["policy"]["limits"]}))
        .send().await.unwrap().error_for_status().unwrap();
}
