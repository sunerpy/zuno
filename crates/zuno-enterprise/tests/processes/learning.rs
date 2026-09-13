use super::*;

const NOTE: &str = "Use cargo test for enterprise memory checks.";
pub fn model(body: &Value) -> Option<Response> {
    let messages = body["messages"].as_array()?;
    let system = messages
        .iter()
        .filter(|m| m["role"] == "system" || m["role"] == "developer")
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let user = messages.iter().find(|m| m["role"] == "user")?["content"].as_str()?;
    if system.contains("isolated experience extractor") {
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        let input: Value = serde_json::from_str(user).unwrap();
        let source = input["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| {
                source["content"]
                    .as_str()
                    .unwrap()
                    .contains("AUTOMATIC-MEMORY")
            })
            .unwrap();
        assert_eq!(source["kind"], "user");
        let response = json!({
            "experiences":[{"kind":"user_correction","title":"alice validation preference",
                "summary":"alice requests cargo test for enterprise memory checks","resolution":null,"confidence":1.0,
                "evidence":[{"kind":"user","source_id":source["source_id"],"excerpt":NOTE}]}],
            "memories":[{"experience_ordinal":0,"scope":"project","action":"add","content":NOTE,
                "old_text":null,"reason":"Explicit user preference","confidence":1.0}]
        });
        return Some(model_response(
            json!({"role":"assistant","content":response.to_string()}),
            true,
        ));
    }
    if system.contains("Maintain a small useful memory") {
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        let input: Value = serde_json::from_str(user).unwrap();
        let source = input["experiences"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| {
                source["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("enterprise memory")
            })
            .unwrap();
        assert_eq!(source["user_authored"], true);
        let response = json!({"updates":[{"scope":"project","action":"add","content":NOTE,"old_text":null,
            "reason":"Explicit user preference with supplied evidence","confidence":1.0,"evidence_ids":[source["id"]]}]});
        return Some(model_response(
            json!({"role":"assistant","content":response.to_string()}),
            true,
        ));
    }
    if user.contains("AUTOMATIC-MEMORY") {
        return Some(model_response(
            json!({"role":"assistant","content":"Memory source turn complete."}),
            true,
        ));
    }
    None
}

async fn memory(
    http: &reqwest::Client,
    control: &str,
    token: &str,
    id: &str,
    command: Value,
) -> Value {
    let result: Value = http
        .post(format!("{control}api/v1/workspaces/workspace/memory"))
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
    assert!(result["result"]["Err"].is_null(), "{result}");
    result["result"]["Ok"].clone()
}
async fn turn(http: &reqwest::Client, control: &str, token: &str, suffix: &str) -> String {
    let session:Value=http.post(format!("{control}api/v1/sessions")).bearer_auth(token)
        .json(&json!({"requestId":format!("automatic-memory-{suffix}"),"workspaceId":"workspace","title":"Automatic memory"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let job: Value = http
        .post(format!(
            "{control}api/v1/sessions/{}/turns",
            session["id"].as_str().unwrap()
        ))
        .bearer_auth(token)
        .json(
            &json!({"requestId":format!("automatic-source-{suffix}"),"expectedInputVersion":"0",
            "text":format!("AUTOMATIC-MEMORY alice: {NOTE}")}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let job = job["id"].as_str().unwrap().to_owned();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let value: Value = http
            .get(format!("{control}api/v1/jobs/{job}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if value["phase"] == "completed" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "source turn did not complete: {value}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    job
}

pub async fn verify(
    http: &reqwest::Client,
    control: &str,
    alice: &str,
    bob: &str,
    admin: &sqlx_postgres::PgPool,
    issuer: &Arc<Issuer>,
) {
    let before = issuer.model_requests.load(Ordering::SeqCst);
    let policy = memory(
        http,
        control,
        alice,
        "automation-policy",
        json!({"kind":"policy","sessionId":null}),
    )
    .await;
    assert_eq!(policy["policy"]["automaticPrivate"], false);
    let policy = memory(
        http,
        control,
        alice,
        "automation-generation",
        json!({"kind":"set_policy","sessionId":null,
        "expectedRevision":policy["policy"]["revision"],"useMemories":true,"generatePrivate":true}),
    )
    .await;
    let policy = memory(
        http,
        control,
        alice,
        "automation-enable",
        json!({"kind":"set_automation","sessionId":null,
        "expectedRevision":policy["policy"]["revision"],"enabled":true}),
    )
    .await;
    assert_eq!(policy["policy"]["automaticPrivate"], true);
    let source = turn(http, control, alice, "enabled").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    loop {
        let result = memory(
            http,
            control,
            alice,
            "automation-read",
            json!({"kind":"read"}),
        )
        .await;
        if result.to_string().contains(NOTE) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let jobs: Vec<Value> = query_scalar(
                "SELECT jsonb_build_object('id',id,'status',status,'kind',kind,'result',result)
                FROM zuno_enterprise_preview.learning_job",
            )
            .fetch_all(admin)
            .await
            .unwrap();
            panic!("automatic Memory did not commit: {jobs:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let jobs:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.learning_execution e
        JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id
        WHERE e.source_job_id=$1 AND j.status='completed'").bind(&source).fetch_one(admin).await.unwrap();
    assert_eq!(
        jobs, 2,
        "extraction and maintenance are separately settled jobs"
    );
    assert_eq!(issuer.model_requests.load(Ordering::SeqCst), before + 3);
    let page: Value = http
        .get(format!(
            "{control}api/v1/workspaces/workspace/learning/jobs?limit=1"
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
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let first = &page["items"][0];
    assert_eq!(first["state"], "completed");
    assert_eq!(first["sourceJobId"], source);
    assert!(first["budget"]["charged"].is_string());
    assert!(!page.to_string().contains("lease"));
    assert!(!page.to_string().contains("configuration"));
    let cursor = &page["before"];
    let next: Value = http
        .get(format!(
            "{control}api/v1/workspaces/workspace/learning/jobs"
        ))
        .bearer_auth(alice)
        .query(&[
            ("limit", "1"),
            ("beforeCreatedAtMs", cursor["createdAtMs"].as_str().unwrap()),
            ("beforeJobId", cursor["jobId"].as_str().unwrap()),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(first["id"], next["items"][0]["id"]);
    assert_eq!(
        http.get(format!(
            "{control}api/v1/learning/jobs/{}",
            first["id"].as_str().unwrap()
        ))
        .bearer_auth(bob)
        .send()
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let history: Value = http
        .get(format!(
            "{control}api/v1/sessions/{}/history",
            first["sessionId"].as_str().unwrap()
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
    let learning_items = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            item["record"]["item"]["activityKind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("memory_"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        learning_items.len(),
        2,
        "both completed model jobs remain in durable activity"
    );
    for item in learning_items {
        assert_eq!(item["record"]["item"]["state"], "completed");
        assert_eq!(item["record"]["actions"][0]["kind"], "view_learning");
    }
    assert!(
        !memory(
            http,
            control,
            bob,
            "automation-foreign",
            json!({"kind":"read"})
        )
        .await
        .to_string()
        .contains(NOTE)
    );
    let policy = memory(
        http,
        control,
        alice,
        "automation-disable",
        json!({"kind":"set_automation","sessionId":null,
        "expectedRevision":policy["policy"]["revision"],"enabled":false}),
    )
    .await;
    assert_eq!(policy["policy"]["automaticPrivate"], false);
    let second = turn(http, control, alice, "disabled").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let jobs: i64 = query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.learning_execution WHERE source_job_id=$1",
    )
    .bind(second)
    .fetch_one(admin)
    .await
    .unwrap();
    assert_eq!(jobs, 0);
    assert_eq!(issuer.model_requests.load(Ordering::SeqCst), before + 4);
}
