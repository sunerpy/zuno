//! `server.port` reaches the bound socket, through the production `serve` handler.
//!
//! The unit tests beside `listen_config` prove the precedence rules. They cannot prove
//! that `serve` still calls it: a revert to the old `ServerConfig::default()
//! .with_hostname(&args.hostname).with_port(args.port)` composition would leave those
//! tests green while the configured port went back to being read by nobody. So this
//! runs the shipped binary against a `zuno.json` whose `server.port` is the only source
//! of the number, and reads the port out of the readiness line the user sees.

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{Value, json};
use zuno_session_control::QuestionService;
use zuno_tool::question::QuestionPort;
use zuno_types::question::{
    QuestionMode, QuestionOrigin, QuestionPrompt, QuestionPurpose, QuestionSpec, QuestionView,
};

/// Long enough for a cold binary to open its database and bind, short enough that a
/// regression fails the run instead of hanging it.
const READINESS_TIMEOUT: Duration = Duration::from_secs(90);

/// A port nothing is listening on, obtained by binding and releasing it.
fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port for the fixture");
    listener
        .local_addr()
        .expect("the bound fixture address")
        .port()
}

/// Collect a stream's lines on its own thread.
///
/// Both streams need a reader for the whole life of the child. `wait_with_output` would
/// read them for us, but it returns only once every writer has closed its end, and a
/// stream nobody reads is worse still: the child blocks forever on a full pipe.
fn collect(stream: impl std::io::Read + Send + 'static, sink: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if sink.send(line).is_err() {
                break;
            }
        }
    });
}

/// End the process this fixture spawned — that one, and nothing below it.
///
/// One `zuno` invocation is one process on every platform, so the child this fixture
/// holds *is* the server and its pid is the whole server. A tree kill would work too,
/// and that is the reason not to use one: it would also end a second process below the
/// pid the fixture holds, so a reintroduced double-spawn would leave this fixture green
/// while a client's `wait_with_output` blocked forever on the inherited pipes. Killing
/// exactly what was spawned keeps that regression observable here, as the hang this
/// fixture's deadline turns into a failure.
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        terminate(&mut self.0);
    }
}

fn seed_question(root: &Path, database: &Path) -> QuestionView {
    let pool = Arc::new(
        zuno_db::Pool::open(&zuno_paths::DbLocation::File(database.to_path_buf())).expect("pool"),
    );
    let directory = root.to_string_lossy();
    {
        let mut connection = pool.get().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes) \
             VALUES ('global',?1,0,0,'[]')",
                [directory.as_ref()],
            )
            .expect("project");
    }
    zuno_db::session::Store::new(&pool)
        .create(&zuno_db::session::SessionCreate::new(
            "ses_serve_question",
            "serve-question",
            "global",
            directory.as_ref(),
            directory.as_ref(),
            "Serve question",
            "test",
        ))
        .expect("session");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(QuestionService::new(pool).open(QuestionSpec {
            origin: QuestionOrigin {
                session_id: "ses_serve_question".to_owned(),
                message_id: None,
                call_id: None,
                turn_id: None,
                goal_id: None,
            },
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::Clarification,
            questions: vec![
                QuestionPrompt::new("Which target?", "Target", Vec::new()).into_request(),
            ],
            expected_goal_revision: None,
            plan: None,
        }))
        .expect("durable question")
        .question
}

async fn verify_question_routes(port: u16, opened: &QuestionView) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client");
    let base = format!("http://127.0.0.1:{port}");
    let pending = client
        .get(format!("{base}/api/session/ses_serve_question/question"))
        .send()
        .await
        .expect("pending request")
        .error_for_status()
        .expect("question provider mounted")
        .json::<Value>()
        .await
        .expect("pending JSON");
    assert_eq!(
        pending["data"],
        json!([opened]),
        "serve must read the same durable question pool"
    );
    let document = client
        .get(format!("{base}/api/doc"))
        .send()
        .await
        .expect("OpenAPI")
        .error_for_status()
        .expect("OpenAPI status")
        .json::<Value>()
        .await
        .expect("OpenAPI JSON");
    assert!(
        document["paths"]["/api/session/{sessionID}/question/{requestID}/defer"]["post"]
            .is_object()
    );

    let invalid = client
        .post(format!(
            "{base}/api/session/ses_serve_question/question/{}/reply",
            opened.id
        ))
        .json(&json!({"answers": []}))
        .send()
        .await
        .expect("malformed legacy reply");
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    let stream = client
        .get(format!("{base}/api/session/ses_serve_question/event"))
        .send()
        .await
        .expect("SSE")
        .error_for_status()
        .expect("SSE status");
    let mut chunks = stream.bytes_stream();
    let command = json!({
        "commandId": "serve-defer",
        "expectedRevision": opened.revision,
        "action": {"type": "defer", "draftAnswers": {
            (opened.questions[0].id.as_str()): ["serve-client-draft"]
        }}
    });
    let endpoint = format!(
        "{base}/api/session/ses_serve_question/question/{}/defer",
        opened.id
    );
    let deferred = client
        .post(&endpoint)
        .json(&command)
        .send()
        .await
        .expect("defer")
        .error_for_status()
        .expect("defer status")
        .json::<Value>()
        .await
        .expect("receipt");
    assert_eq!(deferred["data"]["question"]["state"], "pending");
    assert_eq!(deferred["data"]["question"]["answers"], json!({}));
    assert_eq!(
        deferred["data"]["question"]["draftAnswers"],
        json!({(opened.questions[0].id.as_str()): ["serve-client-draft"]})
    );
    assert!(deferred["data"].get("inputId").is_none());
    assert_eq!(deferred["data"]["duplicate"], false);
    let updated = tokio::time::timeout(Duration::from_secs(10), async {
        let mut parser = zuno_llm::sse::SseParser::new();
        'updates: loop {
            let chunk = chunks
                .next()
                .await
                .expect("SSE remains open")
                .expect("SSE bytes");
            for event in parser.push(&chunk) {
                let payload: Value = event
                    .expect("SSE frame")
                    .deserialize("zuno-server", "serve-test")
                    .expect("SSE JSON");
                if payload["type"] == "question.updated"
                    && payload["data"]["question"]["id"] == opened.id
                {
                    break 'updates payload;
                }
            }
        }
    })
    .await
    .expect("the production question event forwarder publishes the committed update");
    assert_eq!(updated["data"]["question"], deferred["data"]["question"]);
    let duplicate = client
        .post(&endpoint)
        .json(&command)
        .send()
        .await
        .expect("repeat defer")
        .error_for_status()
        .expect("repeat status")
        .json::<Value>()
        .await
        .expect("repeat receipt");
    assert_eq!(duplicate["data"]["duplicate"], true);
    assert!(duplicate["data"].get("inputId").is_none());
}

#[test]
fn serve_binds_the_port_configured_in_zuno_json() {
    let root = tempfile::tempdir().expect("private serve root");
    let port = free_port();
    std::fs::write(
        root.path().join("zuno.json"),
        format!("{{\n  \"server\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("project configuration");
    let database = root.path().join("zuno.db");
    let question = seed_question(root.path(), &database);

    // The isolation mirrors `tests/surface.rs`: no developer config, credentials,
    // sessions or database, and no network fetches during startup.
    let mut child = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_zuno"))
            .arg("serve")
            .current_dir(root.path())
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("HOME", root.path().join("home"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("ZUNO_DB", &database)
            .env_remove("ZUNO_SERVER_PASSWORD")
            .env_remove("ZUNO_SERVER_USERNAME")
            .env("ZUNO_DISABLE_AUTOUPDATE", "true")
            .env("ZUNO_DISABLE_MODELS_FETCH", "true")
            .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the shipped binary must start"),
    );

    let (sender, receiver) = mpsc::channel();
    collect(child.0.stdout.take().expect("piped stdout"), sender.clone());
    let stderr_lines = Arc::new(Mutex::new(Vec::new()));
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    collect(child.0.stderr.take().expect("piped stderr"), stderr_sender);
    {
        let stderr_lines = Arc::clone(&stderr_lines);
        std::thread::spawn(move || {
            for line in stderr_receiver {
                stderr_lines.lock().expect("stderr transcript").push(line);
            }
        });
    }

    // One deadline for the whole wait, not one per line: a binary that streams
    // anything at all must not be able to hold the suite open indefinitely.
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut transcript = Vec::new();
    let readiness = loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break None;
        };
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                let matched = line.contains("listening on");
                transcript.push(line.clone());
                if matched {
                    break Some(line);
                }
            }
            Err(_) => break None,
        }
    };

    let stderr = stderr_lines.lock().expect("stderr transcript").join("\n");

    let readiness = readiness.unwrap_or_else(|| {
        panic!(
            "`serve` never reported a listening address; stdout: {transcript:?}; stderr: {stderr}"
        )
    });
    assert!(
        readiness.ends_with(&format!("127.0.0.1:{port}")),
        "`server.port` did not reach the bound socket; readiness line: {readiness}"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("HTTP runtime")
        .block_on(verify_question_routes(port, &question));
}
