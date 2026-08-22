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
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
    serde_json::json!({
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
    })
    .to_string()
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
    let mut launched_session_id = None;

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
        if launched_session_id.is_none() {
            launched_session_id = remaining
                .iter()
                .find(|session| {
                    !matches!(
                        session.id.as_str(),
                        PICKER_FIRST_ID | PICKER_SECOND_ID | PICKER_THIRD_ID
                    )
                })
                .map(|session| session.id.clone());
        }
        let first_deleted = launched_session_id.as_deref().is_some_and(|id| {
            store
                .find(id)
                .expect("read the newly launched session")
                .is_none()
        });
        let both_deleted = first_deleted
            && matches!(
                remaining.as_slice(),
                [first, third]
                    if first.id == PICKER_FIRST_ID && third.id == PICKER_THIRD_ID
            );

        if text.contains(&format!("resume this session: zuno -s {PICKER_FIRST_ID}")) {
            let entered = text.matches("\u{1b}[?1049h").count();
            let left = text.matches("\u{1b}[?1049l").count();
            saw_wanted = both_deleted && entered == 1 && left == 1;
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
