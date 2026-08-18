//! Whether this binary's memory alerts can actually be observed.
//!
//! # Why this file exists
//!
//! `main.rs` spawned a `MemorySampler` and installed no tracing subscriber. The sampler
//! ran, sampled, judged, formatted an incident and handed it to
//! `tracing::warn!(target: "memory", ...)` — which, with no subscriber, is a no-op
//! dispatcher. A server could pass 2 GiB or grow 512 MiB in fifteen minutes and emit
//! nothing at all. That is the same defect the sampler was added to fix — machinery that
//! runs and cannot be observed — one layer further out, and it survived a round of review
//! because every test in the previous round exercised the sampler through an injected
//! recording sink, which is a sink that exists by construction.
//!
//! # What is actually proven, and how
//!
//! Three claims, none of which is "a sink we handed it received something":
//!
//! 1. **The shipped binary installs a file sink.** [`the_shipped_binary_writes_a_log_file`]
//!    runs the real `zuno-server` executable against an isolated `XDG_DATA_HOME` and reads
//!    the file off disk afterwards. No test harness is between the process and the file.
//! 2. **A real incident reaches a real file through the production sink.**
//!    [`a_memory_incident_reaches_the_log_file_through_the_production_sink`] composes the
//!    two calls `main` makes, in `main`'s order, with `TracingSink` — the sink that ships —
//!    and greps the resulting file for the incident. The only thing scripted is the byte
//!    source, because a test process cannot be made to occupy 2 GiB.
//! 3. **The order is right.** [`logging_is_installed_before_the_sampler_that_reports_to_it`]
//!    reads `main.rs` and checks that `init` precedes `MemorySampler::spawn` and that the
//!    handle is bound to a named local. A handle bound to `_` compiles, drops immediately,
//!    and silently restores the defect.

use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use zuno_observability::memory::{
    MemorySample, MemorySampler, MemorySource, TracingSink, WARNING_RSS_KIB,
};
use zuno_observability::{LogConfig, Rotation};

/// Longest any wait here takes before failing.
const DEADLINE: Duration = Duration::from_secs(30);

/// A source that always reports a process over the warning size.
///
/// Scripted because a test cannot make its own process resident in 2 GiB, and waiting for
/// one that could would be the unbounded wait this repository keeps removing. Everything
/// downstream of it — ring, thresholds, attribution, rate limiting, `TracingSink`, the
/// subscriber, the file — is the production path.
struct OverThreshold;

impl MemorySource for OverThreshold {
    fn sample(&mut self, elapsed: Duration) -> Option<MemorySample> {
        Some(MemorySample {
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            anon_kib: WARNING_RSS_KIB,
            file_kib: 4_096,
            shmem_kib: 0,
            active_sessions: 1,
        })
    }
}

#[test]
fn a_memory_incident_reaches_the_log_file_through_the_production_sink() {
    // `Rotation::Never` so the file name is `opencode.log` and not today's date, which is
    // what lets this assert on bytes rather than on a glob.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let log_dir = directory.path().join("log");
    let logging =
        zuno_observability::init(LogConfig::from_env(&log_dir).with_rotation(Rotation::Never))
            .expect("logging initialises");
    assert!(
        logging.installed(),
        "this test owns the process's subscriber; another test in this binary took it \
         first, so the file below would stay empty for a reason unrelated to the defect"
    );

    let sampler = MemorySampler::spawn_with(Duration::from_millis(20), OverThreshold, TracingSink);
    let log_path = log_dir.join("opencode.log");
    let deadline = Instant::now() + DEADLINE;
    let mut contents = String::new();
    while Instant::now() < deadline {
        contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        if contents.contains("memory.incident") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Joins the sampler thread and then flushes the appender, in that order, exactly as
    // `main` does — so a record still in the writer's queue is on disk before the read.
    sampler.shutdown();
    drop(logging);
    let contents = std::fs::read_to_string(&log_path).unwrap_or(contents);

    assert!(
        contents.contains("memory.incident"),
        "no incident reached the log file at {}. The sampler ran and judged the process \
         over {WARNING_RSS_KIB} KiB, so an empty file means the alert went to a no-op \
         dispatcher — which is exactly what the standalone server did.\nfile:\n{contents}",
        log_path.display()
    );
    assert!(
        contents.contains("severity=warning"),
        "the incident reached the file without its severity:\n{contents}"
    );
    assert!(
        contents.contains("WARN"),
        "the record landed below warn level, so a default filter would hide it:\n{contents}"
    );
}

#[test]
fn the_shipped_binary_writes_a_log_file() {
    // The link to the *binary*. Nothing above proves `main` calls any of it, and a source
    // needle only proves the text is present. This starts the real executable, waits for it
    // to say it is listening, stops it, and reads the file it left behind.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zuno-server"))
        .arg("serve")
        // Port 0, so a busy port on the runner cannot fail this.
        .args(["--port", "0"])
        // Every root isolated, so the run cannot read or write a developer's real data.
        .env("XDG_DATA_HOME", directory.path().join("data"))
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("XDG_CACHE_HOME", directory.path().join("cache"))
        .env("XDG_STATE_HOME", directory.path().join("state"))
        .env("HOME", directory.path())
        .env_remove("ZUNO_LOG_LEVEL")
        .env_remove("ZUNO_PRINT_LOGS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the zuno-server binary runs");

    // It prints its URL once it is bound, which is the only signal that startup — logging
    // included — finished. Reading one line rather than waiting a fixed time: a sleep long
    // enough for a loaded runner would make this test slow, and a short one flaky.
    let stdout = child.stdout.take().expect("piped stdout");
    let (found, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _read = BufReader::new(stdout).read_line(&mut line);
        let _sent = found.send(line);
    });
    let url = receiver.recv_timeout(DEADLINE);
    let stopped = child.kill();
    let output = child.wait_with_output().expect("the child is reaped");

    assert!(stopped.is_ok(), "the server could not be stopped");
    let url = url.unwrap_or_default();
    assert!(
        url.starts_with("http://"),
        "the server never reported a bound address, so it did not finish starting up and \
         this test proves nothing about its logging: {url:?}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log_dir = directory.path().join("data").join("zuno").join("log");
    let written = std::fs::read_dir(&log_dir)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    assert!(
        written > 0,
        "the server bound to {} and left no log file in {}. With no subscriber installed \
         every memory alert it raises goes nowhere.\nstderr:\n{}",
        url.trim(),
        log_dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn logging_is_installed_before_the_sampler_that_reports_to_it() {
    // Order, which neither test above can see. Installing the subscriber *after* the
    // sampler would leave a window in which alerts vanish, and on a process that trips the
    // threshold during startup that window is the whole of the evidence.
    let main = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"),
    )
    .expect("read the server's main.rs");
    let init = main
        .find("zuno_observability::init(")
        .expect("the server initialises logging");
    let spawn = main
        .find("MemorySampler::spawn(")
        .expect("the server spawns a memory sampler");
    assert!(
        init < spawn,
        "the memory sampler is spawned before logging is installed, so anything it \
         reports in between is discarded"
    );
    assert!(
        main.contains("let logging =") && main.contains("drop(logging);"),
        "the log handle is not bound to a named local that outlives the sampler. Binding \
         it to `_` compiles, drops the appender's worker guard immediately, and silences \
         every alert while looking correct"
    );

    // The count is read from the same global the sampler reads, so a wiring that passed a
    // freshly-made counter — which would always read zero — fails here.
    assert!(
        main.contains("zuno_observability::memory::active_sessions()"),
        "the sampler is not given the process-wide session count"
    );
    let _typed: Arc<AtomicU32> = Arc::clone(zuno_observability::memory::active_sessions());
}
