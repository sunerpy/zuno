use super::*;
use sha2::{Digest, Sha256};

pub fn model(body: &Value) -> Response {
    let messages = body["messages"].as_array().unwrap();
    let completed = messages
        .iter()
        .any(|message| message["role"] == "tool" && message["tool_call_id"] == "import-check");
    if completed {
        assert!(body.to_string().contains("import-verified"));
        return model_response(
            json!({"role":"assistant","content":"IMPORT-COMPLETE"}),
            true,
        );
    }
    model_response(
        json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"import-check","type":"function","function":{"name":"environment_command",
                "arguments":json!({"argv":["sh","-c","set -eu; test \"$(cat /workspace/project.txt)\" = imported-project; test \"$(stat -c %u /workspace/project.txt)\" = 0; test \"$(stat -c %a /workspace/project.txt)\" = 600; test \"$(stat -c %a /workspace)\" = 700; printf import-verified"]}).to_string()}
        }]}),
        false,
    )
}
fn archive() -> Vec<u8> {
    let mut writer = tar::Builder::new(Vec::new());
    let mut root = tar::Header::new_gnu();
    root.set_entry_type(tar::EntryType::Directory);
    root.set_mode(0o700);
    root.set_uid(1000);
    root.set_gid(1000);
    root.set_size(0);
    writer
        .append_data(&mut root, "workspace", std::io::empty())
        .unwrap();
    let bytes = b"imported-project";
    let mut file = tar::Header::new_gnu();
    file.set_mode(0o600);
    file.set_uid(1000);
    file.set_gid(1000);
    file.set_size(bytes.len() as u64);
    writer
        .append_data(&mut file, "workspace/project.txt", &bytes[..])
        .unwrap();
    writer.finish().unwrap();
    writer.into_inner().unwrap()
}
pub async fn verify(http: &reqwest::Client, control: &str, alice: &str, bob: &str) {
    let session:Value=http.post(format!("{control}api/v1/sessions")).bearer_auth(alice)
        .json(&json!({"requestId":"import-session","workspaceId":"workspace","title":"Imported project"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let session = session["id"].as_str().unwrap();
    let bytes = archive();
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let base = format!("{control}api/v1/sessions/{session}/workspace/imports");
    let request = json!({"requestId":"project-upload","expectedInputVersion":"0","sha256":hash,"bytes":bytes.len().to_string()});
    let prepared: Value = http
        .post(&base)
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
    assert_eq!(prepared["state"], "uploading");
    let url = format!("{base}/{}", prepared["id"].as_str().unwrap());
    assert_eq!(
        http.get(&url)
            .bearer_auth(bob)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(http.post(format!("{control}api/v1/sessions/{session}/turns")).bearer_auth(alice)
        .json(&json!({"requestId":"too-early","expectedInputVersion":"0","text":"IMPORT-PROBE alice"}))
        .send().await.unwrap().status(),reqwest::StatusCode::CONFLICT);
    let archive_url = format!("{url}/archive");
    let upload_one = http
        .put(&archive_url)
        .bearer_auth(alice)
        .header("content-type", "application/x-tar")
        .body(bytes.clone());
    let upload_two = http
        .put(&archive_url)
        .bearer_auth(alice)
        .header("content-type", "application/x-tar")
        .body(bytes.clone());
    let (one, two) = tokio::join!(upload_one.send(), upload_two.send());
    let completed: Value = one
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let concurrent: Value = two
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        concurrent, completed,
        "concurrent identical uploads settle the same import"
    );
    assert_eq!(completed["state"], "ready");
    let changed = vec![0u8; bytes.len()];
    let repeated: Value = http
        .put(&archive_url)
        .bearer_auth(alice)
        .header("content-type", "application/x-tar")
        .body(bytes)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(repeated, completed);
    assert_eq!(
        http.put(&archive_url)
            .bearer_auth(alice)
            .header("content-type", "application/x-tar")
            .body(changed)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT,
        "a completed upload cannot claim different bytes"
    );
    let job:Value=http.post(format!("{control}api/v1/sessions/{session}/turns")).bearer_auth(alice)
        .json(&json!({"requestId":"first-import-turn","expectedInputVersion":"0","text":"IMPORT-PROBE alice"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut answered = false;
    loop {
        let state: Value = http
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
        if state["phase"] == "completed" {
            break;
        }
        assert!(
            matches!(
                state["phase"].as_str(),
                Some("ready" | "running" | "waiting")
            ),
            "imported session failed: {state}"
        );
        for wait in state["waits"].as_array().unwrap() {
            if wait["target"]["kind"] == "approval" && !answered {
                answered = true;
                http.post(format!(
                    "{control}api/v1/approvals/{}/answer",
                    wait["target"]["approval_id"].as_str().unwrap()
                ))
                .bearer_auth(alice)
                .json(&json!({"requestId":"approve-import-check","answer":"approve"}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            }
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(answered, "importing files cannot approve later commands");
}
