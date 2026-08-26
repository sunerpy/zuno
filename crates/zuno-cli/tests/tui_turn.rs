//! The proof the interactive surface can execute a turn.
//!
//! Todo 105's whole subject is a seam, so this test is deliberately the crudest and
//! most end-to-end thing in the crate: it launches the real binary under a real
//! pseudo-terminal, with the exact argument shape todo 88's frozen harness issues
//! (`crates/zuno-testkit/src/perf/workload.rs:272-286`), and reads the bytes the
//! terminal received.
//!
//! # Why a PTY and not the component tree
//!
//! `zuno-tui`'s own 378 tests all render off-screen, and they stayed green through two
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
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use zuno_testkit::{DbChoice, MockProvider, Scenario, ScriptedEnv};

/// The recorded conversation, chosen because todo 88's harness replays the same one.
const CASSETTE: &str = "openai-chat/drives-a-tool-loop-end-to-end";

/// The tool-free text completion that answers the prelude's title request.
///
/// The harness's own prelude recording (`crates/zuno-testkit/src/perf/workload.rs:93`).
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

/// Wall-clock budget. Everything the run talks to is loopback or local disk.
const BUDGET: Duration = Duration::from_secs(60);

/// The picker does not talk to a provider; a local first frame and dialog should
/// appear well before the full provider-turn budget.
const PICKER_BUDGET: Duration = Duration::from_secs(15);

/// Viewport rows, wide enough that the transcript is not the tightest constraint.
const VIEWPORT_ROWS: u16 = 40;

/// Viewport columns. See [`REPLY_FRAGMENT`] for why the width still matters.
const VIEWPORT_COLUMNS: u16 = 120;

const PICKER_FIRST_ID: &str = "ses_picker_first";
const PICKER_SECOND_ID: &str = "ses_picker_second";
const PICKER_THIRD_ID: &str = "ses_picker_third";
// Single-span markers matter here: ratatui's diff renderer can move the cursor
// across spaces in a title, so `"First conversation"` is not guaranteed to be
// contiguous in the raw PTY byte stream even though the screen shows it exactly.
const PICKER_FIRST_TITLE: &str = "PickerFirstMarker";
const PICKER_SECOND_TITLE: &str = "PickerSecondMarker";
const PICKER_THIRD_TITLE: &str = "PickerThirdMarker";
const PICKER_RENAMED_TITLE: &str = "PickerRenamedMarker";
const PARALLEL_PARENT_PROMPT: &str = "ParentSurfaceMarker delegate two foreground children.";
const PARALLEL_PARENT_TITLE: &str = "ParallelParentMarker";
const FIRST_CHILD_DESCRIPTION: &str = "inspect current tree";
const SECOND_CHILD_DESCRIPTION: &str = "inspect temp tree";
const FIRST_CHILD_PROMPT: &str = "FirstChildProviderMarker inspect the current directory.";
const SECOND_CHILD_PROMPT: &str = "SecondChildProviderMarker inspect the temporary directory.";
const CHILD_STEER_PROMPT: &str = "ChildSteerPromptMarker inspect the changed priority.";
const CHILD_STEER_RESPONSE: &str = "ChildSteerReplyMarker";
const CHILD_RESPONSE_DELAY: Duration = Duration::from_secs(10);
const PARALLEL_BUDGET: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct FlagResponder {
    seen: Arc<AtomicBool>,
    response: ResponseTemplate,
}

impl Respond for FlagResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.seen.store(true, Ordering::Release);
        self.response.clone()
    }
}

#[derive(Debug, Default)]
struct ParallelProviderState {
    child_requests: AtomicUsize,
    child_steers: AtomicUsize,
    first_child_request_at: Mutex<Option<Instant>>,
}

impl ParallelProviderState {
    fn record_child_request(&self) {
        self.child_requests.fetch_add(1, Ordering::AcqRel);
        let mut first = self
            .first_child_request_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        first.get_or_insert_with(Instant::now);
    }

    fn children_started(&self) -> usize {
        self.child_requests.load(Ordering::Acquire)
    }

    fn record_child_steer(&self) {
        self.child_steers.fetch_add(1, Ordering::AcqRel);
    }

    fn child_steers(&self) -> usize {
        self.child_steers.load(Ordering::Acquire)
    }

    fn children_cannot_have_completed(&self) -> bool {
        self.first_child_request_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|started| started.elapsed() < CHILD_RESPONSE_DELAY)
    }
}

#[derive(Clone)]
struct ParallelDelegationResponder {
    state: Arc<ParallelProviderState>,
}

impl Respond for ParallelDelegationResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("provider request is JSON");
        let messages = body
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let serialized_messages = serde_json::Value::Array(messages.clone()).to_string();
        let has_tools = body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tools| !tools.is_empty());

        if !has_tools {
            return compatible_text_response(PARALLEL_PARENT_TITLE);
        }
        if serialized_messages.contains(PARALLEL_PARENT_PROMPT) {
            let has_tool_result = messages.iter().any(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
            });
            return if has_tool_result {
                compatible_text_response("parent completed")
            } else {
                compatible_parallel_task_response()
            };
        }
        if serialized_messages.contains(CHILD_STEER_PROMPT) {
            self.state.record_child_steer();
            return compatible_text_response(CHILD_STEER_RESPONSE);
        }
        if serialized_messages.contains(FIRST_CHILD_PROMPT) {
            self.state.record_child_request();
            return compatible_text_response("first child completed")
                .set_delay(CHILD_RESPONSE_DELAY);
        }
        if serialized_messages.contains(SECOND_CHILD_PROMPT) {
            self.state.record_child_request();
            return compatible_text_response("second child completed")
                .set_delay(CHILD_RESPONSE_DELAY);
        }

        ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "error": {
                "message": "unexpected deterministic PTY fixture request",
                "request": body,
            }
        }))
    }
}

fn compatible_text_response(text: &str) -> ResponseTemplate {
    let chunk = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": text},
            "finish_reason": null
        }]
    });
    let finish = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

fn compatible_parallel_task_response() -> ResponseTemplate {
    let first_arguments = serde_json::json!({
        "description": FIRST_CHILD_DESCRIPTION,
        "prompt": FIRST_CHILD_PROMPT,
        "subagent_type": "explorer",
    })
    .to_string();
    let second_arguments = serde_json::json!({
        "description": SECOND_CHILD_DESCRIPTION,
        "prompt": SECOND_CHILD_PROMPT,
        "subagent_type": "explorer",
    })
    .to_string();
    let chunk = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_first_child",
                        "function": {
                            "name": "task",
                            "arguments": first_arguments,
                        }
                    },
                    {
                        "index": 1,
                        "id": "call_second_child",
                        "function": {
                            "name": "task",
                            "arguments": second_arguments,
                        }
                    }
                ]
            },
            "finish_reason": null
        }]
    });
    let finish = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
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
    provider_config_with_mcp(base_url, None)
}

fn provider_config_with_mcp(base_url: &str, mcp_url: Option<&str>) -> String {
    let mut config = serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "transport": "openai-compatible",
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
    });
    if let Some(mcp_url) = mcp_url {
        config["mcp"] = serde_json::json!({
            "lifecycle-fixture": {
                "type": "remote",
                "url": mcp_url,
                "oauth": false
            }
        });
    }
    config.to_string()
}

fn variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        // No `ZUNO_MODELS_PATH`: the config below fully specifies `test/test-model`,
        // so a catalog is not needed to resolve it. Injecting a fixture here is what hid
        // todo 108 — the binary could not start without one — through five waves.
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), provider_config(base_url)),
    ]);
    variables
}

fn parallel_delegation_variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = variables(env, base_url);
    let mut config: serde_json::Value =
        serde_json::from_str(&provider_config(base_url)).expect("provider fixture config is JSON");
    config["permission"] = serde_json::json!({"mode": "allow_all"});
    variables.insert("ZUNO_CONFIG_CONTENT".to_owned(), config.to_string());
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
/// The shape is `<program> --prompt <text> --model <id> --auto`.
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

#[derive(Debug)]
struct ParallelDelegationTranscript {
    text: String,
    saw_two_running_agents: bool,
    entered_child_surface: bool,
    submitted_child_message: bool,
    provider_received_child_message: bool,
    child_reply_visible: bool,
    returned_to_parent: bool,
    all_observed_before_child_completion: bool,
}

fn run_parallel_delegation_under_pty(
    env: &ScriptedEnv,
    base_url: &str,
    provider: Arc<ParallelProviderState>,
) -> Result<ParallelDelegationTranscript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows 48 cols 180; {} --model {MODEL} --auto --prompt {}",
        shell_quote(&binary().to_string_lossy()),
        shell_quote(PARALLEL_PARENT_PROMPT),
    );
    let mut child = Command::new(&script)
        .args(["-qefc".to_owned(), command, "/dev/null".to_owned()])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(parallel_delegation_variables(env, base_url))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("parallel delegation stdout was not piped"))?;
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
    let mut enter_sent = false;
    let mut child_message_sent = false;
    let mut return_sent = false;
    let mut first_description_count = 0;
    let mut second_description_count = 0;
    let mut parent_prompt_count = 0;
    let mut saw_two_running_agents = false;
    let mut entered_child_surface = false;
    let mut provider_received_child_message = false;
    let mut child_reply_visible = false;
    let mut returned_to_parent = false;
    let mut all_observed_before_child_completion = true;

    while started.elapsed() < PARALLEL_BUDGET {
        match received.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if !enter_sent
            && provider.children_started() == 2
            // Ratatui's diff renderer may insert cursor-positioning CSI sequences
            // between the summary's later spans. `2 running` is one span and remains
            // contiguous in the real PTY byte stream; the two provider requests and
            // the two distinct descriptions prove which two rows it summarizes.
            && text.contains("2 running")
            && text.contains(FIRST_CHILD_DESCRIPTION)
            && text.contains(SECOND_CHILD_DESCRIPTION)
        {
            saw_two_running_agents = true;
            all_observed_before_child_completion &= provider.children_cannot_have_completed();
            first_description_count = text.matches(FIRST_CHILD_DESCRIPTION).count();
            second_description_count = text.matches(SECOND_CHILD_DESCRIPTION).count();
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("parallel delegation stdin was not piped"))?;
            stdin.write_all(b"\x18\x1bOB")?;
            stdin.flush()?;
            enter_sent = true;
        } else if enter_sent
            && !child_message_sent
            && text.contains("running · session ")
            && (text.matches(FIRST_CHILD_DESCRIPTION).count() > first_description_count
                || text.matches(SECOND_CHILD_DESCRIPTION).count() > second_description_count)
        {
            entered_child_surface = true;
            all_observed_before_child_completion &= provider.children_cannot_have_completed();
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("parallel delegation stdin was not piped"))?;
            stdin.write_all(format!("{CHILD_STEER_PROMPT}\r").as_bytes())?;
            stdin.flush()?;
            child_message_sent = true;
        } else if child_message_sent
            && !return_sent
            && provider.child_steers() >= 1
            && text.contains(CHILD_STEER_RESPONSE)
        {
            provider_received_child_message = true;
            child_reply_visible = true;
            all_observed_before_child_completion &= provider.children_cannot_have_completed();
            parent_prompt_count = text.matches(PARALLEL_PARENT_PROMPT).count();
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("parallel delegation stdin was not piped"))?;
            stdin.write_all(b"\x18\x1bOA")?;
            stdin.flush()?;
            return_sent = true;
        } else if return_sent && text.matches(PARALLEL_PARENT_PROMPT).count() > parent_prompt_count
        {
            returned_to_parent = true;
            all_observed_before_child_completion &= provider.children_cannot_have_completed();
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(ParallelDelegationTranscript {
        text,
        saw_two_running_agents,
        entered_child_surface,
        submitted_child_message: child_message_sent,
        provider_received_child_message,
        child_reply_visible,
        returned_to_parent,
        all_observed_before_child_completion,
    })
}

/// Open and leave the real welcome screen without submitting model input.
///
/// The process is allowed to initialize its project, configuration and client
/// projections. The assertion below is intentionally against the database after a
/// graceful Ctrl+C exit: none of those read-only startup effects may materialize the
/// prepared session identity.
fn run_empty_welcome_under_pty(env: &ScriptedEnv) -> Result<Transcript, std::io::Error> {
    run_empty_welcome_under_pty_with_mcp(env, None, None)
}

fn run_empty_welcome_under_pty_with_mcp(
    env: &ScriptedEnv,
    mcp_url: Option<&str>,
    exit_ready: Option<&AtomicBool>,
) -> Result<Transcript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --model {MODEL} --auto",
        shell_quote(&binary().to_string_lossy())
    );
    let mut process_environment = variables(env, "http://127.0.0.1:9");
    process_environment.insert(
        "ZUNO_CONFIG_CONTENT".to_owned(),
        provider_config_with_mcp("http://127.0.0.1:9", mcp_url),
    );
    let mut child = Command::new(&script)
        .args(["-qefc".to_owned(), command, "/dev/null".to_owned()])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(process_environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("welcome stdout was not piped"))?;
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
    let mut first_frame = false;
    let mut exit_sent = false;
    let mut graceful_exit = false;
    while started.elapsed() < PICKER_BUDGET {
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                graceful_exit = exit_sent;
                break;
            }
        }
        if !exit_sent
            && text.contains("ask anything, or / for commands")
            && exit_ready.is_none_or(|ready| ready.load(Ordering::Acquire))
        {
            first_frame = true;
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("welcome stdin was not piped"))?;
            stdin.write_all(b"\x03")?;
            stdin.flush()?;
            exit_sent = true;
        }
        if exit_sent && child.try_wait()?.is_some() {
            graceful_exit = true;
            while let Ok(chunk) = received.recv_timeout(Duration::from_millis(50)) {
                text.push_str(&String::from_utf8_lossy(&chunk));
            }
            break;
        }
    }
    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();

    let pool = session_picker_pool(env);
    let store = zuno_db::session::Store::new(&pool);
    let mut query =
        zuno_db::session::ListQuery::directory(env.working_dir().to_string_lossy().into_owned())
            .active_only();
    query.roots = true;
    let sessions = store
        .list(&query)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let saw_wanted = first_frame
        && graceful_exit
        && sessions.is_empty()
        && !text.contains("resume this session:");
    Ok(Transcript { text, saw_wanted })
}

fn seed_session_picker(env: &ScriptedEnv) {
    let pool = session_picker_pool(env);
    let mut connection = pool.open_connection().expect("picker connection");
    zuno_db::migration::apply(&mut connection).expect("picker migrations");
    let directory = env.working_dir().to_string_lossy().into_owned();
    connection
        .execute(
            "INSERT INTO project \
             (id, worktree, vcs, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, 'git', ?3, ?3, '[]')",
            rusqlite::params!["project-picker", directory, 1_787_381_000_000_i64],
        )
        .expect("picker project");
    let transaction = connection.transaction().expect("picker transaction");
    for (id, slug, title, time) in [
        (
            PICKER_FIRST_ID,
            "picker-first",
            PICKER_FIRST_TITLE,
            1_787_381_100_000_i64,
        ),
        (
            PICKER_SECOND_ID,
            "picker-second",
            PICKER_SECOND_TITLE,
            1_787_381_200_000_i64,
        ),
    ] {
        let mut input = zuno_db::session::SessionCreate::new(
            id,
            slug,
            "project-picker",
            &directory,
            &directory,
            title,
            env!("CARGO_PKG_VERSION"),
        )
        .at(time);
        input.agent = Some("build".to_owned());
        input.model = Some(zuno_db::session::model_reference("test", "test-model"));
        zuno_db::session::create(&transaction, &input).expect("picker session");
    }
    transaction.commit().expect("picker sessions commit");
}

fn seed_third_session(env: &ScriptedEnv) {
    let pool = session_picker_pool(env);
    let mut connection = pool.open_connection().expect("picker connection");
    let directory = env.working_dir().to_string_lossy().into_owned();
    let transaction = connection.transaction().expect("picker transaction");
    let mut input = zuno_db::session::SessionCreate::new(
        PICKER_THIRD_ID,
        "picker-third",
        "project-picker",
        &directory,
        &directory,
        PICKER_THIRD_TITLE,
        env!("CARGO_PKG_VERSION"),
    )
    .at(1_787_381_050_000_i64);
    input.agent = Some("build".to_owned());
    input.model = Some(zuno_db::session::model_reference("test", "test-model"));
    zuno_db::session::create(&transaction, &input).expect("third picker session");
    transaction.commit().expect("third picker session commit");
}

fn session_picker_pool(env: &ScriptedEnv) -> zuno_db::Pool {
    let variables = env.env_vars();
    let database = PathBuf::from(
        variables
            .get("ZUNO_DB")
            .expect("the picker fixture uses a file database"),
    );
    zuno_db::Pool::open(&zuno_paths::DbLocation::File(database)).expect("picker database opens")
}

fn active_root_session_count(env: &ScriptedEnv) -> Result<usize, std::io::Error> {
    let pool = session_picker_pool(env);
    let store = zuno_db::session::Store::new(&pool);
    let mut query =
        zuno_db::session::ListQuery::directory(env.working_dir().to_string_lossy().into_owned())
            .active_only();
    query.roots = true;
    store
        .list(&query)
        .map(|sessions| sessions.len())
        .map_err(|error| std::io::Error::other(error.to_string()))
}

/// Run `/new`, prove the remount itself writes nothing, then submit the first prompt.
///
/// This covers the seam component tests cannot: the slash router, session screen,
/// selection channel and CLI remount loop all have to agree before the prompt can
/// materialize exactly one fresh durable session.
fn run_new_session_under_pty(env: &ScriptedEnv) -> Result<Transcript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --model {MODEL} --auto -s {PICKER_SECOND_ID}",
        shell_quote(&binary().to_string_lossy())
    );
    let mut child = Command::new(&script)
        .args(["-qefc".to_owned(), command, "/dev/null".to_owned()])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, "http://127.0.0.1:9"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("new-session stdout was not piped"))?;
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

    let initial_count = active_root_session_count(env)?;
    let started = Instant::now();
    let mut text = String::new();
    let mut command_typed = false;
    let mut command_typed_at = None;
    let mut command_submitted = false;
    let mut remount_requested_at = None;
    let mut count_before_prompt = None;
    let mut prompt_sent = false;
    let mut materialized = false;
    let mut exit_sent_at = None;
    let mut second_exit_sent = false;

    while started.elapsed() < PICKER_BUDGET {
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if prompt_sent && active_root_session_count(env)? == initial_count + 1 {
            materialized = true;
        }

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("new-session stdin was not piped"))?;
        if !command_typed && text.contains("ask anything, or / for commands") {
            stdin.write_all(b"/new\r")?;
            stdin.flush()?;
            command_typed = true;
            command_typed_at = Some(Instant::now());
        } else if command_typed
            && !command_submitted
            && command_typed_at.is_some_and(|sent| sent.elapsed() >= Duration::from_millis(500))
        {
            // The first Enter accepts the autocomplete row; the second submits it.
            stdin.write_all(b"\r")?;
            stdin.flush()?;
            command_submitted = true;
            remount_requested_at = Some(Instant::now());
        } else if command_submitted
            && !prompt_sent
            && remount_requested_at.is_some_and(|sent| sent.elapsed() >= Duration::from_millis(750))
        {
            let count = active_root_session_count(env)?;
            count_before_prompt = Some(count);
            if count != initial_count {
                break;
            }
            stdin.write_all(b"first prompt in a fresh session\r")?;
            stdin.flush()?;
            prompt_sent = true;
        } else if materialized && exit_sent_at.is_none() {
            stdin.write_all(b"\x03")?;
            stdin.flush()?;
            exit_sent_at = Some(Instant::now());
        } else if exit_sent_at.is_some_and(|sent| sent.elapsed() >= Duration::from_millis(300))
            && !second_exit_sent
        {
            // The first Ctrl+C interrupts an in-flight provider request; the second exits.
            stdin.write_all(b"\x03")?;
            stdin.flush()?;
            second_exit_sent = true;
        }

        if second_exit_sent && child.try_wait()?.is_some() {
            while let Ok(chunk) = received.recv_timeout(Duration::from_millis(50)) {
                text.push_str(&String::from_utf8_lossy(&chunk));
            }
            break;
        }
    }

    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let final_count = active_root_session_count(env)?;
    let saw_wanted = count_before_prompt == Some(initial_count)
        && materialized
        && final_count == initial_count + 1;
    Ok(Transcript { text, saw_wanted })
}

fn run_session_picker_under_pty(env: &ScriptedEnv) -> Result<Transcript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --model {MODEL} --auto -s {PICKER_SECOND_ID}",
        shell_quote(&binary().to_string_lossy())
    );
    let mut child = Command::new(&script)
        .args(["-qefc".to_owned(), command, "/dev/null".to_owned()])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, "http://127.0.0.1:9"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("picker stdout was not piped"))?;
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
    let mut command_sent = false;
    let mut submit_sent_at = None;
    let mut saw_wanted = false;
    while started.elapsed() < PICKER_BUDGET {
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if text.contains(PICKER_FIRST_TITLE)
            && text.contains(PICKER_SECOND_TITLE)
            && text.contains("ctrl+r")
            && text.contains("ctrl+d")
            && text.contains("delete twice")
        {
            saw_wanted = true;
            break;
        }
        if !command_sent && text.contains("ask anything, or / for commands") {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
            stdin.write_all(b"/session\r")?;
            stdin.flush()?;
            command_sent = true;
            submit_sent_at = Some(Instant::now());
        } else if submit_sent_at
            .is_some_and(|submitted| submitted.elapsed() >= Duration::from_millis(500))
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
            stdin.write_all(b"\r")?;
            stdin.flush()?;
            submit_sent_at = None;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(Transcript { text, saw_wanted })
}

#[derive(Clone, Copy)]
enum SessionPickerAction {
    SwitchToPrevious,
    RenameCurrent,
}

impl SessionPickerAction {
    const fn expected_session(self) -> &'static str {
        match self {
            Self::SwitchToPrevious => PICKER_FIRST_ID,
            Self::RenameCurrent => PICKER_SECOND_ID,
        }
    }
}

fn run_session_picker_action_under_pty(
    env: &ScriptedEnv,
    action: SessionPickerAction,
) -> Result<Transcript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --model {MODEL} --auto -s {PICKER_SECOND_ID}",
        shell_quote(&binary().to_string_lossy())
    );
    let mut child = Command::new(&script)
        .args(["-qefc".to_owned(), command, "/dev/null".to_owned()])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, "http://127.0.0.1:9"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("picker stdout was not piped"))?;
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

    let pool = session_picker_pool(env);
    let store = zuno_db::session::Store::new(&pool);
    let started = Instant::now();
    let mut text = String::new();
    let mut command_sent = false;
    let mut command_sent_at = None;
    let mut command_submit_sent = false;
    let mut action_sent = false;
    let mut action_sent_at = None;
    let mut second_step_sent = false;
    let mut applied_at = None;
    let mut exit_sent = false;
    let mut saw_wanted = false;
    while started.elapsed() < PICKER_BUDGET {
        match received.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let applied = match action {
            SessionPickerAction::SwitchToPrevious => second_step_sent,
            SessionPickerAction::RenameCurrent => store
                .find(PICKER_SECOND_ID)
                .expect("read renamed session")
                .is_some_and(|session| session.title == PICKER_RENAMED_TITLE),
        };
        if applied {
            applied_at.get_or_insert_with(Instant::now);
        }
        if !exit_sent
            && applied_at.is_some_and(|applied| applied.elapsed() >= Duration::from_secs(1))
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
            stdin.write_all(b"\x03")?;
            stdin.flush()?;
            exit_sent = true;
        }
        let expected_hint = format!("resume this session: zuno -s {}", action.expected_session());
        if exit_sent && text.contains(&expected_hint) {
            let entered = text.matches("\u{1b}[?1049h").count();
            let left = text.matches("\u{1b}[?1049l").count();
            saw_wanted = applied && entered == 1 && left == 1;
            break;
        }
        if !command_sent && text.contains("ask anything, or / for commands") {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
            stdin.write_all(b"/session\r")?;
            stdin.flush()?;
            command_sent = true;
            command_sent_at = Some(Instant::now());
        } else if command_sent
            && !command_submit_sent
            && command_sent_at.is_some_and(|sent| sent.elapsed() >= Duration::from_millis(500))
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
            stdin.write_all(b"\r")?;
            stdin.flush()?;
            command_submit_sent = true;
        } else if command_submit_sent
            && !action_sent
            && text.contains(PICKER_FIRST_TITLE)
            && text.contains(PICKER_SECOND_TITLE)
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
            match action {
                SessionPickerAction::SwitchToPrevious => {
                    stdin.write_all(b"\x1b[B\r")?;
                    second_step_sent = true;
                }
                SessionPickerAction::RenameCurrent => stdin.write_all(b"\x12")?,
            }
            stdin.flush()?;
            action_sent = true;
            action_sent_at = Some(Instant::now());
        } else if action_sent
            && !second_step_sent
            && matches!(action, SessionPickerAction::RenameCurrent)
        {
            let ready = match action {
                SessionPickerAction::SwitchToPrevious => false,
                SessionPickerAction::RenameCurrent => {
                    text.contains("Rename session")
                        || action_sent_at
                            .is_some_and(|sent| sent.elapsed() >= Duration::from_millis(500))
                }
            };
            if ready {
                let stdin = child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
                match action {
                    SessionPickerAction::SwitchToPrevious => {}
                    SessionPickerAction::RenameCurrent => {
                        stdin.write_all(&vec![0x7f; PICKER_SECOND_TITLE.len()])?;
                        stdin.write_all(PICKER_RENAMED_TITLE.as_bytes())?;
                        stdin.write_all(b"\r")?;
                    }
                }
                stdin.flush()?;
                second_step_sent = true;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(Transcript { text, saw_wanted })
}

fn run_consecutive_session_deletes_under_pty(
    env: &ScriptedEnv,
) -> Result<Transcript, std::io::Error> {
    let script = which::which("script").map_err(|_| {
        std::io::Error::other("`script` is required to give the TUI a real PTY; install util-linux")
    })?;
    let command = format!(
        "stty rows {VIEWPORT_ROWS} cols {VIEWPORT_COLUMNS}; {} --model {MODEL} --auto",
        shell_quote(&binary().to_string_lossy())
    );
    let mut child = Command::new(&script)
        .args(["-qefc".to_owned(), command, "/dev/null".to_owned()])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, "http://127.0.0.1:9"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("picker stdout was not piped"))?;
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

    let pool = session_picker_pool(env);
    let store = zuno_db::session::Store::new(&pool);
    let directory = env.working_dir().to_string_lossy().into_owned();
    let started = Instant::now();
    let mut text = String::new();
    let mut command_sent = false;
    let mut command_sent_at = None;
    let mut command_submit_sent = false;
    let mut first_arm_sent = false;
    let mut first_confirm_sent = false;
    let mut first_title_count_before_remount = 0;
    let mut second_arm_sent = false;
    let mut second_confirmation_count = 0;
    let mut second_confirm_sent = false;
    let mut third_title_count_before_remount = 0;
    let mut exit_sent = false;
    let mut saw_wanted = false;

    while started.elapsed() < PICKER_BUDGET {
        let disconnected = match received.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                text.push_str(&String::from_utf8_lossy(&chunk));
                false
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => true,
        };

        let mut query = zuno_db::session::ListQuery::directory(directory.clone()).active_only();
        query.roots = true;
        let remaining = store
            .list(&query)
            .expect("list sessions after consecutive deletion");
        let first_deleted = store
            .find(PICKER_SECOND_ID)
            .expect("read the initially selected session")
            .is_none();
        let both_deleted = first_deleted
            && matches!(
                remaining.as_slice(),
                [third] if third.id == PICKER_THIRD_ID
            );

        if exit_sent && text.contains("\u{1b}[?1049l") {
            let entered = text.matches("\u{1b}[?1049h").count();
            let left = text.matches("\u{1b}[?1049l").count();
            saw_wanted =
                both_deleted && entered == 1 && left == 1 && !text.contains("resume this session:");
            if saw_wanted {
                break;
            }
        }
        if disconnected {
            break;
        }

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("picker stdin was not piped"))?;
        if !command_sent && text.contains("ask anything, or / for commands") {
            stdin.write_all(b"/session\r")?;
            stdin.flush()?;
            command_sent = true;
            command_sent_at = Some(Instant::now());
        } else if command_sent
            && !command_submit_sent
            && command_sent_at.is_some_and(|sent| sent.elapsed() >= Duration::from_millis(500))
        {
            stdin.write_all(b"\r")?;
            stdin.flush()?;
            command_submit_sent = true;
        } else if command_submit_sent
            && !first_arm_sent
            && text.contains(PICKER_FIRST_TITLE)
            && text.contains(PICKER_SECOND_TITLE)
            && text.contains(PICKER_THIRD_TITLE)
        {
            stdin.write_all(b"\x04")?;
            stdin.flush()?;
            first_arm_sent = true;
        } else if first_arm_sent && !first_confirm_sent && text.contains("deletion") {
            first_title_count_before_remount = text.matches(PICKER_FIRST_TITLE).count();
            stdin.write_all(b"\x04")?;
            stdin.flush()?;
            first_confirm_sent = true;
        } else if first_confirm_sent
            && first_deleted
            && !second_arm_sent
            && text.matches(PICKER_FIRST_TITLE).count() > first_title_count_before_remount
        {
            second_confirmation_count = text.matches("deletion").count();
            stdin.write_all(b"\x04")?;
            stdin.flush()?;
            second_arm_sent = true;
        } else if second_arm_sent
            && !second_confirm_sent
            && text.matches("deletion").count() > second_confirmation_count
        {
            third_title_count_before_remount = text.matches(PICKER_THIRD_TITLE).count();
            stdin.write_all(b"\x04")?;
            stdin.flush()?;
            second_confirm_sent = true;
        } else if second_confirm_sent
            && both_deleted
            && !exit_sent
            && text.matches(PICKER_THIRD_TITLE).count() > third_title_count_before_remount
        {
            // The final picker render proves it was reopened after the second deletion.
            stdin.write_all(b"\x03")?;
            stdin.flush()?;
            exit_sent = true;
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
    if zuno_testkit::recordings_root_or_skip(
        "tui_turn::one_turn_through",
        "the recorded TUI turn was NOT tested",
    )
    .is_none()
    {
        return;
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_foreground_children_remain_live_and_navigable_in_the_real_tui() {
    let server = MockServer::start().await;
    let provider = Arc::new(ParallelProviderState::default());
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ParallelDelegationResponder {
            state: Arc::clone(&provider),
        })
        .mount(&server)
        .await;

    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let base_url = server.uri();
    let observed = Arc::clone(&provider);
    let transcript = tokio::task::spawn_blocking(move || {
        run_parallel_delegation_under_pty(&env, &base_url, observed)
    })
    .await
    .expect("parallel delegation PTY task")
    .expect("parallel delegation TUI starts");

    assert_eq!(
        provider.children_started(),
        2,
        "the parent did not dispatch two real child provider turns\ntranscript:\n{}",
        transcript.text
    );
    assert!(
        transcript.saw_two_running_agents,
        "the real TUI did not refresh its Agents sidebar with two running foreground children\n\
         transcript:\n{}",
        transcript.text
    );
    assert!(
        transcript.entered_child_surface,
        "Ctrl+X Down did not replace the parent main pane with a full running child-session \
         surface\ntranscript:\n{}",
        transcript.text
    );
    assert!(
        transcript.submitted_child_message,
        "the running child composer did not accept a direct user message\ntranscript:\n{}",
        transcript.text
    );
    assert!(
        transcript.provider_received_child_message,
        "the direct child message did not interrupt and restart the real child provider request\n\
         transcript:\n{}",
        transcript.text
    );
    assert!(
        transcript.child_reply_visible,
        "the steered child's provider reply did not reach the attached child transcript\n\
         transcript:\n{}",
        transcript.text
    );
    assert!(
        transcript.returned_to_parent,
        "Ctrl+X Up did not return from the child-session surface to the still-running parent\n\
         transcript:\n{}",
        transcript.text
    );
    assert!(
        transcript.all_observed_before_child_completion,
        "one of the asserted UI states appeared only after the delayed child provider response; \
         this would not prove the TUI remains live while foreground children run\ntranscript:\n{}",
        transcript.text
    );
}

#[test]
fn opening_and_leaving_the_welcome_screen_creates_no_session() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);

    let transcript = run_empty_welcome_under_pty(&env)
        .expect("the empty welcome screen starts and exits under a PTY");

    assert!(
        transcript.saw_wanted,
        "opening and leaving the welcome screen either created a durable session, printed an \
         unusable resume hint, or failed to exit cleanly\ntranscript:\n{}",
        transcript.text
    );
}

#[test]
fn slash_new_is_lazy_until_the_first_prompt_and_then_creates_exactly_one_session() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    seed_session_picker(&env);

    let transcript =
        run_new_session_under_pty(&env).expect("the real TUI starts a new session under a PTY");

    assert!(
        transcript.saw_wanted,
        "`/new` either materialized before model input, failed to remount, or created more than \
         one durable session for the first prompt\ntranscript:\n{}",
        transcript.text
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_the_tui_closes_an_initialized_remote_mcp_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            serde_json::json!({"method": "initialize"}),
        ))
        .respond_with(|request: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("initialize body");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("mcp-session-id", "tui-lifecycle-session")
                .set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "lifecycle-fixture", "version": "1.0.0"}
                    }
                }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            serde_json::json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let tools_seen = Arc::new(AtomicBool::new(false));
    let tools_response_seen = Arc::clone(&tools_seen);
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            serde_json::json!({"method": "tools/list"}),
        ))
        .respond_with(move |request: &Request| {
            tools_response_seen.store(true, Ordering::Release);
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("tools/list body");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {"tools": []}
                }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let delete_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("DELETE"))
        .and(path("/mcp"))
        .and(header("mcp-session-id", "tui-lifecycle-session"))
        .respond_with(FlagResponder {
            seen: Arc::clone(&delete_seen),
            response: ResponseTemplate::new(200),
        })
        .expect(1)
        .mount(&server)
        .await;

    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let mcp_url = format!("{}/mcp", server.uri());
    let ready = Arc::clone(&tools_seen);
    let transcript = tokio::task::spawn_blocking(move || {
        run_empty_welcome_under_pty_with_mcp(&env, Some(&mcp_url), Some(&ready))
    })
    .await
    .expect("PTY task")
    .expect("TUI exits");

    server.verify().await;
    assert!(
        transcript.saw_wanted && delete_seen.load(Ordering::Acquire),
        "TUI exit did not complete the remote MCP DELETE shutdown contract\ntranscript:\n{}",
        transcript.text
    );
}

#[test]
fn session_picker_lists_other_active_sessions_from_the_same_directory() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    seed_session_picker(&env);

    let transcript =
        run_session_picker_under_pty(&env).expect("the real TUI session picker runs under a PTY");

    assert!(
        transcript.saw_wanted,
        "`/session` did not show both persisted root sessions plus the rename and two-press \
         delete hints\n\
         transcript:\n{}",
        transcript.text
    );
}

#[test]
fn session_picker_switches_without_leaving_the_terminal_session() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    seed_session_picker(&env);

    let transcript =
        run_session_picker_action_under_pty(&env, SessionPickerAction::SwitchToPrevious)
            .expect("the real TUI switches sessions under a PTY");

    assert!(
        transcript.saw_wanted,
        "selecting another session either did not switch or left and re-entered the alternate \
         screen\ntranscript:\n{}",
        transcript.text
    );
}

#[test]
fn session_picker_renames_the_current_session_through_the_real_tui() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    seed_session_picker(&env);

    let transcript = run_session_picker_action_under_pty(&env, SessionPickerAction::RenameCurrent)
        .expect("the real TUI renames a session under a PTY");

    assert!(
        transcript.saw_wanted,
        "Ctrl+R did not persist the new title without leaving and re-entering the alternate \
         screen\ntranscript:\n{}",
        transcript.text
    );
}

#[test]
fn session_picker_stays_open_for_consecutive_deletes() {
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    seed_session_picker(&env);
    seed_third_session(&env);

    let transcript = run_consecutive_session_deletes_under_pty(&env)
        .expect("the real TUI deletes consecutive sessions under a PTY");

    assert!(
        transcript.saw_wanted,
        "the session picker did not remain available for a second two-press deletion without \
         another `/session` command, or the terminal session was reopened\n\
         transcript:\n{}",
        transcript.text
    );
}
