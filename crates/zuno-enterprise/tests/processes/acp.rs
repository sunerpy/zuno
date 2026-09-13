use super::*;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

pub fn model(body: &Value) -> Response {
    if body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "acp-command")
    {
        return model_response(
            json!({"role":"assistant","content":"ACP-BRIDGE-COMPLETE"}),
            true,
        );
    }
    model_response(
        json!({"role":"assistant","tool_calls":[{"index":0,"id":"acp-command","type":"function",
        "function":{"name":"environment_command","arguments":json!({"argv":["/bin/echo","ACP-BRIDGE-PROOF"]}).to_string()}}]}),
        false,
    )
}
struct Editor {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    events: Vec<Value>,
}
#[derive(Clone)]
struct LostAdmission {
    http: reqwest::Client,
    control: String,
    dropped: Arc<std::sync::atomic::AtomicBool>,
    submissions: Arc<AtomicUsize>,
}
async fn proxy(State(state): State<LostAdmission>, request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
    let turn = parts.method == axum::http::Method::POST && parts.uri.path().ends_with("/turns");
    let mut forward = state.http.request(
        parts.method,
        format!(
            "{}{}",
            state.control,
            parts.uri.to_string().trim_start_matches('/')
        ),
    );
    if let Some(value) = parts.headers.get(header::AUTHORIZATION) {
        forward = forward.header(header::AUTHORIZATION, value);
    }
    if !bytes.is_empty() {
        forward = forward
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes);
    }
    let response = forward.send().await.unwrap();
    let status = response.status();
    let body = response.bytes().await.unwrap();
    if turn && status.is_success() {
        state.submissions.fetch_add(1, Ordering::SeqCst);
        if !state.dropped.swap(true, Ordering::SeqCst) {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                "injected lost admission response",
            )
                .into_response();
        }
    }
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}
impl Editor {
    async fn open(control: &str, token: &str, fixture: &Fixture, root: &Path, name: &str) -> Self {
        let token_file = root.join(format!("acp-{name}.token"));
        write(&token_file, token);
        let path = root.join(format!("acp-{name}.json"));
        write(
            &path,
            serde_json::to_vec(&ServiceConfig {
                state_directory: root.join(format!("acp-{name}-state")),
                service: ServiceRole::AcpBridge(AcpBridgeConfig {
                    api: StateClientConfig {
                        endpoint: format!("{control}api/v1/"),
                        access_token_file: token_file,
                        root_certificate: Some(fixture.root_certificate.clone()),
                    },
                    workspace_id: WorkspaceId::new("workspace").unwrap(),
                    local_directory: PathBuf::from("/editor"),
                    max_sessions: 8,
                    poll_millis: 100,
                }),
            })
            .unwrap(),
        );
        let mut child = tokio::process::Command::new(executable::binary())
            .arg("--config")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::File::create(root.join(format!("acp-{name}.log"))).unwrap(),
            ))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut editor = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            events: Vec::new(),
        };
        editor.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true}}})).await;
        let init = editor.response(1).await;
        assert_eq!(init["result"]["agentInfo"]["name"], "zuno-enterprise");
        assert_eq!(
            init["result"]["_meta"]["zuno"]["approvalAuthority"],
            "enterprise_api"
        );
        editor
    }
    async fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.input.write_all(&bytes).await.unwrap();
        self.input.flush().await.unwrap();
    }
    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(40), self.output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "ACP stdout closed before response");
        serde_json::from_str(&line).expect("stdout is only ACP JSON-RPC")
    }
    async fn response(&mut self, id: u64) -> Value {
        loop {
            let value = self.next().await;
            if value["id"] == id {
                return value;
            }
            assert!(
                value.get("id").is_none(),
                "bridge must not request editor filesystem, terminal or permission execution: {value}"
            );
            self.events.push(value);
        }
    }
    async fn new_session(&mut self, id: u64) -> String {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":"/editor","mcpServers":[]}})).await;
        self.response(id).await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn close(mut self) {
        self.input.shutdown().await.unwrap();
        drop(self.input);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
pub async fn verify(
    http: &reqwest::Client,
    control: &str,
    tokens: &BTreeMap<&str, String>,
    fixture: &Fixture,
    root: &Path,
) {
    let proxy_address = address();
    let proxy_url = format!("https://localhost:{}/", proxy_address.port());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let submissions = Arc::new(AtomicUsize::new(0));
    let stopped = InterruptSignal::new();
    let proxy_task = tokio::spawn({
        let options = fixture.tls(proxy_address);
        let stopped = stopped.clone();
        let state = LostAdmission {
            http: http.clone(),
            control: control.to_owned(),
            dropped: dropped.clone(),
            submissions: submissions.clone(),
        };
        async move {
            zuno_enterprise::serve_tls(
                &options,
                Router::new().fallback(proxy).with_state(state),
                stopped,
            )
            .await
            .unwrap()
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if http
            .get(format!("{proxy_url}api/v1/identity"))
            .bearer_auth(&tokens["alice"])
            .send()
            .await
            .is_ok()
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut alice = Editor::open(&proxy_url, &tokens["alice"], fixture, root, "alice").await;
    let mut bob = Editor::open(control, &tokens["bob"], fixture, root, "bob").await;
    let a = alice.new_session(2).await;
    let b = bob.new_session(2).await;
    bob.send(json!({"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":a,"cwd":"/editor","mcpServers":[]}})).await;
    assert!(bob.response(3).await["error"].is_object());
    for (editor, session, name) in [(&mut alice, &a, "alice"), (&mut bob, &b, "bob")] {
        editor.send(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,
            "prompt":[{"type":"text","text":format!("ACP-PROBE {name}")}],"_meta":{"zuno":{"requestId":format!("acp-turn-{name}")}}}})).await;
    }
    for (editor, session, name) in [(&mut alice, &a, "alice"), (&mut bob, &b, "bob")] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let job = loop {
            let response = http
                .get(format!(
                    "{control}api/v1/sessions/{session}/requests/acp-turn-{name}"
                ))
                .bearer_auth(&tokens[name])
                .send()
                .await
                .unwrap();
            if response.status().is_success() {
                break response.json::<Value>().await.unwrap();
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        let approval = loop {
            let current: Value = http
                .get(format!(
                    "{control}api/v1/jobs/{}",
                    job["id"].as_str().unwrap()
                ))
                .bearer_auth(&tokens[name])
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
                .find_map(|w| w["target"]["approval_id"].as_str().map(str::to_owned))
            {
                break id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "ACP Job did not wait for approval: {current}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        // An unsolicited editor permission answer cannot approve enterprise work.
        editor.send(json!({"jsonrpc":"2.0","id":"acp-agent-fake","result":{"outcome":{"outcome":"selected","optionId":"allow_always"}}})).await;
        let review: Value = http
            .get(format!("{control}api/v1/approvals/{approval}"))
            .bearer_auth(&tokens[name])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(review["state"], "pending");
        http.post(format!("{control}api/v1/approvals/{approval}/answer"))
            .bearer_auth(&tokens[name])
            .json(&json!({"requestId":format!("acp-approve-{name}"),"answer":"approve"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(editor.response(4).await["result"]["stopReason"], "end_turn");
        assert!(
            editor
                .events
                .iter()
                .any(|v| v["method"] == "_zuno/activity")
        );
        assert!(editor.events.iter().any(
            |v| v.pointer("/params/update/content/text") == Some(&json!("ACP-BRIDGE-COMPLETE"))
        ));
    }
    alice
        .send(json!({"jsonrpc":"2.0","id":5,"method":"session/close","params":{"sessionId":a}}))
        .await;
    assert!(alice.response(5).await["result"].is_object());
    alice.send(json!({"jsonrpc":"2.0","id":6,"method":"session/load","params":{"sessionId":a,"cwd":"/editor","mcpServers":[]}})).await;
    let loaded = alice.response(6).await;
    assert!(loaded["result"]["_meta"]["zuno"]["history"]["through"].is_string());
    let cancelled = alice.new_session(8).await;
    alice.send(json!({"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":cancelled,
        "prompt":[{"type":"text","text":"ACP-PROBE alice cancel"}],"_meta":{"zuno":{"requestId":"acp-cancel"}}}})).await;
    let (cancel_job, _) = waiting(http, control, &tokens["alice"], &cancelled, "acp-cancel").await;
    alice
        .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":cancelled}}))
        .await;
    assert_eq!(alice.response(9).await["result"]["stopReason"], "cancelled");
    let cancelled_view: Value = http
        .get(format!("{control}api/v1/jobs/{cancel_job}"))
        .bearer_auth(&tokens["alice"])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cancelled_view["stopRequested"] == true || cancelled_view["phase"] == "cancelled");
    let detached = bob.new_session(8).await;
    bob.send(json!({"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":detached,
        "prompt":[{"type":"text","text":"ACP-PROBE bob detached"}],"_meta":{"zuno":{"requestId":"acp-detached"}}}})).await;
    let (detached_job, approval) =
        waiting(http, control, &tokens["bob"], &detached, "acp-detached").await;
    bob.close().await;
    let detached_view: Value = http
        .get(format!("{control}api/v1/jobs/{detached_job}"))
        .bearer_auth(&tokens["bob"])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detached_view["phase"], "waiting");
    assert_eq!(
        detached_view["stopRequested"], false,
        "disconnect does not cancel durable work"
    );
    let mut resumed = Editor::open(control, &tokens["bob"], fixture, root, "bob-resumed").await;
    resumed.send(json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":detached,"cwd":"/editor","mcpServers":[]}})).await;
    assert!(resumed.response(2).await["result"].is_object());
    resumed.send(json!({"jsonrpc":"2.0","id":3,"method":"_zuno/observe","params":{"sessionId":detached,"jobId":detached_job}})).await;
    http.post(format!("{control}api/v1/approvals/{approval}/answer"))
        .bearer_auth(&tokens["bob"])
        .json(&json!({"requestId":"acp-resume-approve","answer":"approve"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let result = resumed.response(3).await;
    assert_eq!(result["result"]["stopReason"], "end_turn");
    assert_eq!(
        result["result"]["_meta"]["zuno"]["jobId"], detached_job,
        "observe must not admit another input"
    );
    resumed.close().await;
    // Token-file rotation must preserve actor identity, not silently attach Bob
    // to Alice's already populated local connection.
    write(&root.join("acp-alice.token"), &tokens["bob"]);
    alice
        .send(json!({"jsonrpc":"2.0","id":7,"method":"session/list","params":{}}))
        .await;
    assert!(alice.response(7).await["error"].is_object());
    alice.close().await;
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(
        submissions.load(Ordering::SeqCst),
        2,
        "lost response must be resolved by receipt, never a repeated POST"
    );
    stopped.fire();
    proxy_task.await.unwrap();
}

async fn waiting(
    http: &reqwest::Client,
    control: &str,
    token: &str,
    session: &str,
    request: &str,
) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    loop {
        let response = http
            .get(format!(
                "{control}api/v1/sessions/{session}/requests/{request}"
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        if response.status().is_success() {
            let admitted: Value = response.json().await.unwrap();
            let id = admitted["id"].as_str().unwrap();
            let job: Value = http
                .get(format!("{control}api/v1/jobs/{id}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if let Some(approval) = job["waits"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|w| w["target"]["approval_id"].as_str())
            {
                return (id.to_owned(), approval.to_owned());
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "ACP wait deadline");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
