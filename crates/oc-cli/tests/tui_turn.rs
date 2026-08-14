//! The proof the interactive surface can execute a turn.
//!
//! Todo 105's whole subject is a seam, so this test is deliberately the crudest and
//! most end-to-end thing in the crate: it launches the real binary under a real
//! pseudo-terminal, with the exact argument shape todo 88's frozen harness issues
//! (`crates/oc-testkit/src/perf/workload.rs:272-286`), and reads the bytes the
//! terminal received.
//!
//! # Why a PTY and not the component tree
//!
//! `oc-tui`'s own 378 tests all render off-screen, and they stayed green through two
//! waves in which submitting a prompt started nothing at all. The property that was
//! missing is not visible from inside the crate: it is that the *process*, given a
//! prompt, talks to a provider and paints the answer. Only a real terminal can
//! observe it, because the surface refuses a pipe by design.
//!
//! # Why the provider's own recording
//!
//! The conversation is `openai-chat/drives-a-tool-loop-end-to-end`, replayed byte for
//! byte with no authored responses, so neither the wire format nor the assistant's
//! reply is this repository's opinion. Its recorded call names `get_weather`, a tool
//! this runtime does not have — which is exactly what makes it useful here: the turn
//! must still send a second request carrying the tool result, and that is the half of
//! the loop a screen that merely rendered could not fake.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use oc_testkit::{MockProvider, Scenario, ScriptedEnv};

/// The recorded conversation, chosen because todo 88's harness replays the same one.
const CASSETTE: &str = "openai-chat/drives-a-tool-loop-end-to-end";

/// The tool-free text completion that answers the prelude's title request.
///
/// The harness's own prelude recording (`crates/oc-testkit/src/perf/workload.rs:93`).
/// Since todo 106 a new session generates its title before its first real turn, so the
/// interactive surface opens with the same single tool-free request the harness counts
/// as `PRELUDE_REQUESTS`.
const TITLE_CASSETTE: &str = "openai-chat/streams-text";

/// One prelude request plus the two a tool turn takes, which is what the frozen
/// `completed_tool_turns(captured) = (captured - 1) / 2` scores as exactly one turn.
const REQUESTS_FOR_ONE_TOOL_TURN: usize = 3;

/// The prompt the frozen harness submits, verbatim.
const PROMPT: &str = "Use get_weather for Paris.";

/// The model the frozen harness names, verbatim.
const MODEL: &str = "test/test-model";

/// A fragment of the recorded assistant reply.
///
/// A fragment and not the sentence: the transcript wraps to the terminal's width, so
/// asserting the whole sentence would fail for a layout reason rather than a wiring
/// one. `sunny` appears in the recording and nowhere else in a frame.
const REPLY_FRAGMENT: &str = "sunny";

const SERVER_PLUGIN: &str = r#"
import { writeFileSync } from "node:fs";

export default {
  id: "production-server-kind-fixture",
  server: async (_input, options) => {
    writeFileSync(options.marker, "server");
    return {};
  },
};
"#;

const TUI_PLUGIN: &str = r#"
import { writeFileSync } from "node:fs";

export default {
  id: "production-tui-kind-fixture",
  tui: async (_input, options) => {
    writeFileSync(options.marker, "tui");
    return {};
  },
};
"#;

/// Wall-clock budget. Everything the run talks to is loopback or local disk.
const BUDGET: Duration = Duration::from_secs(60);

const PLUGIN_STARTUP_BUDGET: Duration = Duration::from_secs(10);

/// Viewport rows, wide enough that the transcript is not the tightest constraint.
const VIEWPORT_ROWS: u16 = 40;

/// Viewport columns. See [`REPLY_FRAGMENT`] for why the width still matters.
const VIEWPORT_COLUMNS: u16 = 120;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-rust"))
}

/// A config naming one OpenAI-compatible provider pointed at the mock.
///
/// `permission` is unset on purpose, exactly as in `tool_turn.rs`: the turn must be
/// governed by the real ruleset, so an `"*": "allow"` override here would hide a
/// regression in which the rules never reach the dispatcher.
///
/// A top-level `api` is unset for the same reason, also exactly as in `tool_turn.rs`:
/// the endpoint lives only in `options.baseURL`, the shape the upstream docs show. The
/// key that used to be here was the same URL by another name, and it is what hid todo
/// 109 — the binary could not dial a provider configured the documented way.
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

fn plugin_kind_config(
    base_url: &str,
    server_plugin: &Path,
    server_marker: &Path,
    tui_plugin: &Path,
    tui_marker: &Path,
) -> String {
    let mut config: serde_json::Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider config is JSON");
    config["plugin"] = serde_json::json!([
        [
            format!("file:{}", server_plugin.display()),
            { "marker": server_marker }
        ],
        [
            format!("file:{}", tui_plugin.display()),
            { "marker": tui_marker }
        ]
    ]);
    config.to_string()
}

fn variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ("ZUNO_PURE".to_owned(), "1".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        // No `OPENCODE_MODELS_PATH`: the config below fully specifies `test/test-model`,
        // so a catalog is not needed to resolve it. Injecting a fixture here is what hid
        // todo 108 — the binary could not start without one — through five waves.
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        (
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            provider_config(base_url),
        ),
    ]);
    variables
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// How the prompt reaches the surface.
///
/// Both are shapes todo 88's frozen harness issues, and they are different code paths
/// with nothing in common past the editor: `W-idle` and `W-real` pass `--prompt` and
/// never touch the pty's input side, while `W-soak` sends each follow-up turn by
/// writing `Use get_weather for Paris.\r` to the launcher's stdin
/// (`perf/workload.rs:200-214`). A surface that honoured only the flag would leave the
/// soak workload unmeasurable while looking wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Submission {
    /// Supplied on the command line, as `W-idle` and `W-real` do.
    Flag,
    /// Typed into the pseudo-terminal and sent with a carriage return, as `W-soak` does.
    Typed,
}

/// The command line todo 88's frozen harness builds, applied to this binary.
///
/// Reproduced rather than imported because `perf::workload::oracle_command` is
/// private to that module and `perf/**` is a frozen input this task may not touch.
/// The shape is `<program> --pure --prompt <text> --model <id> --auto`.
///
/// The `stty` prefix is this test's own addition and is not part of that shape.
/// `script` copies its window size from its own stdin, which under a test harness is
/// a pipe and therefore has none: the pty is real, `is_terminal` is true, and the
/// viewport is `0x0`, so every frame draws nothing and the reply assertion fails for
/// a reason that has nothing to do with the wiring. Sizing the pty from inside is the
/// only way to fix that, because the size belongs to the terminal `script` made.
fn harness_command(program: &Path, submission: Submission) -> String {
    let mut args = vec![
        shell_quote(&program.to_string_lossy()),
        "--pure".to_owned(),
        "--model".to_owned(),
        MODEL.to_owned(),
        "--auto".to_owned(),
    ];
    if submission == Submission::Flag {
        args.extend(["--prompt".to_owned(), shell_quote(PROMPT)]);
    }
    let oracle = args.join(" ");
    format!("stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {oracle}")
}

fn plugin_kind_command(program: &Path) -> String {
    format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --model {MODEL} --auto",
        shell_quote(&program.to_string_lossy())
    )
}

fn run_plugin_kind_startup(
    env: &ScriptedEnv,
    base_url: &str,
    server_plugin: &Path,
    server_marker: &Path,
    tui_plugin: &Path,
    tui_marker: &Path,
) -> Result<(), std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let mut variables = variables(env, base_url);
    variables.remove("ZUNO_PURE");
    variables.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        plugin_kind_config(
            base_url,
            server_plugin,
            server_marker,
            tui_plugin,
            tui_marker,
        ),
    );
    let mut child = Command::new(&script)
        .args([
            "-qefc".to_owned(),
            plugin_kind_command(&binary()),
            "/dev/null".to_owned(),
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let started = Instant::now();
    while started.elapsed() < PLUGIN_STARTUP_BUDGET {
        if server_marker.is_file() && tui_marker.is_file() {
            break;
        }
        if child.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// Everything the pseudo-terminal received, up to the point `wanted` appeared.
struct Transcript {
    text: String,
    saw_wanted: bool,
}

impl Transcript {
    /// The final screen with its escape sequences stripped, for a readable record.
    ///
    /// Cut at the last cursor-home because only the newest paint is the finished
    /// screen; printing the whole stream would bury the answer under every
    /// intermediate frame, and leaving the CSI sequences in makes it unreadable.
    fn last_frame(&self) -> String {
        let raw = self
            .text
            .rsplit_once("\u{1b}[H")
            .map_or(self.text.as_str(), |(_, tail)| tail);
        let mut plain = String::with_capacity(raw.len());
        let mut characters = raw.chars().peekable();
        while let Some(character) = characters.next() {
            if character != '\u{1b}' {
                plain.push(character);
                continue;
            }
            if characters.peek() == Some(&'[') {
                characters.next();
                while characters
                    .peek()
                    .is_some_and(|next| !next.is_ascii_alphabetic())
                {
                    characters.next();
                }
            }
            characters.next();
        }
        plain
    }
}

/// Run the binary under `script`, reading until `wanted` appears or the budget ends.
///
/// `script -qefc` is how the frozen harness obtains a real PTY, so using anything
/// else here would test a different topology from the one the baseline measures.
/// The reader runs on its own thread because the pty read is blocking and the only
/// way to bound it is to stop waiting on it.
fn run_under_pty(
    env: &ScriptedEnv,
    base_url: &str,
    wanted: &str,
    submission: Submission,
) -> Result<Transcript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let mut child = Command::new(&script)
        .args([
            "-qefc".to_owned(),
            harness_command(&binary(), submission),
            "/dev/null".to_owned(),
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, base_url))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut output = child.stdout.take().ok_or_else(|| {
        std::io::Error::other("the launcher's stdout was not piped, so nothing can be read")
    })?;
    let (chunks, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match output.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if chunks.send(buffer[..read].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let started = Instant::now();
    let mut text = String::new();
    let mut saw_wanted = false;
    let mut typed = submission == Submission::Flag;
    while started.elapsed() < BUDGET {
        match received.recv_timeout(Duration::from_millis(250)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if text.contains(wanted) {
            saw_wanted = true;
            break;
        }
        // Typing only once the first frame has arrived, because keystrokes sent
        // before the surface has read the terminal are consumed by whatever owned
        // it — which looks exactly like a prompt the editor ignored.
        if !typed && !text.is_empty() {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                std::io::Error::other("the launcher's stdin was not piped, so nothing can be typed")
            })?;
            stdin.write_all(format!("{PROMPT}\r").as_bytes())?;
            stdin.flush()?;
            typed = true;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(Transcript { text, saw_wanted })
}

/// Drive one turn through `submission` and assert on everything it must produce.
///
/// Shared verbatim by both submission paths, because the property under test is not
/// how the prompt arrives — it is that whichever way it arrives, the same turn runs.
/// Two copies of these assertions would be free to drift into testing two different
/// things while still both passing.
async fn one_turn_through(submission: Submission) {
    let env = ScriptedEnv::new().expect("isolated environment");
    let scenario = Scenario::new("recorded-tool-loop")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded text completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded tool loop loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    assert!(
        provider.authored_scenarios().is_empty(),
        "this test must replay recorded provider bytes only"
    );

    let base_url = provider.base_url().to_owned();
    let transcript = tokio::task::spawn_blocking(move || {
        let outcome = run_under_pty(&env, &base_url, REPLY_FRAGMENT, submission);
        // `env` is dropped here, with the run already finished, so the temporary tree
        // outlives every process that reads it.
        outcome
    })
    .await
    .expect("the pty reader task")
    .expect("the pty launcher runs");
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !captured.is_empty(),
        "the TUI submitted a prompt via {submission:?} and no provider request was \
         captured; the prompt never reached a turn\ntranscript:\n{}",
        transcript.text
    );
    let prelude = captured[0].json().expect("the prelude request is JSON");
    assert!(
        prelude
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|tools| tools.is_empty()),
        "the interactive surface's prelude offered tools, but the title agent denies \
         every one of them:\n{prelude:#}"
    );
    let first = captured[1].json().expect("the first turn request is JSON");
    assert!(
        first
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tools| !tools.is_empty()),
        "the request carried no tools, so the interactive surface built a \
         dispatcher the headless one would not have:\n{first:#}"
    );
    assert_eq!(
        captured.len(),
        REQUESTS_FOR_ONE_TOOL_TURN,
        "the interactive surface must produce the same one-prelude-plus-two shape the \
         headless one does, so the frozen perf gate scores its turn as completed"
    );
    assert!(
        transcript.saw_wanted,
        "the assistant's reply never reached the screen; the turn ran but the \
         events did not\ntranscript:\n{}",
        transcript.text
    );
    eprintln!(
        "HAPPY_QA submission={submission:?} pty_requests={} last_frame:\n{}",
        captured.len(),
        transcript.last_frame()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_submitted_prompt_drives_a_provider_request_and_renders_the_reply() {
    one_turn_through(Submission::Flag).await;
}

/// The same turn, submitted the way todo 88's soak workload submits every one of its.
///
/// `W-soak` never passes `--prompt`; it types into the pty. That reaches the surface
/// through the terminal reader, the key dispatcher and the editor — none of which the
/// flag path touches — so a green flag path says nothing about whether the soak
/// workload can be measured at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_typed_into_the_pty_drives_the_same_turn_as_the_flag() {
    one_turn_through(Submission::Typed).await;
}

#[test]
fn interactive_tui_selects_tui_plugin_factory_from_production_config() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let server_plugin = env.project().join("server-kind-plugin.mjs");
    let tui_plugin = env.project().join("tui-kind-plugin.mjs");
    let server_marker = env.project().join("server-kind.marker");
    let tui_marker = env.project().join("tui-kind.marker");
    std::fs::write(&server_plugin, SERVER_PLUGIN).expect("write server-kind plugin");
    std::fs::write(&tui_plugin, TUI_PLUGIN).expect("write TUI-kind plugin");

    run_plugin_kind_startup(
        &env,
        "http://127.0.0.1:9",
        &server_plugin,
        &server_marker,
        &tui_plugin,
        &tui_marker,
    )
    .expect("launch the production TUI under a PTY");

    assert_eq!(
        std::fs::read_to_string(&server_marker).unwrap_or_default(),
        "server",
        "the ordinary turn runtime must still select server()"
    );
    assert_eq!(
        std::fs::read_to_string(&tui_marker).unwrap_or_default(),
        "tui",
        "the interactive production path must additionally select tui(); a public PluginKind::Tui that only tests can construct is not a supported capability"
    );
}
