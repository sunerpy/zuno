use super::*;
pub(super) async fn verify(
    http: &reqwest::Client,
    control: &str,
    alice: &str,
    bob: &str,
    identities: &BTreeMap<&str, Value>,
) {
    let space = format!("{control}api/v1/memory/spaces/evidence-runbooks");
    http.put(&space).bearer_auth(alice).json(&json!({"requestId":"evidence-space","expectedRevision":"0","workspaceId":"workspace",
        "title":"Shared evidence","enabled":true,"characterLimit":3000,"members":[
        {"principalId":identities["alice"]["principalId"],"role":"contributor"},{"principalId":identities["bob"]["principalId"],"role":"reviewer"}]}))
        .send().await.unwrap().error_for_status().unwrap();
    let source = start(
        http,
        control,
        alice,
        "SHARED-EXPECTED alice source verification",
        "shared-evidence-source",
    )
    .await;
    complete(http, &source, alice).await;
    let job: Value = http
        .get(&source)
        .bearer_auth(alice)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let evidence:Value=http.post(format!("{control}api/v1/workspaces/workspace/memory")).bearer_auth(alice)
        .json(&json!({"requestId":"record-shared-proof","command":{"kind":"record_evidence",
            "origin":{"kind":"user_input","sessionId":job["sessionId"],"inputId":job["inputId"]},"excerpt":"source verification"}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let reference = &evidence["result"]["Ok"]["reference"];
    let request = json!({"requestId":"share-proof","evidenceId":reference["experience_id"],"expectedDigest":reference["digest"]});
    assert_eq!(
        http.post(format!("{space}/evidence"))
            .bearer_auth(bob)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let grant: Value = http
        .post(format!("{space}/evidence"))
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
    assert_eq!(grant["current"], true);
    let candidate:Value=http.post(format!("{space}/changes")).bearer_auth(alice)
        .json(&json!({"requestId":"evidence-proposal","expectedRevision":"1","reason":"Use explicitly shared evidence",
            "edits":[{"kind":"add","content":"SHARED-RUNBOOK-APPROVED"}],
            "evidence":[{"content":"SHARED-RUNBOOK-APPROVED","grants":[grant["id"]]}]}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    http.post(format!("{space}/review")).bearer_auth(bob)
        .json(&json!({"requestId":"approve-shared-proof","changeId":candidate["id"],"expectedState":candidate["stateDigest"],"decision":"apply"}))
        .send().await.unwrap().error_for_status().unwrap();
    let waiting = start(
        http,
        control,
        bob,
        "SHARED-CHECKPOINT bob evidence",
        "evidence-checkpoint",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let approval = loop {
        let job: Value = http
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
        if let Some(id) = job["waits"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|w| w["target"]["approval_id"].as_str().map(str::to_owned))
        {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no evidence approval wait: {job}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    let revoked: Value = http
        .post(format!(
            "{space}/evidence/{}/revoke",
            grant["id"].as_str().unwrap()
        ))
        .bearer_auth(alice)
        .json(&json!({"requestId":"revoke-proof","expectedRevision":grant["revision"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(revoked["current"], false);
    let after: Value = http
        .get(&space)
        .bearer_auth(bob)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["suppressed"], json!(["SHARED-RUNBOOK-APPROVED"]));
    http.post(format!("{control}api/v1/approvals/{approval}/answer"))
        .bearer_auth(bob)
        .json(&json!({"requestId":"resume-evidence-checkpoint","answer":"approve"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    complete(http, &waiting, bob).await;
}
