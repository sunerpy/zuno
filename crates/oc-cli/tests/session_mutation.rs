use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use oc_testkit::env::DbChoice;
use oc_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

const CASSETTE: &str = "openai-chat/streams-text";
const SESSION_ID: &str = "ses_task129entryparity0000000000";
const PROMPT: &str = "answer from the recorded cassette";
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_TIMEOUT: Duration = Duration::from_secs(5);

const FAILING_TOOL_DEFINITION_PLUGIN: &str = r#"
import { appendFileSync } from "node:fs";

export default {
  id: "http-failing-tool-definition",
  server: async (_input, options) => ({
    "tool.definition": async (input, _output) => {
      appendFileSync(options.callLog, `${input.toolID}\n`);
      if (input.toolID === "question") {
        throw new Error("intentional HTTP tool.definition failure");
      }
    },
  }),
};
"#;

const NOOP_TOOL_DEFINITION_PLUGIN: &str = r#"
import { appendFileSync } from "node:fs";

export default {
  id: "http-noop-tool-definition",
  server: async (_input, options) => ({
    "tool.definition": async (input, _output) => {
      appendFileSync(options.callLog, `${input.toolID}\n`);
    },
  }),
};
"#;

const FAILING_AUTH_LOADER_PLUGIN: &str = r#"
export default {
  id: "http-failing-auth-loader",
  server: async () => ({
    auth: {
      provider: "test",
      loader: async (getAuth) => {
        await getAuth();
        throw new Error("task173 HTTP auth loader failure");
      },
      methods: [],
    },
  }),
};
"#;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-rust"))
}

fn provider_config(base_url: &str) -> String {
    json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test Model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": { "context": 100_000, "output": 10_000 },
                        "cost": { "input": 0, "output": 0 },
                        "options": {}
                    }
                },
                "options": {
                    "apiKey": "test-key",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    })
    .to_string()
}

fn broker_provider_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["permission"] = json!({
        "bash": "ask",
        "question": "allow"
    });
    config.to_string()
}

fn tool_definition_plugin_provider_config(
    base_url: &str,
    plugin: &Path,
    call_log: &Path,
) -> String {
    let mut config: Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["plugin"] = json!([[
        format!("file:{}", plugin.display()),
        { "callLog": call_log }
    ]]);
    config.to_string()
}

fn failing_auth_loader_provider_config(base_url: &str, plugin: &Path) -> String {
    let mut config: Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["plugin"] = json!([[format!("file:{}", plugin.display()), {}]]);
    config.to_string()
}

fn variables(env: &ScriptedEnv, base_url: &str, database: &Path) -> BTreeMap<String, String> {
    variables_with_config(env, database, provider_config(base_url))
}

fn variables_with_config(
    env: &ScriptedEnv,
    database: &Path,
    config: String,
) -> BTreeMap<String, String> {
    let mut variables = env
        .env_vars()
        .into_iter()
        .map(|(key, value)| (oc_paths::env::accepted_env_name(&key).to_owned(), value))
        .collect::<BTreeMap<_, _>>();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("ZUNO_PURE".to_owned(), "1".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_DISABLE_DEFAULT_PLUGINS".to_owned(), "true".to_owned()),
        ("ZUNO_DISABLE_LSP_DOWNLOAD".to_owned(), "true".to_owned()),
        (
            "ZUNO_DB".to_owned(),
            database.to_string_lossy().into_owned(),
        ),
        ("OPENCODE_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

fn seed_session(database: &Path, project: &Path) {
    let mut connection = oc_db::open_at(database).expect("open isolated database");
    oc_db::migration::apply(&mut connection).expect("apply production schema");
    let resolved = oc_paths::project::resolve_project(project);
    let now = oc_db::message::now_millis();
    connection
        .execute(
            "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, ?3, ?4, ?4, '[]') ON CONFLICT (id) DO NOTHING",
            rusqlite::params![
                resolved.id,
                resolved.directory.to_string_lossy(),
                resolved.vcs.as_ref().map(|_| "git"),
                now,
            ],
        )
        .expect("seed project");
    let mut input = oc_db::session::SessionCreate::new(
        SESSION_ID,
        "task129-entry-parity",
        &resolved.id,
        resolved.directory.to_string_lossy(),
        project.to_string_lossy(),
        "Task 129 entry parity",
        env!("CARGO_PKG_VERSION"),
    )
    .at(now);
    input.agent = Some("build".to_owned());
    input.model = Some(oc_db::session::model_reference("test", "test-model"));
    let transaction = connection.transaction().expect("start session transaction");
    oc_db::session::create(&transaction, &input).expect("seed fixed session");
    transaction.commit().expect("commit fixed session");
}

async fn run_prompt(env: &ScriptedEnv, base_url: &str, database: &Path) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args([
            "run",
            "--session",
            SESSION_ID,
            "--model",
            "test/test-model",
            PROMPT,
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, base_url, database));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the CLI turn must finish inside its budget")
        .expect("launch production run entry point")
}

struct RunningServer {
    child: tokio::process::Child,
    _stdout: BufReader<tokio::process::ChildStdout>,
    base_url: String,
}

struct HangingProvider {
    base_url: String,
    requested: Option<tokio::sync::oneshot::Receiver<()>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl HangingProvider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hanging provider");
        let addr = listener
            .local_addr()
            .expect("read hanging provider address");
        let (requested_tx, requested_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let (mut stream, _) = tokio::select! {
                accepted = listener.accept() => accepted.expect("accept provider request"),
                _ = &mut shutdown_rx => return,
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream
                    .read(&mut buffer)
                    .await
                    .expect("read provider request");
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\n\
                      transfer-encoding: chunked\r\nconnection: keep-alive\r\n\r\n",
                )
                .await
                .expect("write hanging response headers");
            stream
                .flush()
                .await
                .expect("flush hanging response headers");
            let _ = requested_tx.send(());
            let _ = shutdown_rx.await;
        });
        Self {
            base_url: format!("http://{addr}"),
            requested: Some(requested_rx),
            shutdown: Some(shutdown_tx),
            join,
        }
    }

    async fn wait_for_request(&mut self) {
        tokio::time::timeout(
            RUN_TIMEOUT,
            self.requested.take().expect("request receiver exists"),
        )
        .await
        .expect("turn must reach the provider")
        .expect("provider request notification must be sent");
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = (&mut self.join).await;
    }
}

impl Drop for HangingProvider {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.join.abort();
    }
}

impl RunningServer {
    async fn start(env: &ScriptedEnv, provider_base_url: &str, database: &Path) -> Self {
        Self::start_with_config(env, database, provider_config(provider_base_url)).await
    }

    async fn start_with_config(env: &ScriptedEnv, database: &Path, config: String) -> Self {
        Self::start_with_variables(env, variables_with_config(env, database, config)).await
    }

    async fn start_with_plugin_config(env: &ScriptedEnv, database: &Path, config: String) -> Self {
        let mut variables = variables_with_config(env, database, config);
        variables.remove("ZUNO_PURE");
        variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
        variables.insert(
            "MISE_DATA_DIR".to_owned(),
            "/config/.local/share/mise".to_owned(),
        );
        variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
        Self::start_with_variables(env, variables).await
    }

    async fn start_with_failing_auth_loader(
        env: &ScriptedEnv,
        database: &Path,
        config: String,
    ) -> Self {
        let mut variables = variables_with_config(env, database, config);
        variables.remove("ZUNO_PURE");
        variables.insert("XDG_CACHE_HOME".to_owned(), "/config/.cache".to_owned());
        variables.insert(
            "MISE_DATA_DIR".to_owned(),
            "/config/.local/share/mise".to_owned(),
        );
        variables.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
        variables.insert(
            "ZUNO_AUTH_CONTENT".to_owned(),
            r#"{"test":{"type":"api","key":"fixture-key"}}"#.to_owned(),
        );
        Self::start_with_variables(env, variables).await
    }

    async fn start_with_variables(env: &ScriptedEnv, variables: BTreeMap<String, String>) -> Self {
        let mut command = tokio::process::Command::new(binary());
        command
            .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
            .current_dir(env.working_dir())
            .env_clear()
            .envs(variables)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .expect("launch production serve entry point");
        let stdout = child.stdout.take().expect("server stdout is piped");
        let mut stdout = BufReader::new(stdout);
        let base_url = tokio::time::timeout(RUN_TIMEOUT, async {
            let mut line = String::new();
            loop {
                line.clear();
                let read = stdout
                    .read_line(&mut line)
                    .await
                    .expect("read server readiness line");
                assert!(read > 0, "server exited before reporting readiness");
                if let Some(url) = line.trim().strip_prefix("opencode server listening on ") {
                    break url.to_owned();
                }
            }
        })
        .await
        .expect("server must report readiness inside its budget");
        Self {
            child,
            _stdout: stdout,
            base_url,
        }
    }

    async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(RUN_TIMEOUT, self.child.wait()).await;
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct SseClient {
    response: reqwest::Response,
    buffered: Vec<u8>,
    frames: VecDeque<Value>,
}

impl SseClient {
    async fn connect(client: &reqwest::Client, url: String) -> Self {
        let response = client.get(url).send().await.expect("open SSE stream");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        Self {
            response,
            buffered: Vec::new(),
            frames: VecDeque::new(),
        }
    }

    async fn next_json(&mut self) -> Value {
        self.next_json_within(SSE_TIMEOUT)
            .await
            .expect("SSE stream must produce a data frame inside its budget")
    }

    async fn next_json_within(&mut self, budget: Duration) -> Option<Value> {
        tokio::time::timeout(budget, async {
            loop {
                self.decode_complete_frames();
                if let Some(frame) = self.frames.pop_front() {
                    return frame;
                }
                let chunk = self
                    .response
                    .chunk()
                    .await
                    .expect("read SSE chunk")
                    .expect("SSE stream remains open");
                self.buffered.extend_from_slice(&chunk);
            }
        })
        .await
        .ok()
    }

    async fn drain_until_idle(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_json_within(Duration::from_millis(250)).await {
            frames.push(frame);
        }
        frames
    }

    async fn read_until_text(&mut self, expected: &str) -> Value {
        tokio::time::timeout(RUN_TIMEOUT, async {
            loop {
                let frame = self.next_json().await;
                if value_contains_text(&frame, expected) {
                    return frame;
                }
            }
        })
        .await
        .expect("the turn's assistant text must arrive over SSE")
    }

    fn decode_complete_frames(&mut self) {
        while let Some(boundary) = self
            .buffered
            .windows(2)
            .position(|window| window == b"\n\n")
        {
            let frame = self.buffered.drain(..boundary + 2).collect::<Vec<_>>();
            let frame = String::from_utf8(frame).expect("SSE frames are UTF-8");
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                self.frames
                    .push_back(serde_json::from_str(&data).expect("SSE data is JSON"));
            }
        }
    }
}

fn value_contains_text(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(text) => text.contains(expected),
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_text(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_text(value, expected)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn advertised_tools(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn compatible_sse(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

fn broker_scenario(name: &str, tool: &str, arguments: Value, completion: &str) -> Scenario {
    let arguments = arguments.to_string();
    Scenario::new(name)
        .on_path("/v1/chat/completions")
        .respond(MockResponse::authored(
            200,
            "text/event-stream",
            compatible_sse([
                json!({"choices":[{"delta":{"tool_calls":[{
                    "index": 0,
                    "id": format!("call_{tool}"),
                    "function": {"name": tool, "arguments": arguments}
                }]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ]),
            "the broker test needs a deterministic tool call whose request id is discovered at runtime",
        ))
        .respond(MockResponse::authored(
            200,
            "text/event-stream",
            compatible_sse([
                json!({"choices":[{"delta":{"content": completion},"finish_reason":"stop"}]})
            ]),
            "the broker test needs a deterministic completion after the HTTP reply",
        ))
}

async fn submit_http_prompt(client: &reqwest::Client, server: &RunningServer, message_id: &str) {
    let response = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/prompt",
            server.base_url
        ))
        .json(&json!({
            "id": message_id,
            "prompt": { "text": PROMPT },
            "delivery": "steer"
        }))
        .send()
        .await
        .expect("submit broker HTTP prompt");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

async fn wait_for_pending(client: &reqwest::Client, url: String) -> Value {
    tokio::time::timeout(RUN_TIMEOUT, async {
        loop {
            let body = client
                .get(&url)
                .send()
                .await
                .expect("read pending requests")
                .json::<Value>()
                .await
                .expect("pending request response is JSON");
            if let Some(request) = body["data"].as_array().and_then(|data| data.first()) {
                return request.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the HTTP-driven turn must park on a pending request")
}

async fn wait_for_http_turn(client: &reqwest::Client, server: &RunningServer) {
    let response = tokio::time::timeout(
        RUN_TIMEOUT,
        client
            .post(format!("{}/api/session/{SESSION_ID}/wait", server.base_url))
            .send(),
    )
    .await
    .expect("HTTP turn must finish inside its budget")
    .expect("wait for brokered HTTP turn");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
}

async fn assert_message_contains(client: &reqwest::Client, server: &RunningServer, expected: &str) {
    let messages = client
        .get(format!(
            "{}/api/session/{SESSION_ID}/message",
            server.base_url
        ))
        .send()
        .await
        .expect("read completed brokered turn")
        .json::<Value>()
        .await
        .expect("message response is JSON");
    assert!(
        value_contains_text(&messages["data"], expected),
        "completed turn omitted `{expected}`: {messages}"
    );
}

fn scrub_dynamic(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("time");
        if object.contains_key("parentID") {
            object.insert("parentID".to_owned(), Value::String("<parent>".to_owned()));
        }
    }
    value
}

fn persisted_semantics(database: &Path) -> Value {
    let connection = oc_db::open_at(database).expect("open completed turn database");
    let session = oc_db::session::get(&connection, SESSION_ID).expect("read fixed session");
    let messages = oc_db::message::MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate completed turn")
        .into_iter()
        .map(|message| {
            let parts = message
                .parts
                .into_iter()
                .map(|part| scrub_dynamic(Value::Object(part.data)))
                .collect::<Vec<_>>();
            json!({
                "role": message.info.role.to_string(),
                "data": scrub_dynamic(Value::Object(message.info.data)),
                "parts": parts,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "session": {
            "title": session.title,
            "agent": session.agent,
            "model": session.model,
            "cost": session.cost,
            "tokens": {
                "input": session.tokens.input,
                "output": session.tokens.output,
                "reasoning": session.tokens.reasoning,
                "cacheRead": session.tokens.cache_read,
                "cacheWrite": session.tokens.cache_write,
            },
            "revert": session.revert,
        },
        "messages": messages,
    })
}

fn assistant_text(database: &Path) -> String {
    let connection = oc_db::open_at(database).expect("open completed turn database");
    oc_db::message::MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate completed turn")
        .into_iter()
        .filter(|message| message.info.role.to_string() == "assistant")
        .flat_map(|message| message.parts)
        .filter_map(|part| {
            (part.kind.to_string() == "text")
                .then(|| {
                    part.data
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_prompt_matches_cli_output_and_persisted_rows_on_the_same_cassette() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let cli_database = env.xdg_data().join("cli-turn.db");
    let http_database = env.xdg_data().join("http-turn.db");
    seed_session(&cli_database, env.project());
    seed_session(&http_database, env.project());

    let scenario = Scenario::new("same-recording-through-both-entry-points")
        .from_oracle_cassette(CASSETTE)
        .expect("first copy of the recorded completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("second copy of the recorded completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    assert!(
        provider.authored_scenarios().is_empty(),
        "entry-point parity must use recorded provider bytes only"
    );

    let cli = run_prompt(&env, provider.base_url(), &cli_database).await;
    assert!(
        cli.status.success(),
        "CLI turn failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&cli.stdout),
        String::from_utf8_lossy(&cli.stderr)
    );

    let mut server = RunningServer::start(&env, provider.base_url(), &http_database).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");
    let prompt = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/prompt",
            server.base_url
        ))
        .json(&json!({
            "id": "msg_task129_http_user",
            "prompt": { "text": PROMPT },
            "delivery": "steer"
        }))
        .send()
        .await
        .expect("submit HTTP prompt");
    assert_eq!(prompt.status(), reqwest::StatusCode::OK);
    let wait = tokio::time::timeout(
        RUN_TIMEOUT,
        client
            .post(format!("{}/api/session/{SESSION_ID}/wait", server.base_url))
            .send(),
    )
    .await
    .expect("HTTP wait must observe turn completion")
    .expect("send HTTP wait");
    assert_eq!(wait.status(), reqwest::StatusCode::NO_CONTENT);

    let cli_output = String::from_utf8(cli.stdout)
        .expect("CLI output is UTF-8")
        .trim()
        .to_owned();
    assert_eq!(assistant_text(&cli_database), cli_output);
    assert_eq!(assistant_text(&http_database), cli_output);
    assert_eq!(
        persisted_semantics(&http_database),
        persisted_semantics(&cli_database),
        "serve and run must persist the same normalized session/message/part rows"
    );
    assert_eq!(
        provider.captured_count().await,
        2,
        "each entry point must make exactly one real turn request"
    );

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_client_reads_one_prompt_answer_from_message_history_and_preopened_session_sse() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-readback.db");
    seed_session(&database, env.project());
    let scenario = Scenario::new("http-client-readback")
        .from_oracle_cassette(CASSETTE)
        .expect("recorded completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let mut server = RunningServer::start(&env, provider.base_url(), &database).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");
    let mut session_events = SseClient::connect(
        &client,
        format!("{}/api/session/{SESSION_ID}/event?after=0", server.base_url),
    )
    .await;

    let prompt = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/prompt",
            server.base_url
        ))
        .json(&json!({
            "id": "msg_task131_http_user",
            "prompt": { "text": PROMPT },
            "delivery": "steer"
        }))
        .send()
        .await
        .expect("submit HTTP prompt");
    assert_eq!(prompt.status(), reqwest::StatusCode::OK);
    let wait = client
        .post(format!("{}/api/session/{SESSION_ID}/wait", server.base_url))
        .send()
        .await
        .expect("wait for HTTP turn");
    assert_eq!(wait.status(), reqwest::StatusCode::NO_CONTENT);

    let session_event = session_events.read_until_text("Hello").await;
    let messages = client
        .get(format!(
            "{}/api/session/{SESSION_ID}/message",
            server.base_url
        ))
        .send()
        .await
        .expect("read messages")
        .json::<Value>()
        .await
        .expect("messages response is JSON");
    let history = client
        .get(format!(
            "{}/api/session/{SESSION_ID}/history",
            server.base_url
        ))
        .send()
        .await
        .expect("read history")
        .json::<Value>()
        .await
        .expect("history response is JSON");
    assert!(
        value_contains_text(&session_event, "Hello"),
        "pre-opened session SSE omitted the assistant answer: {session_event}"
    );
    assert!(
        value_contains_text(&messages["data"], "Hello"),
        "message read omitted the assistant answer: {messages}"
    );
    assert!(
        value_contains_text(&history["data"], "Hello"),
        "history read omitted the assistant answer: {history}"
    );

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_http_event_stream_carries_the_http_driven_turn_after_server_connected() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-global-events.db");
    seed_session(&database, env.project());
    let scenario = Scenario::new("http-global-event-readback")
        .from_oracle_cassette(CASSETTE)
        .expect("recorded completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let mut server = RunningServer::start(&env, provider.base_url(), &database).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");
    let mut global_events =
        SseClient::connect(&client, format!("{}/api/event", server.base_url)).await;
    let connected = global_events.next_json().await;
    assert_eq!(connected["type"], "server.connected");

    let prompt = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/prompt",
            server.base_url
        ))
        .json(&json!({
            "id": "msg_task131_global_user",
            "prompt": { "text": PROMPT },
            "delivery": "steer"
        }))
        .send()
        .await
        .expect("submit HTTP prompt");
    assert_eq!(prompt.status(), reqwest::StatusCode::OK);
    let wait = client
        .post(format!("{}/api/session/{SESSION_ID}/wait", server.base_url))
        .send()
        .await
        .expect("wait for HTTP turn");
    assert_eq!(wait.status(), reqwest::StatusCode::NO_CONTENT);

    let turn_event = global_events.read_until_text("Hello").await;
    assert_ne!(turn_event["type"], "server.connected");
    assert!(
        value_contains_text(&turn_event, "Hello"),
        "global SSE omitted the HTTP-driven turn: {turn_event}"
    );

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_tool_definition_hook_is_disabled_and_http_turn_completes_with_a_diagnostic() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-plugin-diagnostic.db");
    seed_session(&database, env.project());
    let plugin = env.project().join("http-failing-tool-definition.mjs");
    let call_log = env.project().join("http-failing-tool-definition.calls");
    std::fs::write(&plugin, FAILING_TOOL_DEFINITION_PLUGIN).expect("write failing plugin");
    let scenario = Scenario::new("http-plugin-failure-is-contained")
        .from_oracle_cassette(CASSETTE)
        .expect("recorded completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let config = tool_definition_plugin_provider_config(provider.base_url(), &plugin, &call_log);
    let mut server = RunningServer::start_with_plugin_config(&env, &database, config).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");
    let mut session_events = SseClient::connect(
        &client,
        format!("{}/api/session/{SESSION_ID}/event?after=0", server.base_url),
    )
    .await;

    submit_http_prompt(&client, &server, "msg_task168_plugin_failure").await;
    wait_for_http_turn(&client, &server).await;

    let (diagnostic, completed) = tokio::time::timeout(RUN_TIMEOUT, async {
        let mut completed = false;
        loop {
            let frame = session_events.next_json().await;
            assert_ne!(
                frame["type"], "session.error",
                "hook failure killed HTTP turn: {frame}"
            );
            completed |= value_contains_text(&frame, "turn.completed");
            if value_contains_text(&frame, "http-failing-tool-definition")
                && value_contains_text(&frame, "tool.definition")
                && value_contains_text(&frame, "intentional HTTP tool.definition failure")
            {
                break (frame, completed);
            }
        }
    })
    .await
    .expect("HTTP SSE must publish the plugin diagnostic inside the turn budget");
    assert!(
        completed,
        "turn.completed must precede the contained diagnostic: {diagnostic}"
    );
    assert_message_contains(&client, &server, "Hello").await;
    let calls = std::fs::read_to_string(&call_log).expect("read HTTP tool.definition calls");
    assert_eq!(
        calls.lines().filter(|tool| *tool == "question").count(),
        1,
        "the HTTP plugin must fail once on the deliberately failing tool: {calls:?}"
    );
    assert_eq!(
        calls.lines().last(),
        Some("question"),
        "the disabled HTTP plugin must receive no later definitions: {calls:?}"
    );

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noop_tool_definition_hook_preserves_real_schemas_and_stays_enabled_over_http() {
    let baseline_env = ScriptedEnv::new()
        .expect("isolated baseline environment")
        .with_db(DbChoice::Default);
    let baseline_database = baseline_env
        .xdg_data()
        .join("http-tool-definition-baseline.db");
    seed_session(&baseline_database, baseline_env.project());
    let baseline_scenario = Scenario::new("http-tool-definition-baseline")
        .from_oracle_cassette(CASSETTE)
        .expect("recorded baseline completion loads");
    let baseline_provider = MockProvider::start(vec![baseline_scenario])
        .await
        .expect("baseline mock provider binds loopback");
    let mut baseline_server = RunningServer::start(
        &baseline_env,
        baseline_provider.base_url(),
        &baseline_database,
    )
    .await;
    let baseline_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build baseline loopback client");

    submit_http_prompt(
        &baseline_client,
        &baseline_server,
        "msg_task172_http_baseline",
    )
    .await;
    wait_for_http_turn(&baseline_client, &baseline_server).await;
    assert_message_contains(&baseline_client, &baseline_server, "Hello").await;
    let baseline_captured = baseline_provider.captured().await;
    baseline_server.stop().await;
    baseline_provider.shutdown().await;

    let env = ScriptedEnv::new()
        .expect("isolated plugin environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-noop-tool-definition.db");
    seed_session(&database, env.project());
    let plugin = env.project().join("http-noop-tool-definition.mjs");
    let call_log = env.project().join("http-noop-tool-definition.calls");
    std::fs::write(&plugin, NOOP_TOOL_DEFINITION_PLUGIN)
        .expect("write no-op HTTP tool.definition plugin");
    let scenario = Scenario::new("http-noop-tool-definition-round-trip")
        .from_oracle_cassette(CASSETTE)
        .expect("recorded plugin completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("plugin mock provider binds loopback");
    let config = tool_definition_plugin_provider_config(provider.base_url(), &plugin, &call_log);
    let mut server = RunningServer::start_with_plugin_config(&env, &database, config).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build plugin loopback client");
    let mut session_events = SseClient::connect(
        &client,
        format!("{}/api/session/{SESSION_ID}/event?after=0", server.base_url),
    )
    .await;

    submit_http_prompt(&client, &server, "msg_task172_http_noop").await;
    wait_for_http_turn(&client, &server).await;
    assert_message_contains(&client, &server, "Hello").await;
    let frames = session_events.drain_until_idle().await;
    let captured = provider.captured().await;

    assert_eq!(
        baseline_captured.len(),
        1,
        "baseline makes one turn request"
    );
    assert_eq!(captured.len(), 1, "plugin makes one turn request");
    let baseline_turn = baseline_captured[0]
        .json()
        .expect("baseline provider request is JSON");
    let plugin_turn = captured[0].json().expect("plugin provider request is JSON");
    assert_eq!(
        serde_json::to_vec(&plugin_turn["tools"]).expect("serialize plugin tools"),
        serde_json::to_vec(&baseline_turn["tools"]).expect("serialize baseline tools"),
        "every real built-in schema must round-trip byte-identically over HTTP"
    );
    assert!(
        frames
            .iter()
            .any(|frame| value_contains_text(frame, "turn.completed")),
        "HTTP session stream omitted turn completion: {frames:#?}"
    );
    assert!(
        frames.iter().all(|frame| frame["type"] != "session.error"),
        "the no-op hook must not fail the HTTP turn: {frames:#?}"
    );
    assert!(
        frames.iter().all(|frame| {
            !value_contains_text(frame, "disabled plugin")
                && !value_contains_text(frame, "http-noop-tool-definition")
        }),
        "host data must not produce a plugin diagnostic over HTTP: {frames:#?}"
    );
    let calls = std::fs::read_to_string(&call_log).expect("read no-op HTTP hook calls");
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        advertised_tools(&plugin_turn),
        "the no-op HTTP plugin must remain enabled through every definition: {calls:?}"
    );

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_auth_loader_is_disabled_and_http_turn_completes_with_a_diagnostic() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-auth-loader-diagnostic.db");
    seed_session(&database, env.project());
    let plugin = env.project().join("http-failing-auth-loader.mjs");
    std::fs::write(&plugin, FAILING_AUTH_LOADER_PLUGIN).expect("write failing auth loader");
    let scenario = Scenario::new("HTTP auth loader failure is contained")
        .from_oracle_cassette(CASSETTE)
        .expect("recorded completion loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let config = failing_auth_loader_provider_config(provider.base_url(), &plugin);
    let mut server = RunningServer::start_with_failing_auth_loader(&env, &database, config).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");
    let mut session_events = SseClient::connect(
        &client,
        format!("{}/api/session/{SESSION_ID}/event?after=0", server.base_url),
    )
    .await;

    submit_http_prompt(&client, &server, "msg_task173_auth_loader_failure").await;
    wait_for_http_turn(&client, &server).await;

    let (diagnostic, completed) = tokio::time::timeout(RUN_TIMEOUT, async {
        let mut completed = false;
        loop {
            let frame = session_events.next_json().await;
            assert_ne!(
                frame["type"], "session.error",
                "auth loader failure killed HTTP turn: {frame}"
            );
            completed |= value_contains_text(&frame, "turn.completed");
            if value_contains_text(&frame, "http-failing-auth-loader")
                && value_contains_text(&frame, "auth.loader")
                && value_contains_text(&frame, "task173 HTTP auth loader failure")
            {
                break (frame, completed);
            }
        }
    })
    .await
    .expect("HTTP SSE must publish the auth-loader diagnostic inside the turn budget");
    assert!(
        completed,
        "turn.completed must precede the contained diagnostic: {diagnostic}"
    );
    assert_message_contains(&client, &server, "Hello").await;

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_interrupt_releases_wait_and_leaves_one_complete_abort_checkpoint() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("interrupted-http-turn.db");
    seed_session(&database, env.project());
    let mut provider = HangingProvider::start().await;
    let mut server = RunningServer::start(&env, &provider.base_url, &database).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");

    let prompt = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/prompt",
            server.base_url
        ))
        .json(&json!({
            "id": "msg_task129_interrupted_user",
            "prompt": { "text": "wait for interruption" },
            "delivery": "steer"
        }))
        .send()
        .await
        .expect("submit interruptible HTTP prompt");
    assert_eq!(prompt.status(), reqwest::StatusCode::OK);
    provider.wait_for_request().await;

    let wait = client
        .post(format!("{}/api/session/{SESSION_ID}/wait", server.base_url))
        .send();
    tokio::pin!(wait);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut wait)
            .await
            .is_err(),
        "wait returned while the provider stream was still active"
    );
    let interrupt = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/interrupt",
            server.base_url
        ))
        .send()
        .await
        .expect("interrupt the active HTTP turn");
    assert_eq!(interrupt.status(), reqwest::StatusCode::NO_CONTENT);
    let waited = tokio::time::timeout(RUN_TIMEOUT, &mut wait)
        .await
        .expect("wait must be notified after interruption")
        .expect("wait request completes");
    assert_eq!(waited.status(), reqwest::StatusCode::NO_CONTENT);

    let connection = oc_db::open_at(&database).expect("open interrupted turn database");
    let messages = oc_db::message::MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate interrupted turn");
    assert_eq!(
        messages.len(),
        2,
        "one user and one assistant row must remain"
    );
    let user = messages
        .iter()
        .find(|message| message.info.role.to_string() == "user")
        .expect("interrupted prompt keeps its user message");
    assert_eq!(user.info.id, "msg_task129_interrupted_user");
    let assistant = messages
        .iter()
        .find(|message| message.info.role.to_string() == "assistant")
        .expect("interrupted turn checkpoints its assistant message");
    assert_eq!(assistant.info.data["parentID"], user.info.id);
    assert_eq!(assistant.info.data["error"]["name"], "AbortError");
    assert!(
        assistant.info.data["time"]["completed"].is_i64(),
        "interrupted assistant must not remain half-written"
    );
    assert!(
        assistant.parts.is_empty(),
        "no provider payload arrived, so interruption must not invent a partial part"
    );
    let orphan_parts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM part p LEFT JOIN message m ON m.id = p.message_id \
             WHERE m.id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count orphan parts");
    assert_eq!(orphan_parts, 0);

    drop(connection);
    server.stop().await;
    provider.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_permission_request_parks_rejects_cross_session_reply_and_resumes() {
    const COMPLETION: &str = "PERMISSION_HTTP_OK";
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-permission-broker.db");
    seed_session(&database, env.project());
    let scenario = broker_scenario(
        "http-permission-broker",
        "bash",
        json!({"command": "pwd", "intent": "prove the HTTP permission broker"}),
        COMPLETION,
    );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let mut server = RunningServer::start_with_config(
        &env,
        &database,
        broker_provider_config(provider.base_url()),
    )
    .await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");

    submit_http_prompt(&client, &server, "msg_task132_permission").await;
    let pending = wait_for_pending(
        &client,
        format!("{}/api/session/{SESSION_ID}/permission", server.base_url),
    )
    .await;
    assert_eq!(pending["sessionID"], SESSION_ID);
    assert_eq!(pending["action"], "bash");
    let request_id = pending["id"]
        .as_str()
        .expect("permission request has an id");

    let reply = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/permission/{request_id}/reply",
            server.base_url
        ))
        .json(&json!({"reply": "once"}))
        .send()
        .await
        .expect("approve pending permission");
    assert_eq!(reply.status(), reqwest::StatusCode::NO_CONTENT);

    wait_for_http_turn(&client, &server).await;
    assert_message_contains(&client, &server, COMPLETION).await;
    assert_eq!(provider.captured_count().await, 2);

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_question_request_parks_and_reply_resumes_the_same_turn() {
    const COMPLETION: &str = "QUESTION_HTTP_OK";
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-question-broker.db");
    seed_session(&database, env.project());
    let scenario = broker_scenario(
        "http-question-broker",
        "question",
        json!({
            "intent": "prove the HTTP question broker",
            "questions": [{
                "question": "Which database?",
                "header": "Database",
                "options": [
                    {"label": "Postgres", "description": "Relational"},
                    {"label": "SQLite", "description": "Embedded"}
                ]
            }]
        }),
        COMPLETION,
    );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let mut server = RunningServer::start_with_config(
        &env,
        &database,
        broker_provider_config(provider.base_url()),
    )
    .await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");

    submit_http_prompt(&client, &server, "msg_task132_question").await;
    let pending =
        wait_for_pending(&client, format!("{}/api/question/request", server.base_url)).await;
    assert_eq!(pending["sessionID"], SESSION_ID);
    assert_eq!(pending["questions"][0]["header"], "Database");
    let request_id = pending["id"].as_str().expect("question request has an id");

    let reply = client
        .post(format!(
            "{}/api/session/{SESSION_ID}/question/{request_id}/reply",
            server.base_url
        ))
        .json(&json!({"answers": [["Postgres"]]}))
        .send()
        .await
        .expect("answer pending question");
    assert_eq!(reply.status(), reqwest::StatusCode::NO_CONTENT);

    wait_for_http_turn(&client, &server).await;
    assert_message_contains(&client, &server, COMPLETION).await;
    assert_eq!(provider.captured_count().await, 2);

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnected_permission_reply_fails_closed_without_running_the_tool() {
    const COMPLETION: &str = "DISCONNECTED_PERMISSION_DENIED";
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env.xdg_data().join("http-permission-disconnect.db");
    let forbidden_side_effect = env.working_dir().join("permission-was-allowed");
    seed_session(&database, env.project());
    let scenario = broker_scenario(
        "http-permission-disconnect",
        "bash",
        json!({
            "command": format!("touch {}", forbidden_side_effect.display()),
            "intent": "prove disconnects deny permission"
        }),
        COMPLETION,
    );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let mut server = RunningServer::start_with_config(
        &env,
        &database,
        broker_provider_config(provider.base_url()),
    )
    .await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");

    submit_http_prompt(&client, &server, "msg_task132_disconnect").await;
    let pending = wait_for_pending(
        &client,
        format!("{}/api/permission/request", server.base_url),
    )
    .await;
    let request_id = pending["id"]
        .as_str()
        .expect("permission request has an id");

    let address = server
        .base_url
        .strip_prefix("http://")
        .expect("server URL uses HTTP");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect raw HTTP client");
    let path = format!("/api/session/{SESSION_ID}/permission/{request_id}/reply");
    stream
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\n\
                 Content-Length: 128\r\nConnection: close\r\n\r\n{{\"reply\":"
            )
            .as_bytes(),
        )
        .await
        .expect("send a deliberately incomplete reply body");
    stream.flush().await.expect("flush incomplete reply");
    stream.shutdown().await.expect("disconnect reply client");
    drop(stream);

    wait_for_http_turn(&client, &server).await;
    assert_message_contains(&client, &server, COMPLETION).await;
    assert!(
        !forbidden_side_effect.exists(),
        "a disconnected reply must reject; it allowed the shell tool to run"
    );
    assert_eq!(provider.captured_count().await, 2);

    server.stop().await;
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnected_only_session_observer_rejects_permission_without_running_the_tool() {
    const COMPLETION: &str = "DISCONNECTED_OBSERVER_DENIED";
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::Default);
    let database = env
        .xdg_data()
        .join("http-permission-observer-disconnect.db");
    let forbidden_side_effect = env.working_dir().join("observer-allowed-permission");
    seed_session(&database, env.project());
    let scenario = broker_scenario(
        "http-permission-observer-disconnect",
        "bash",
        json!({
            "command": format!("touch {}", forbidden_side_effect.display()),
            "intent": "prove the last observer disconnecting denies permission"
        }),
        COMPLETION,
    );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let mut server = RunningServer::start_with_config(
        &env,
        &database,
        broker_provider_config(provider.base_url()),
    )
    .await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client");
    let observer = SseClient::connect(
        &client,
        format!("{}/api/session/{SESSION_ID}/event?after=0", server.base_url),
    )
    .await;

    submit_http_prompt(&client, &server, "msg_task134_observer_disconnect").await;
    let _pending = wait_for_pending(
        &client,
        format!("{}/api/session/{SESSION_ID}/permission", server.base_url),
    )
    .await;

    drop(observer);
    let waited = tokio::time::timeout(
        Duration::from_secs(15),
        client
            .post(format!("{}/api/session/{SESSION_ID}/wait", server.base_url))
            .send(),
    )
    .await
    .expect("dropping the only observer must release the HTTP turn")
    .expect("wait for the observer-disconnected turn");
    assert_eq!(waited.status(), reqwest::StatusCode::NO_CONTENT);
    assert_message_contains(&client, &server, COMPLETION).await;
    assert!(
        !forbidden_side_effect.exists(),
        "the last observer disconnecting must reject; it allowed the shell tool to run"
    );
    assert_eq!(provider.captured_count().await, 2);

    server.stop().await;
    provider.shutdown().await;
}
