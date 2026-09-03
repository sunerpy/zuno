//! One `zuno` invocation is one process, on every platform.
//!
//! A client that spawns `zuno` — an editor over ACP, a service supervising
//! `zuno serve`, a CI harness — holds exactly one handle, and three things have to
//! be true of it: the pid it holds is the pid doing the work, killing it ends the
//! command, and reading its pipes afterwards reaches end of file. Startup used to
//! hand the command process its environment by *spawning* a second process wherever
//! `exec` is unavailable, which broke all three on Windows: `TerminateProcess` ended
//! the waiter the client held, the command process kept running with the inherited
//! pipe write ends, and the client's `wait_with_output` never returned.
//!
//! Unit tests beside the dispatch seam cannot show this: `exec` and
//! `TerminateProcess` semantics exist only in a real process tree. So these tests
//! drive the shipped binary and observe it from outside, the way a client does. Both
//! run on every platform and both pass on Unix before and after the change; on
//! Windows they are the regression tests, and the pid assertion is the one that
//! names the defect directly.
//!
//! The process that initializes logging reports its own pid in its first record, and
//! only a process that reaches dispatch initializes logging. `--print-logs` puts that
//! record on stderr, which makes it the client-visible answer to "which process ran
//! the command" — and, because the flag reaches the logger only through the resolved
//! startup environment, the same record is the evidence that the bootstrap
//! environment still arrives without a second process to carry it.

use std::io::{BufRead as _, BufReader, Read};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Long enough for a cold binary to open its database and bind, short enough that a
/// regression fails the run instead of hanging it.
const READINESS_TIMEOUT: Duration = Duration::from_secs(90);

/// The bound on everything after the kill.
///
/// A surviving command process holds the inherited pipe write ends and the listening
/// socket for as long as it lives, so this bound is what turns the orphan into a
/// failure instead of a hung suite — which is how the defect first showed up.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// The first record the logging process writes about itself.
const PROCESS_STARTED: &str = "event=\"process.started\"";

/// What one reader thread reports about the stream it owns.
enum StreamEvent {
    Line(String),
    /// Every write end of the pipe is closed. This is the observation
    /// `wait_with_output` makes, with a deadline around it.
    Eof,
}

/// A port nothing is listening on, obtained by binding and releasing it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port for the fixture");
    listener
        .local_addr()
        .expect("the bound fixture address")
        .port()
}

/// The isolation every child here runs under: no developer configuration,
/// credentials, sessions or database, and no network fetches during startup.
fn isolated<'a>(command: &'a mut Command, root: &std::path::Path) -> &'a mut Command {
    command
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("ZUNO_DB", root.join("sessions.db"))
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
}

/// The pid the process that ran the command reported for itself.
fn logged_pid(stderr: &str) -> Option<u32> {
    stderr
        .lines()
        .find(|line| line.contains(PROCESS_STARTED))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))?
        .parse()
        .ok()
}

/// Read one stream on its own thread, reporting its lines and then its end of file.
///
/// Both streams need a reader for the whole life of the child: a stream nobody reads
/// blocks the writer once the pipe buffer fills, which would look like the hang these
/// tests exist to catch.
fn collect(stream: impl Read + Send + 'static) -> Receiver<StreamEvent> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if sender.send(StreamEvent::Line(line)).is_err() {
                return;
            }
        }
        let _ = sender.send(StreamEvent::Eof);
    });
    receiver
}

/// Wait for `stream` to reach end of file, keeping whatever it still emits.
fn await_eof(
    stream: &Receiver<StreamEvent>,
    deadline: Instant,
    transcript: &mut Vec<String>,
) -> bool {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        match stream.recv_timeout(remaining) {
            Ok(StreamEvent::Line(line)) => transcript.push(line),
            Ok(StreamEvent::Eof) => return true,
            Err(RecvTimeoutError::Timeout) => return false,
            // The reader thread ended without reporting end of file, which happens
            // only if it was itself torn down; that is no observation either way.
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

#[test]
fn a_dispatching_invocation_runs_in_the_process_the_client_spawned() {
    let root = tempfile::tempdir().expect("private CLI root");

    // Given/When: the cheapest invocation that pays full startup, asked to put its
    // logs on stderr — a global CLI option, so the logger can only see it through
    // the resolved startup environment.
    let child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_zuno")), root.path())
        .args(["--print-logs", "session", "list"])
        .spawn()
        .expect("the shipped binary must start");
    let supervised = child.id();
    let output = child
        .wait_with_output()
        .expect("reading a finished command's pipes reaches end of file");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`session list` exited with {:?}\nstderr:\n{stderr}",
        output.status.code()
    );

    // Then: the bootstrap environment arrived. `--print-logs` becomes
    // `ZUNO_PRINT_LOGS` in the resolved environment and nothing else turns on the
    // stderr sink, so this record is absent unless the resolution reached the
    // process that dispatched.
    assert!(
        stderr.contains(PROCESS_STARTED),
        "`--print-logs` produced no logs, so the resolved startup environment did \
         not reach the process that ran the command\nstderr:\n{stderr}"
    );

    // And: that process is the one the client spawned. This is what the Windows
    // bootstrap broke — the command ran in a second process while the client held
    // its waiter — and it is unobservable from inside a single process.
    assert_eq!(
        logged_pid(&stderr),
        Some(supervised),
        "the command ran under a different pid than the {supervised} the client \
         spawned, so a client's signals and process bookkeeping would name the \
         wrong process\nstderr:\n{stderr}"
    );
}

#[test]
fn a_log_level_from_the_command_line_reaches_the_logger_without_a_restart() {
    let root = tempfile::tempdir().expect("private CLI root");

    // When: the same invocation, with the level raised above the start record.
    let output = isolated(&mut Command::new(env!("CARGO_BIN_EXE_zuno")), root.path())
        .args(["--print-logs", "--log-level", "ERROR", "session", "list"])
        .output()
        .expect("the shipped binary must run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`session list` exited with {:?}\nstderr:\n{stderr}",
        output.status.code()
    );

    // Then: the sink is on and the level applied, which together say the resolved
    // environment reached the logger as a value. A second process is one way to
    // deliver it; the point of this assertion is that it is not the only way.
    assert!(
        !stderr.contains(PROCESS_STARTED),
        "`--log-level ERROR` did not raise the level, so an info record survived \
         it\nstderr:\n{stderr}"
    );
}

#[test]
fn killing_a_spawned_serve_ends_the_command_and_closes_its_pipes() {
    let root = tempfile::tempdir().expect("private serve root");
    let port = free_port();
    std::fs::write(
        root.path().join("zuno.json"),
        format!("{{\n  \"server\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("project configuration");

    // Given: a long-running command a client supervises, started the way a service
    // manager or an editor starts it.
    let mut child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_zuno")), root.path())
        .args(["--print-logs", "serve"])
        .spawn()
        .expect("the shipped binary must start");
    let supervised = child.id();
    let stdout = collect(child.stdout.take().expect("piped stdout"));
    let stderr = collect(child.stderr.take().expect("piped stderr"));

    // One deadline for reaching readiness, not one per line.
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut transcript = Vec::new();
    let mut ready = false;
    while !ready {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match stdout.recv_timeout(remaining) {
            Ok(StreamEvent::Line(line)) => {
                ready = line.contains("listening on");
                transcript.push(line);
            }
            Ok(StreamEvent::Eof) | Err(_) => break,
        }
    }
    assert!(
        ready,
        "`serve` never reported a listening address; stdout: {transcript:?}"
    );

    // When: the client ends the process it spawned, which is all a client has.
    child.kill().expect("the supervised process accepts a kill");
    let status = child.wait().expect("the supervised process is reaped");

    // Then: both pipes reach end of file. A second process holding the inherited
    // write ends is exactly what made a client's `wait_with_output` block forever,
    // and the deadline here is what makes that a failure instead of a hang.
    let closed = Instant::now() + SHUTDOWN_TIMEOUT;
    let mut diagnostics = Vec::new();
    assert!(
        await_eof(&stdout, closed, &mut transcript),
        "stdout never reached end of file after the kill, so something still holds \
         its write end: status {status:?}; stdout: {transcript:?}"
    );
    assert!(
        await_eof(&stderr, closed, &mut diagnostics),
        "stderr never reached end of file after the kill, so something still holds \
         its write end: status {status:?}; stderr: {diagnostics:?}"
    );

    // And: the killed process was the one serving. End of file alone would also be
    // satisfied by a survivor that happened to close its handles, so the released
    // port is the statement that nothing is left listening.
    let released = Instant::now() + SHUTDOWN_TIMEOUT;
    let mut rebind = TcpListener::bind(("127.0.0.1", port));
    while rebind.is_err() && Instant::now() < released {
        std::thread::sleep(Duration::from_millis(50));
        rebind = TcpListener::bind(("127.0.0.1", port));
    }
    assert!(
        rebind.is_ok(),
        "port {port} is still held after the supervised process was killed and \
         reaped, so a surviving zuno is still serving: {:?}",
        rebind.err()
    );

    // And: it was the pid the client held all along.
    let logs = diagnostics.join("\n");
    assert_eq!(
        logged_pid(&logs),
        Some(supervised),
        "`serve` ran under a different pid than the {supervised} the client \
         spawned, so the kill could only ever have reached the wrong \
         process\nstderr:\n{logs}"
    );
}

/// **Both CLI references state the process guarantee, in both languages.**
///
/// The guarantee above is what a client integrates against, and the platform
/// difference behind it — Unix replaces its own image, Windows keeps the resolved
/// values inside the process — is the kind of detail that is only ever written down
/// once. The pages that clients actually read are the CLI index and the two pages for
/// the commands they launch, so a change that alters process lifetime has to leave all
/// of them, and their Chinese mirrors, saying the same thing.
#[test]
fn the_cli_reference_states_the_process_guarantee_in_both_languages() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let pages: [(&str, &[&str]); 6] = [
        (
            "docs/cli/index.md",
            &[
                "## One invocation, one process",
                "one process per\ninvocation on every supported platform",
                "replaces its own image once at startup, keeping the\nsame process id",
                "Windows has no equivalent replacement",
            ],
        ),
        (
            "docs/zh/cli/index.md",
            &[
                "## 一次调用就是一个进程",
                "每次调用都只得到\n一个进程",
                "替换自身镜像一次（进程 id 不变）",
                "Windows 没有等价的替换操作",
            ],
        ),
        (
            "docs/cli/acp.md",
            &["[One invocation, one process](/cli/#one-invocation-one-process)"],
        ),
        (
            "docs/cli/serve.md",
            &["[One invocation, one process](/cli/#one-invocation-one-process)"],
        ),
        (
            "docs/zh/cli/acp.md",
            &["[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)"],
        ),
        (
            "docs/zh/cli/serve.md",
            &["[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)"],
        ),
    ];

    let mut missing = Vec::new();
    for (page, needles) in pages {
        let path = root.join(page);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for needle in needles {
            if !text.contains(needle) {
                missing.push(format!("{page} no longer states {needle:?}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "the CLI reference and its mirror have to describe the process lifetime this \
         binary actually has:\n  {}",
        missing.join("\n  ")
    );
}
