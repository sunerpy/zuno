//! Exercise actual file dispatch through stdio, not just constructed tool events.
use super::*;

struct FileTitleResponder {
    target: String,
    main_requests: Arc<AtomicUsize>,
}

impl Respond for FileTitleResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if !body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
        {
            return compatible_text_response("File title fixture");
        }
        if self.main_requests.fetch_add(1, Ordering::SeqCst) != 0 {
            return compatible_text_response("The synthetic file is complete.");
        }
        let arguments = json!({"filePath": self.target, "content": "hello\n"}).to_string();
        let delta = json!({"choices": [{"index": 0, "delta": {
            "role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_write_filename", "type": "function",
                "function": {"name": "write", "arguments": arguments}
            }]
        }, "finish_reason": null}]});
        let end = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]});
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_file_titles_name_real_writes_during_dispatch_and_completion() {
    let provider = MockServer::start().await;
    let root = Arc::new(tempfile::tempdir().unwrap());
    let target = root.path().join("visible 中文.rs");
    Mock::given(method("POST"))
        .respond_with(FileTitleResponder {
            target: target.to_str().unwrap().to_owned(),
            main_requests: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&provider)
        .await;
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).unwrap();
    config["default_agent"] = json!("build");
    config["permission"] = json!({"mode":"allow_all"});
    let mut client = PromptClient::connect(&config.to_string(), root.clone(), None);
    client.prompt(10, "Run the file-title fixture.");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut updates = Vec::new();
    loop {
        let frame = client
            .frames
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("file-title prompt finishes");
        if frame.get("id") == Some(&json!(10)) {
            assert_eq!(frame["result"]["stopReason"], "end_turn", "{frame}");
            break;
        }
        let update = &frame["params"]["update"];
        if update["toolCallId"] == "call_write_filename" {
            updates.push(update.clone());
        }
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");
    assert!(
        updates.iter().any(|update| {
            update["status"] == "in_progress" && update["title"] == "Editing visible 中文.rs"
        }),
        "{updates:#?}"
    );
    let completed = updates
        .iter()
        .find(|update| update["status"] == "completed")
        .unwrap();
    assert_eq!(completed["title"], "Editing visible 中文.rs");
    let actual_path = zuno_paths::wire_path(&std::fs::canonicalize(target).unwrap());
    assert!(
        completed["locations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|location| location["path"] == actual_path),
        "{completed}"
    );
    assert!(
        completed["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|content| content["type"] == "diff" && content["path"] == actual_path),
        "{completed}"
    );
}
