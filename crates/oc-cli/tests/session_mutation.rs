use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use oc_testkit::env::DbChoice;
use oc_testkit::{MockProvider, Scenario, ScriptedEnv};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

const CASSETTE: &str = "openai-chat/streams-text";
const SESSION_ID: &str = "ses_task129entryparity0000000000";
const PROMPT: &str = "answer from the recorded cassette";
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

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

fn variables(env: &ScriptedEnv, base_url: &str, database: &Path) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("OPENCODE_PURE".to_owned(), "1".to_owned()),
        ("OPENCODE_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        (
            "OPENCODE_DISABLE_MODELS_FETCH".to_owned(),
            "true".to_owned(),
        ),
        (
            "OPENCODE_DISABLE_DEFAULT_PLUGINS".to_owned(),
            "true".to_owned(),
        ),
        (
            "OPENCODE_DISABLE_LSP_DOWNLOAD".to_owned(),
            "true".to_owned(),
        ),
        (
            "OPENCODE_DB".to_owned(),
            database.to_string_lossy().into_owned(),
        ),
        (
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            provider_config(base_url),
        ),
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
        let mut command = tokio::process::Command::new(binary());
        command
            .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
            .current_dir(env.working_dir())
            .env_clear()
            .envs(variables(env, provider_base_url, database))
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
