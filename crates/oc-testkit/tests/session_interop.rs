//! One session, both binaries, in both directions.
//!
//! # What this exists to catch that nothing else could
//!
//! Success criterion 1 — "side-by-side use and rollback are real rather than
//! claimed" — is a claim about a **session's whole life**, not about one row being
//! parseable. Before this file the round trip was proved at two much weaker points:
//!
//! * `compat_suite.rs::journal_round_trip_through_the_real_binary_does_not_replay_migrations`
//!   opens an **empty** Rust database and compares the `migration` table. It passed
//!   throughout the life of the worst defect this project has had.
//! * `compat_suite.rs::a_session_written_by_this_port_is_decodable_by_the_real_binary`
//!   and `oc-cli/tests/rollback.rs` ask the release to **list** a session this port
//!   wrote. That is the fix for todo 115's `session.model` key name, and it is real,
//!   but "can be listed" is one verb out of five.
//!
//! Neither asks the question a user asks: *if I run a turn on the release, then a
//! turn on this port, then go back — is it still one conversation, and does each
//! side see what the other added?* A session can be listable and still lose its
//! transcript; a transcript can be readable and still not grow. Those failures are
//! invisible to every test above.
//!
//! So each lifecycle test here drives **one persisted session** through
//! `list → open/export → continue → export again → read back by the first binary`,
//! with the two real programs alternating over the same database file, and asserts:
//!
//! 1. the session count stays at **one** — proof both binaries touched the *same*
//!    session rather than each creating its own, which is the failure mode that
//!    would make a two-session test look green while proving nothing;
//! 2. the persisted `message` and `part` counts **strictly grew**;
//! 3. each binary's `export` names **every** turn, including the other binary's;
//! 4. the continuing binary's **outbound provider request** replays the other
//!    binary's persisted user turn. This is the strongest available form of "the
//!    opposite implementation can decode what this one wrote": the bytes did not
//!    merely parse, they came back out of the decoder and onto the wire.
//!
//! # Why assertion 4 needed a discriminating fake provider
//!
//! A new session asks the model for a **title** before its first turn, so a run
//! makes two provider requests of two different kinds against the same path. The
//! shared [`MockProvider`] routes by path and then serves its scenario's responses
//! in arrival order (`mock_provider.rs:533-545`), which means an order-based fixture
//! silently depends on how many preludes a run happens to make. Feed the title
//! request a body meant for the tool loop and it dies with `title model returned no
//! usable line` — a fixture defect that reads exactly like a product defect.
//!
//! [`TranscriptProvider`] below therefore classifies each request by upstream's own
//! title system prompt and answers each kind correctly, so the tests do not depend
//! on request ordering at all. It also records, per request, the user turns the
//! client replayed — which is what assertion 4 reads.
//!
//! # What these tests deliberately do NOT compare, and why that is stated here
//!
//! They compare transcript **growth**, not transcript **shape**. That exemption is
//! load-bearing and was measured rather than assumed: for one assistant turn the
//! release writes four parts — `text`, `step-start`, `text`, `step-finish` — while
//! this port writes two, `text` and `text`. Reproduced at release 1.18.15 on the
//! `run` path, in a git repository and outside one, so it is not the "`step-start`
//! only carries a snapshot" case; production data has 218,899 `step-start` rows
//! whose blob is literally `{"type":"step-start"}`.
//!
//! That difference does not break interoperability — every assertion below holds
//! across it, in both directions — so it is out of scope for a test todo and is
//! **not** silently normalised away here. It is named, with its measurement, so
//! that a reader of a green run knows this file does not claim part-shape parity.
//! An invisible exemption is the thing F4 rejected twice; a visible one is fine.
//!
//! # The skip contract
//!
//! The released binary is a machine-local install. Its absence is announced on
//! stderr and the test returns; a silent skip reporting green is the failure mode
//! this file exists to prevent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use oc_testkit::env::DbChoice;
use oc_testkit::{Oracle, ScriptedEnv, Subject, TestkitError};

/// Restated wherever a test skips, so a reader of the output knows what was wanted.
const NO_ORACLE: &str = "no opencode on PATH and no OC_TESTKIT_ORACLE override";

/// Wall-clock budget for one cassette-free run. Everything it talks to is loopback,
/// so exceeding this is a hang rather than slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// The marker upstream's own title prompt opens with.
///
/// From `packages/opencode/src/session/prompt/title.txt`, observed verbatim in the
/// captured request bodies of both binaries at release 1.18.15. Classifying on the
/// prompt the *client* sends — rather than on a request counter — is what makes
/// these tests independent of how many preludes a run makes.
const TITLE_PROMPT_MARKER: &str = "You are a title generator";

/// The single line the fake provider answers a title request with.
///
/// Upstream trims the completion and rejects an empty result with `title model
/// returned no usable line`, so this must be one non-empty line and nothing else.
const TITLE_REPLY: &str = "Interop lifecycle session";

/// The line the fake provider answers a chat request with.
///
/// Distinct from [`TITLE_REPLY`] so that a test can tell, from the persisted
/// transcript alone, which kind of request produced which row.
const ASSISTANT_REPLY: &str = "acknowledged";

// ---------------------------------------------------------------------------
// A fake provider that tells a title request from a chat request
// ---------------------------------------------------------------------------

/// What kind of completion a captured request asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    /// The session-title prelude: no tools, upstream's title system prompt.
    Title,
    /// A real turn.
    Chat,
}

/// One request, reduced to the two facts these tests assert on.
#[derive(Debug, Clone)]
struct CapturedTurn {
    kind: RequestKind,
    /// The `user` message contents, in order, as the client replayed them.
    ///
    /// This is the decoded transcript leaving the client, which is why it is the
    /// evidence for "the other implementation decoded what this one wrote".
    user_turns: Vec<String>,
}

impl CapturedTurn {
    /// True when some replayed user turn contains `needle`.
    fn replayed(&self, needle: &str) -> bool {
        self.user_turns.iter().any(|turn| turn.contains(needle))
    }
}

/// An OpenAI-compatible fake that answers title and chat requests differently.
///
/// Deliberately local to this test rather than an addition to
/// [`oc_testkit::MockProvider`]: the shared mock's contract is ordered replay of
/// recorded bytes, and teaching it to classify request *semantics* would change
/// behaviour every other consumer depends on. What is needed here is narrow.
struct TranscriptProvider {
    base_url: String,
    captured: Arc<Mutex<Vec<CapturedTurn>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl TranscriptProvider {
    async fn start() -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("read the bound address");
        let router = axum::Router::new()
            .fallback(serve)
            .with_state(Arc::clone(&captured));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            base_url: format!("http://{addr}"),
            captured,
            shutdown: Some(tx),
            join,
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn captured(&self) -> Vec<CapturedTurn> {
        self.captured.lock().expect("captured lock").clone()
    }

    /// Every chat request, in arrival order. Title preludes are excluded because
    /// they carry a synthesised prompt rather than the session's transcript.
    fn chats(&self) -> Vec<CapturedTurn> {
        self.captured()
            .into_iter()
            .filter(|turn| turn.kind == RequestKind::Chat)
            .collect()
    }

    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.join).await;
    }
}

impl Drop for TranscriptProvider {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.join.abort();
    }
}

/// Classify, record, then answer.
///
/// The request is captured before a response is chosen so that a misclassification
/// is still visible to the test that provoked it.
async fn serve(
    State(captured): State<Arc<Mutex<Vec<CapturedTurn>>>>,
    request: Request,
) -> Response {
    let (_, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

    let messages = parsed
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_title = messages.iter().any(|message| {
        message.get("role").and_then(serde_json::Value::as_str) == Some("system")
            && message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains(TITLE_PROMPT_MARKER))
    });
    let user_turns = messages
        .iter()
        .filter(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .map(|message| match message.get("content") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        })
        .collect();

    let kind = if is_title {
        RequestKind::Title
    } else {
        RequestKind::Chat
    };
    captured
        .lock()
        .expect("captured lock")
        .push(CapturedTurn { kind, user_turns });

    let text = match kind {
        RequestKind::Title => TITLE_REPLY,
        RequestKind::Chat => ASSISTANT_REPLY,
    };
    sse_completion(text)
}

/// A minimal OpenAI-compatible streamed completion carrying `text` and stopping.
fn sse_completion(text: &str) -> Response {
    let delta = serde_json::json!({
        "id": "cmpl_interop",
        "object": "chat.completion.chunk",
        "created": 1_780_000_000,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": text },
            "finish_reason": serde_json::Value::Null
        }]
    });
    let stop = serde_json::json!({
        "id": "cmpl_interop",
        "object": "chat.completion.chunk",
        "created": 1_780_000_000,
        "model": "test-model",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    });
    let body = format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n");
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    *response.status_mut() = StatusCode::OK;
    response
}

// ---------------------------------------------------------------------------
// The shared world both binaries run in
// ---------------------------------------------------------------------------

/// A config naming one OpenAI-compatible provider pointed at the fake.
///
/// The endpoint lives only in `options.baseURL`, the shape the upstream docs show;
/// see `oc-cli/tests/tool_turn.rs:90-95` for why a top-level `api` here hid a defect
/// for several waves.
fn provider_config(base_url: &str) -> String {
    serde_json::json!({
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

/// The one world both programs run in.
///
/// A single [`ScriptedEnv`] with one absolute `OPENCODE_DB` is what makes "the same
/// session" structurally true rather than asserted: neither binary is given a way to
/// reach a different database. Two envs sharing a path by convention would let a
/// later edit silently split them, which is exactly the two-session fixture this
/// todo forbids.
struct SharedWorld {
    env: ScriptedEnv,
    database: PathBuf,
}

impl SharedWorld {
    fn new(base_url: &str, name: &str) -> Self {
        let env = ScriptedEnv::new().expect("isolated environment");
        let database = env.xdg_data().join("opencode").join(format!("{name}.db"));
        std::fs::create_dir_all(database.parent().expect("database has a parent"))
            .expect("create the shared data directory");
        let env = env
            .with_db(DbChoice::Absolute(database.clone()))
            .set("NO_COLOR", "1")
            .set("TERM", "dumb")
            .set("OPENCODE_PURE", "1")
            .set("OPENCODE_AUTH_CONTENT", "{}")
            .set("OPENCODE_DISABLE_MODELS_FETCH", "true")
            .set("OPENCODE_CONFIG_CONTENT", provider_config(base_url));
        Self { env, database }
    }

    fn env(&self) -> &ScriptedEnv {
        &self.env
    }

    fn database(&self) -> &Path {
        &self.database
    }

    /// The persisted shape of one session: how many messages and parts it owns.
    ///
    /// Read straight out of SQLite rather than out of either binary's `export`, so
    /// that "the transcript grew" is a fact about storage and cannot be satisfied by
    /// a formatter change on either side.
    fn transcript_size(&self, session_id: &str) -> TranscriptSize {
        let connection = oc_db::Connection::open(&self.database).expect("open the shared database");
        let messages: i64 = connection
            .query_row(
                "SELECT count(*) FROM message WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .expect("count the session's messages");
        let parts: i64 = connection
            .query_row(
                "SELECT count(*) FROM part WHERE message_id IN \
                 (SELECT id FROM message WHERE session_id = ?1)",
                [session_id],
                |row| row.get(0),
            )
            .expect("count the session's parts");
        TranscriptSize { messages, parts }
    }

    /// Every session id in the shared database, in insertion order.
    fn session_ids(&self) -> Vec<String> {
        let connection = oc_db::Connection::open(&self.database).expect("open the shared database");
        let mut statement = connection
            .prepare("SELECT id FROM session ORDER BY rowid")
            .expect("prepare the session query");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query the sessions")
            .collect::<Result<Vec<_>, _>>()
            .expect("read the sessions")
    }

    /// The stored `session.model` JSON, for failure messages.
    fn stored_model(&self, session_id: &str) -> Option<String> {
        let connection = oc_db::Connection::open(&self.database).expect("open the shared database");
        connection
            .query_row(
                "SELECT model FROM session WHERE id = ?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("read the session model")
    }
}

/// How big a transcript is, in the two units storage keeps it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptSize {
    messages: i64,
    parts: i64,
}

impl TranscriptSize {
    /// True when both counts are strictly larger than `earlier`'s.
    ///
    /// Strictly, on both units, on purpose. A run that appended a message row but no
    /// part rows persisted a turn with no content, and "readable" would still hold.
    fn grew_from(self, earlier: Self) -> bool {
        self.messages > earlier.messages && self.parts > earlier.parts
    }
}

/// One implementation, resolved and ready to run against the shared world.
struct Runner {
    label: &'static str,
    program: PathBuf,
    /// Any interpreter prefix. Empty for both an installed release and this port.
    prefix: Vec<String>,
}

impl Runner {
    /// The released TypeScript binary, pinned.
    ///
    /// Resolved through [`Oracle::discover_pinned`] rather than a hard-coded mise
    /// path: todo 130 removed exactly that hard-coding from `compat_suite.rs`, where
    /// the file named 1.18.12 while recording 1.18.13 and nothing could fail over the
    /// difference.
    fn oracle() -> Option<Self> {
        match Oracle::discover_pinned() {
            Ok(oracle) => Some(Self {
                label: "oracle",
                program: oracle.program().to_path_buf(),
                prefix: Vec::new(),
            }),
            Err(TestkitError::BinaryNotFound { .. }) => None,
            Err(mismatch) => panic!("{mismatch}"),
        }
    }

    /// This port's production binary.
    fn subject() -> Self {
        let subject = Subject::discover_or_build().expect("build the subject binary");
        Self {
            label: "subject",
            program: subject.program().to_path_buf(),
            prefix: Vec::new(),
        }
    }

    /// Run `args` in the shared world, bounded.
    ///
    /// `tokio::process` rather than `std::process` because the fake provider runs on
    /// this test's runtime: a synchronous wait would stop driving it and the run
    /// would hang rather than fail.
    async fn run(&self, world: &SharedWorld, args: &[&str]) -> Output {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.prefix)
            .args(args)
            .current_dir(world.env().working_dir())
            .env_clear()
            .envs(world.env().env_vars())
            .kill_on_drop(true);
        tokio::time::timeout(RUN_TIMEOUT, command.output())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{} {args:?} did not finish inside {RUN_TIMEOUT:?}",
                    self.label
                )
            })
            .unwrap_or_else(|error| panic!("launch {} {args:?}: {error}", self.label))
    }

    /// Run `args` and require exit zero, reporting both streams when it is not.
    async fn expect_ok(&self, world: &SharedWorld, args: &[&str], why: &str) -> String {
        let output = self.run(world, args).await;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "{} could not {why} — `{args:?}` exited {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            self.label,
            output.status
        );
        stdout
    }
}

/// The env-var names the shared world adds on top of [`ScriptedEnv`]'s isolation.
///
/// Recorded so a failure message can state what steered a run.
fn steering(world: &SharedWorld) -> BTreeMap<String, String> {
    world
        .env()
        .env_vars()
        .into_iter()
        .filter(|(key, _)| key.starts_with("OPENCODE_") || key.starts_with("XDG_"))
        .collect()
}

// ---------------------------------------------------------------------------
// Direction 1: the release starts the session, this port carries it on
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_the_oracle_created_survives_a_full_lifecycle_in_this_port() {
    let Some(oracle) = Runner::oracle() else {
        eprintln!(
            "SKIPPED a_session_the_oracle_created_survives_a_full_lifecycle_in_this_port: \
             {NO_ORACLE}; the TS→Rust session lifecycle was NOT tested"
        );
        return;
    };
    let subject = Runner::subject();
    let provider = TranscriptProvider::start().await;
    let world = SharedWorld::new(provider.base_url(), "ts-to-rust");

    // 1. The release creates the session and writes the first turn.
    const FIRST: &str = "first turn written by the release";
    oracle
        .expect_ok(
            &world,
            &["run", "--model", "test/test-model", FIRST],
            "start a session",
        )
        .await;

    let sessions = world.session_ids();
    assert_eq!(
        sessions.len(),
        1,
        "the release's run must leave exactly one session to hand over: {sessions:?}"
    );
    let session = sessions[0].clone();
    let after_oracle = world.transcript_size(&session);
    assert!(
        after_oracle.messages >= 2,
        "the release's first turn must persist a user and an assistant message, got \
         {after_oracle:?}"
    );

    // 2. This port LISTS it.
    let listed = subject
        .expect_ok(
            &world,
            &["session", "list", "--format", "json"],
            "list the release's session",
        )
        .await;
    assert!(
        listed.contains(&session),
        "this port listed no session the release created. session={session} \
         model={:?}\nstdout:\n{listed}",
        world.stored_model(&session)
    );

    // 3. This port OPENS it — `export` is the read-the-whole-transcript verb, and it
    //    must show the release's turn, not merely the row's existence.
    let opened = subject
        .expect_ok(
            &world,
            &["export", &session],
            "export the release's session",
        )
        .await;
    assert!(
        opened.contains(FIRST),
        "this port exported the release's session without the release's user turn — \
         the row was listable but its transcript was not decoded\nstdout:\n{opened}"
    );

    // 4. This port CONTINUES it.
    const SECOND: &str = "second turn written by this port";
    subject
        .expect_ok(
            &world,
            &[
                "run",
                "--session",
                &session,
                "--model",
                "test/test-model",
                SECOND,
            ],
            "continue the release's session",
        )
        .await;

    // 5. It is still ONE session, and the transcript GREW.
    let sessions = world.session_ids();
    assert_eq!(
        sessions,
        vec![session.clone()],
        "continuing must extend the release's session, not fork a second one — a \
         two-session outcome is the fixture this todo forbids"
    );
    let after_subject = world.transcript_size(&session);
    assert!(
        after_subject.grew_from(after_oracle),
        "the transcript did not grow when this port continued the release's session: \
         {after_oracle:?} → {after_subject:?}. Readable is not the same as writable."
    );

    // 6. This port's outbound request replayed the release's turn — the release's
    //    rows did not merely parse, they came back out of the decoder.
    let chats = provider.chats();
    let continued = chats
        .last()
        .expect("continuing the session must reach the provider");
    assert!(
        continued.replayed(FIRST),
        "this port continued the session without replaying the release's user turn, \
         so it did not decode the history it inherited. replayed: {:?}",
        continued.user_turns
    );

    // 7. The release READS BACK what this port appended. Same session, other way.
    let relisted = oracle
        .expect_ok(
            &world,
            &["session", "list", "--format", "json"],
            "re-list after this port wrote",
        )
        .await;
    assert!(
        relisted.contains(&session),
        "the release stopped listing the session after this port continued it. \
         model={:?}\nstdout:\n{relisted}",
        world.stored_model(&session)
    );
    let reopened = oracle
        .expect_ok(
            &world,
            &["export", &session],
            "export after this port wrote",
        )
        .await;
    for turn in [FIRST, SECOND] {
        assert!(
            reopened.contains(turn),
            "the release's export of the shared session is missing {turn:?} — both \
             binaries must see the full history. steering={:?}\nstdout:\n{reopened}",
            steering(&world)
        );
    }

    eprintln!(
        "TS→Rust lifecycle: session={session} transcript {:?} → {:?}, \
         provider chats={}",
        after_oracle,
        after_subject,
        chats.len()
    );
    provider.shutdown().await;
}

// ---------------------------------------------------------------------------
// Direction 2: this port starts the session, the release carries it on
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_this_port_created_survives_a_full_lifecycle_in_the_oracle() {
    let Some(oracle) = Runner::oracle() else {
        eprintln!(
            "SKIPPED a_session_this_port_created_survives_a_full_lifecycle_in_the_oracle: \
             {NO_ORACLE}; the Rust→TS session lifecycle was NOT tested"
        );
        return;
    };
    let subject = Runner::subject();
    let provider = TranscriptProvider::start().await;
    let world = SharedWorld::new(provider.base_url(), "rust-to-ts");

    // 1. This port creates the session and writes the first turn.
    const FIRST: &str = "first turn written by this port";
    subject
        .expect_ok(
            &world,
            &["run", "--model", "test/test-model", FIRST],
            "start a session",
        )
        .await;

    let sessions = world.session_ids();
    assert_eq!(
        sessions.len(),
        1,
        "this port's run must leave exactly one session to hand over: {sessions:?}"
    );
    let session = sessions[0].clone();
    let after_subject = world.transcript_size(&session);
    assert!(
        after_subject.messages >= 2,
        "this port's first turn must persist a user and an assistant message, got \
         {after_subject:?}"
    );

    // 2. The release LISTS it. This is todo 115's `session.model` seam: a row spelled
    //    `modelID` takes the *whole* listing down with `Expected string, got
    //    undefined`, so exit zero here is load-bearing.
    let listed = oracle
        .expect_ok(
            &world,
            &["session", "list", "--format", "json"],
            "list this port's session",
        )
        .await;
    assert!(
        listed.contains(&session),
        "the release exited zero but listed no session this port created — a \
         made-up project id silently empties this listing. session={session} \
         model={:?}\nstdout:\n{listed}",
        world.stored_model(&session)
    );

    // 3. The release OPENS it, and must see this port's turn.
    let opened = oracle
        .expect_ok(&world, &["export", &session], "export this port's session")
        .await;
    assert!(
        opened.contains(FIRST),
        "the release exported this port's session without this port's user turn\n\
         stdout:\n{opened}"
    );

    // 4. The release CONTINUES it.
    const SECOND: &str = "second turn written by the release";
    oracle
        .expect_ok(
            &world,
            &[
                "run",
                "--session",
                &session,
                "--model",
                "test/test-model",
                SECOND,
            ],
            "continue this port's session",
        )
        .await;

    // 5. Still ONE session, transcript GREW.
    let sessions = world.session_ids();
    assert_eq!(
        sessions,
        vec![session.clone()],
        "the release must extend this port's session, not fork a second one"
    );
    let after_oracle = world.transcript_size(&session);
    assert!(
        after_oracle.grew_from(after_subject),
        "the transcript did not grow when the release continued this port's session: \
         {after_subject:?} → {after_oracle:?}"
    );

    // 6. The release's outbound request replayed this port's turn.
    let chats = provider.chats();
    let continued = chats
        .last()
        .expect("continuing the session must reach the provider");
    assert!(
        continued.replayed(FIRST),
        "the release continued the session without replaying this port's user turn, \
         so it did not decode the history this port wrote. replayed: {:?}",
        continued.user_turns
    );

    // 7. This port READS BACK what the release appended.
    let relisted = subject
        .expect_ok(
            &world,
            &["session", "list", "--format", "json"],
            "re-list after the release wrote",
        )
        .await;
    assert!(
        relisted.contains(&session),
        "this port stopped listing the session after the release continued it\n\
         stdout:\n{relisted}"
    );
    let reopened = subject
        .expect_ok(
            &world,
            &["export", &session],
            "export after the release wrote",
        )
        .await;
    for turn in [FIRST, SECOND] {
        assert!(
            reopened.contains(turn),
            "this port's export of the shared session is missing {turn:?}\n\
             stdout:\n{reopened}"
        );
    }

    eprintln!(
        "Rust→TS lifecycle: session={session} transcript {:?} → {:?}, \
         provider chats={}",
        after_subject,
        after_oracle,
        chats.len()
    );
    provider.shutdown().await;
}

// ---------------------------------------------------------------------------
// The alternating decode proof, and the failure that keeps it honest
// ---------------------------------------------------------------------------

/// Three turns, alternating binaries, one session — and each side's request carries
/// everything written before it.
///
/// The two lifecycle tests above each prove one handover. This proves the property a
/// user actually depends on: history accumulates across *repeated* switching, so
/// neither implementation quietly truncates what it inherited. Assertion 3's
/// `messages` deltas make a silent truncation fail even if both exports still name
/// the newest turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_implementation_replays_every_turn_the_other_one_wrote() {
    let Some(oracle) = Runner::oracle() else {
        eprintln!(
            "SKIPPED each_implementation_replays_every_turn_the_other_one_wrote: \
             {NO_ORACLE}; cross-implementation decoding was NOT tested"
        );
        return;
    };
    let subject = Runner::subject();
    let provider = TranscriptProvider::start().await;
    let world = SharedWorld::new(provider.base_url(), "alternating");

    const TURN_ONE: &str = "alpha turn from the release";
    const TURN_TWO: &str = "bravo turn from this port";
    const TURN_THREE: &str = "charlie turn from the release again";

    oracle
        .expect_ok(
            &world,
            &["run", "--model", "test/test-model", TURN_ONE],
            "start the alternating session",
        )
        .await;
    let session = {
        let sessions = world.session_ids();
        assert_eq!(
            sessions.len(),
            1,
            "one session to alternate over: {sessions:?}"
        );
        sessions[0].clone()
    };
    let sizes = [world.transcript_size(&session)];

    subject
        .expect_ok(
            &world,
            &[
                "run",
                "--session",
                &session,
                "--model",
                "test/test-model",
                TURN_TWO,
            ],
            "take the session over",
        )
        .await;
    let sizes = [sizes[0], world.transcript_size(&session)];

    oracle
        .expect_ok(
            &world,
            &[
                "run",
                "--session",
                &session,
                "--model",
                "test/test-model",
                TURN_THREE,
            ],
            "take the session back",
        )
        .await;
    let sizes = [sizes[0], sizes[1], world.transcript_size(&session)];

    assert_eq!(
        world.session_ids(),
        vec![session.clone()],
        "alternating binaries must never fork the session"
    );
    assert!(
        sizes[1].grew_from(sizes[0]) && sizes[2].grew_from(sizes[1]),
        "the transcript must grow at every handover, got {sizes:?}"
    );

    let chats = provider.chats();
    assert_eq!(
        chats.len(),
        3,
        "three turns must produce three chat requests; got {}. Title preludes are \
         classified separately by design — see TITLE_PROMPT_MARKER.",
        chats.len()
    );
    // Turn 2 ran on this port and must carry the release's turn 1.
    assert!(
        chats[1].replayed(TURN_ONE),
        "this port did not replay the release's first turn: {:?}",
        chats[1].user_turns
    );
    // Turn 3 ran on the release and must carry BOTH earlier turns, including this
    // port's. A release that dropped only the foreign turn would still pass a test
    // that checked the newest one.
    assert!(
        chats[2].replayed(TURN_ONE) && chats[2].replayed(TURN_TWO),
        "the release did not replay the full alternating history: {:?}",
        chats[2].user_turns
    );
    assert!(
        chats[2].user_turns.len() > chats[1].user_turns.len(),
        "the replayed history must be strictly longer at each handover: {:?} then {:?}",
        chats[1].user_turns,
        chats[2].user_turns
    );

    // And both binaries can still print the whole thing.
    for runner in [&oracle, &subject] {
        let exported = runner
            .expect_ok(
                &world,
                &["export", &session],
                "export the alternating session",
            )
            .await;
        for turn in [TURN_ONE, TURN_TWO, TURN_THREE] {
            assert!(
                exported.contains(turn),
                "{}'s export is missing {turn:?} after three alternating turns\n\
                 stdout:\n{exported}",
                runner.label
            );
        }
    }

    eprintln!(
        "alternating lifecycle: session={session} sizes={sizes:?} \
         replayed user turns={:?}",
        chats.iter().map(|c| c.user_turns.len()).collect::<Vec<_>>()
    );
    provider.shutdown().await;
}

/// The failure case: a `session.model` shape only this port can decode must make the
/// release fail, by name.
///
/// This is what stops every assertion above from being vacuous. A suite that proves
/// "the release reads what we write" is worth nothing unless it also demonstrates
/// that it *would* have noticed the defect — and this project's worst defect was
/// exactly this row shape. `session.model` spelled `modelID` reads and writes through
/// this port perfectly; only upstream's decoder, which reads `row.model.id`
/// (`packages/opencode/src/session/session.ts:88-93`), rejects it, with `Expected
/// string, got undefined` and exit 1 for the entire listing.
///
/// The release creates the database here so its own `project` row scopes the listing.
/// A fabricated project id makes `session list` exit 0 with an empty result, which
/// would disarm the test — recorded because a first draft of the sibling test in
/// `compat_suite.rs` did exactly that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_oracle_refuses_a_session_model_shape_only_this_port_could_decode() {
    let Some(oracle) = Runner::oracle() else {
        eprintln!(
            "SKIPPED the_oracle_refuses_a_session_model_shape_only_this_port_could_decode: \
             {NO_ORACLE}; the row-shape guard was NOT tested"
        );
        return;
    };
    let provider = TranscriptProvider::start().await;
    let world = SharedWorld::new(provider.base_url(), "bad-shape");

    // Let the release create the database and resolve its own project.
    oracle
        .expect_ok(
            &world,
            &["session", "list", "--format", "json"],
            "create the database",
        )
        .await;

    let (project_id, worktree) = {
        let connection =
            oc_db::Connection::open(world.database()).expect("open the release's database");
        connection
            .query_row(
                "SELECT id, worktree FROM project ORDER BY rowid LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("the release must have written a project row")
    };

    // The message spelling, in the session column: one field name apart from correct.
    let wrong_shape =
        serde_json::json!({ "providerID": "test", "modelID": "test-model" }).to_string();
    let session_id = "ses_wrongshapewrongshapewrongsha";
    {
        let mut connection =
            oc_db::Connection::open(world.database()).expect("reopen for the bad write");
        let mut input = oc_db::session::SessionCreate::new(
            session_id,
            "wrong-shape",
            &project_id,
            &worktree,
            &worktree,
            "Wrong shape",
            "0.1.0",
        )
        .at(1_780_000_000_000);
        input.agent = Some("build".to_owned());
        input.model = Some(wrong_shape.clone());
        let transaction = connection.transaction().expect("begin");
        oc_db::session::create(&transaction, &input).expect("write the mis-spelled session");
        transaction.commit().expect("commit");
    }

    let listed = oracle
        .run(&world, &["session", "list", "--format", "json"])
        .await;
    let stderr = String::from_utf8_lossy(&listed.stderr);
    eprintln!(
        "row-shape guard: release exited {} for model={wrong_shape}\n  stderr={}",
        listed.status,
        stderr.trim()
    );
    assert!(
        !listed.status.success(),
        "the release accepted a `session.model` spelled {wrong_shape} — this suite \
         can therefore no longer detect the class of defect it exists for. Either \
         upstream's decoder changed (re-derive the shape from \
         session.ts:88-93 and update `oc_db::session::model_reference`), or this \
         test is no longer reaching the decoder.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&listed.stdout)
    );

    // And the correct spelling, written through the production helper, is accepted —
    // so the assertion above is about the shape and not about the row's presence.
    {
        let connection = oc_db::Connection::open(world.database()).expect("reopen for the fix");
        connection
            .execute(
                "UPDATE session SET model = ?1 WHERE id = ?2",
                (
                    oc_db::session::model_reference("test", "test-model"),
                    session_id,
                ),
            )
            .expect("rewrite the model in the session spelling");
    }
    let fixed = oracle
        .expect_ok(
            &world,
            &["session", "list", "--format", "json"],
            "list after the spelling was corrected",
        )
        .await;
    assert!(
        fixed.contains(session_id),
        "the release accepted the corrected database but did not list the row, so the \
         failure above was not caused by the model spelling\nstdout:\n{fixed}"
    );

    provider.shutdown().await;
}
