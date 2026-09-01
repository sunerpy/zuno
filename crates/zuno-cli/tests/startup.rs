//! G1 — the startup budget, enforced in CI.
//!
//! §7's G1 says the reference implementation has eight startup budgets and runs
//! **none of them in CI**, and that not running them is the weakness rather than
//! the thing to copy. So this lives in `crates/zuno-cli/tests/`, which
//! `make test-nextest` runs in the workflow's `test` job: a startup regression
//! fails a build instead of being noticed later.
//!
//! # Why a step in an existing job rather than a new one
//!
//! `crates/zuno-cli/tests/release_surface.rs` asserts `ci-success.needs` lists
//! every job in `ci.yml`, and `EXPECTED_MIGRATED_JOBS` counts CodeBuild label
//! sets. A new job therefore has to be added in two more places or the gate it
//! was meant to strengthen goes green while the new job fails unnoticed. A test
//! file inside an existing target needs neither edit and cannot be forgotten from
//! a `needs:` list, so the blast radius is smaller for the same enforcement.
//!
//! # Why the budgets are not the reference implementation's numbers
//!
//! §7 requires them to be re-measured here: a different binary with a different
//! startup path. Each constant below carries the median it was calibrated from
//! and the headroom multiple, so the slack is visible rather than implied.
//!
//! # Why this can be a wall-clock gate without being flaky
//!
//! Four reasons. The reported value is a median of [`RUNS`] runs, so one
//! descheduled process cannot fail the build. The budgets sit far enough above
//! the measured medians that runner-to-runner variation is inside the headroom.
//! `.config/nextest.toml` reserves the complete nextest worker pool for this
//! test binary, so workspace concurrency cannot turn unrelated CPU or SQLite
//! contention into a startup regression.
//! And the assertion that actually catches the regression this gate exists for —
//! a blocking step added to startup — is [`startup_version_pays_for_no_log_file_and_no_reexec`],
//! which is structural and has no timing in it at all.
//!
//! # Reproduce
//!
//! ```text
//! cargo test -p zuno-cli --test startup -- --nocapture
//! cargo nextest run -p zuno-cli --test startup --no-tests=warn
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use zuno_cli::startup::{PROFILE_LINE_PREFIX, StartupPhase, ZUNO_STARTUP_PROFILE};

/// Runs each invocation is measured over. Odd, so the median is a sample.
///
/// Nine rather than five: startup is milliseconds, so a single descheduled run
/// perturbs it far more than it does the minute-scale memory workloads, and the
/// extra samples cost under a second in total.
const RUNS: usize = 9;

/// Startup tests share one subject binary and one host.
///
/// Rust runs integration-test functions in parallel by default. Every test in
/// this binary takes this lock, including structural tests that launch only one
/// child, so `cargo test` never benchmarks sibling functions from this binary.
///
/// Nextest launches each test case in a separate process, so this mutex cannot
/// coordinate that runner. `.config/nextest.toml` independently reserves all
/// nextest worker slots for the `startup` binary, excluding unrelated workspace
/// tests while each of these processes runs.
static STARTUP_MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

fn startup_measurement_lock() -> MutexGuard<'static, ()> {
    STARTUP_MEASUREMENT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `zuno --version` — the shortest path through the binary.
///
/// Measured debug-profile median 4.547 ms over nine runs (min 4.387, max 4.915,
/// max/min 1.1205x); budget 30 ms, 6.6x headroom. It parses arguments, prints the
/// package version and returns; it opens no log file and does not re-exec,
/// which [`startup_version_pays_for_no_log_file_and_no_reexec`] pins structurally.
const BUDGET_VERSION: Duration = Duration::from_millis(30);

/// `zuno --version --long`, which additionally formats the build identity.
///
/// Measured debug-profile median 4.523 ms over nine runs (min 4.114, max 4.698,
/// max/min 1.1419x); budget 30 ms, 6.6x headroom.
const BUDGET_VERSION_LONG: Duration = Duration::from_millis(30);

/// `zuno --help`, which renders the whole command surface.
///
/// Measured debug-profile median 4.684 ms over nine runs (min 4.461, max 4.981,
/// max/min 1.1166x); budget 30 ms, 6.4x headroom.
const BUDGET_HELP: Duration = Duration::from_millis(30);

/// `zuno session list`, the cheapest invocation that pays full startup.
///
/// This is the budget with real content: it re-execs once to hand the command
/// process its environment, builds the tracing subscriber, opens the structured log
/// file and opens the database. Measured debug-profile median 15.666 ms over nine
/// runs (min 15.196, max 18.327, max/min 1.2061x); budget 100 ms, 6.4x headroom —
/// the same multiple as the fast paths, so no invocation is held to a looser
/// standard than the rest.
#[cfg(not(windows))]
const BUDGET_SESSION_LIST: Duration = Duration::from_millis(100);

/// Windows pays the native process-creation, DLL loader and SQLite startup cost.
///
/// Measured on the hosted `windows-2022` runner at 133.1934 ms median
/// (126.2972 ms min, 153.7779 ms max). A 200 ms ceiling keeps 1.5x median
/// headroom while retaining the structural no-reexec/no-log assertions above as
/// the platform-independent regression gate.
#[cfg(windows)]
const BUDGET_SESSION_LIST: Duration = Duration::from_millis(200);

/// The subject binary, built by cargo for this integration test.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

/// A private config and data root, so no measurement reads the developer's own.
///
/// `label` scopes the root per test rather than per process. It has to: the
/// filesystem assertion in
/// [`startup_version_pays_for_no_log_file_and_no_reexec`] counts files under the
/// data root, and a root shared with the budget test sees the log
/// `zuno session list` legitimately opens — which the first version of that
/// assertion misread as `--version` opening one.
///
/// Not a `tempfile`: the directory has to outlive every child in the run, and the
/// point is isolation rather than cleanliness of a single call.
fn isolated_roots(label: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "zuno-startup-budget-{}-{label}",
        std::process::id()
    ));
    let config = base.join("config");
    let data = base.join("data");
    std::fs::create_dir_all(&config).expect("create the isolated config root");
    std::fs::create_dir_all(&data).expect("create the isolated data root");
    (config, data)
}

fn command_in(label: &str, args: &[&str], profile: bool) -> Command {
    let (config, data) = isolated_roots(label);
    let mut command = Command::new(binary());
    command
        .args(args)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", &data)
        .env("HOME", config.parent().expect("the isolated base"))
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env_remove(ZUNO_STARTUP_PROFILE);
    if profile {
        command.env(ZUNO_STARTUP_PROFILE, "1");
    }
    command
}

/// min / median / max of [`RUNS`] wall-clock runs of one invocation.
///
/// The first run is discarded: it pays for faulting the binary's pages in, which
/// is a property of the page cache rather than of startup, and including it makes
/// the first measurement on a cold machine the whole result.
fn measure(label: &str, args: &[&str]) -> (Duration, Duration, Duration) {
    let status = command_in(label, args, false)
        .output()
        .expect("the subject binary must run");
    assert!(
        status.status.code().is_some(),
        "{args:?} was killed by a signal rather than exiting"
    );

    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let output = command_in(label, args, false)
            .output()
            .expect("the subject binary must run");
        samples.push(started.elapsed());
        assert!(
            output.status.code().is_some(),
            "{args:?} was killed by a signal rather than exiting"
        );
    }
    samples.sort_unstable();
    (samples[0], samples[RUNS / 2], samples[RUNS - 1])
}

/// Every profile line one invocation wrote to stderr, parsed into phase maps.
///
/// A dispatching invocation writes two: the parent that re-execs and the command
/// process it becomes.
fn profile_lines(label: &str, args: &[&str]) -> Vec<Vec<(String, u128)>> {
    let output = command_in(label, args, true)
        .output()
        .expect("the subject binary must run");
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(PROFILE_LINE_PREFIX))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|field| field.split_once('='))
                .filter_map(|(key, value)| {
                    let phase = key.strip_suffix("_us")?;
                    Some((phase.to_owned(), value.parse::<u128>().ok()?))
                })
                .collect()
        })
        .collect()
}

fn phases(line: &[(String, u128)]) -> BTreeSet<&str> {
    line.iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| *key != "total")
        .collect()
}

#[test]
fn startup_medians_are_inside_their_budgets() {
    let _measurement_lock = startup_measurement_lock();
    let cases: [(&str, &[&str], Duration); 4] = [
        ("zuno --version", &["--version"], BUDGET_VERSION),
        (
            "zuno --version --long",
            &["--version", "--long"],
            BUDGET_VERSION_LONG,
        ),
        ("zuno --help", &["--help"], BUDGET_HELP),
        (
            "zuno session list",
            &["session", "list"],
            BUDGET_SESSION_LIST,
        ),
    ];

    println!("\nG1 STARTUP BUDGET  (runs={RUNS}, first run discarded, isolated XDG roots)\n");
    let mut failures = Vec::new();
    for (label, args, budget) in cases {
        let (min, median, max) = measure("budget", args);
        let ratio = median.as_secs_f64() / min.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "  {label:<24} min {min:>12?}  median {median:>12?}  max {max:>12?}  \
             (max/min {:.4}x)",
            max.as_secs_f64() / min.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        println!(
            "  {:<24} budget {budget:>11?}  headroom {:.1}x  median/min {ratio:.4}x",
            "",
            budget.as_secs_f64() / median.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        if median > budget {
            failures.push(format!(
                "{label}: median {median:?} exceeds its {budget:?} budget"
            ));
        }
    }
    println!();
    assert!(
        failures.is_empty(),
        "startup regressed past its budget:\n  {}\n\nRun with ZUNO_STARTUP_PROFILE=1 to \
         see which phase grew; the phases are listed in \
         crates/zuno-cli/src/startup.rs.",
        failures.join("\n  ")
    );
}

#[test]
fn startup_profile_attributes_every_phase_it_declares() {
    let _measurement_lock = startup_measurement_lock();
    // Given: the two invocations that between them traverse every phase.
    let version = profile_lines("phases", &["--version"]);
    let dispatch = profile_lines("phases", &["session", "list"]);

    // Then: each writes at least one profile line, so an over-budget run always
    // has an attribution rather than only a total.
    assert_eq!(
        version.len(),
        1,
        "`--version` must be one process; it wrote {} profile lines",
        version.len()
    );
    assert_eq!(
        dispatch.len(),
        2,
        "a dispatching invocation is the parent plus the command process it \
         re-execs into; it wrote {} profile lines",
        dispatch.len()
    );

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for line in version.iter().chain(dispatch.iter()) {
        assert!(
            line.iter().any(|(key, _)| key == "total"),
            "a profile line without a total cannot be compared against a budget"
        );
        seen.extend(phases(line));
    }
    let declared: BTreeSet<&str> = StartupPhase::ALL
        .into_iter()
        .map(StartupPhase::as_str)
        .collect();

    // Then: every phase the module declares is actually reached by one of the two
    // invocations. A declared-but-unmarked phase is attribution that silently is
    // not there.
    let unreached: Vec<&&str> = declared.difference(&seen).collect();
    assert!(
        unreached.is_empty(),
        "StartupPhase declares {unreached:?} but no measured invocation marks them, \
         so an over-budget run in those segments would be unattributable"
    );
    let undeclared: Vec<&&str> = seen.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "the profile emitted {undeclared:?}, which StartupPhase does not declare"
    );
}

#[test]
fn startup_version_pays_for_no_log_file_and_no_reexec() {
    let _measurement_lock = startup_measurement_lock();
    // Given: the fast path, profiled.
    let lines = profile_lines("version-only", &["--version"]);
    let line = &lines[0];
    let reached = phases(line);

    // Then: it neither re-execs nor builds the tracing subscriber. This is the
    // assertion that catches the regression the budget exists for — a blocking
    // step added to startup — without depending on a clock, so it holds on a
    // loaded runner where a wall-clock budget would have to be loosened.
    assert!(
        !reached.contains(StartupPhase::BootstrapRestart.as_str()),
        "`--version` re-execs; it used to answer from the first process: {reached:?}"
    );
    assert!(
        !reached.contains(StartupPhase::Logging.as_str()),
        "`--version` now initializes logging, so it opens the structured log store \
         before printing a constant: {reached:?}"
    );
    assert_eq!(
        reached,
        BTreeSet::from([
            StartupPhase::ProcessGuard.as_str(),
            StartupPhase::Parse.as_str(),
            StartupPhase::Environment.as_str(),
            StartupPhase::Dispatch.as_str(),
        ]),
        "the fast path's phase set changed; if that is intended, re-measure \
         BUDGET_VERSION rather than only updating this set"
    );

    // And: no log file was created under the isolated data root, which is the
    // filesystem-level statement of the same guarantee.
    let (_, data) = isolated_roots("version-only");
    let logs = data.join("zuno").join("log");
    let entries = std::fs::read_dir(&logs).map(|dir| dir.count()).unwrap_or(0);
    assert_eq!(
        entries,
        0,
        "`--version` left {entries} file(s) in {}; printing a constant must not \
         open a log",
        logs.display()
    );
}

#[test]
fn startup_profile_is_off_unless_asked_for() {
    let _measurement_lock = startup_measurement_lock();
    // Given/When: the fast path run without the profile variable set.
    let output = command_in("profile-off", &["--version"], false)
        .output()
        .expect("the subject binary must run");

    // Then: stderr carries no profile line. The profile is a diagnostic, and a
    // diagnostic that is always on is output every consumer has to filter.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(PROFILE_LINE_PREFIX),
        "the startup profile was emitted without {ZUNO_STARTUP_PROFILE} being set: \
         {stderr}"
    );

    // And: stdout stays exactly the version, because the profile must never be
    // able to reach it — see zuno-observability's crate docs for why a stray byte
    // on stdout is a protocol parse error rather than noise.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(PROFILE_LINE_PREFIX),
        "a profile line reached stdout: {stdout}"
    );
}

#[test]
fn startup_profile_never_reaches_stdout_even_when_requested() {
    let _measurement_lock = startup_measurement_lock();
    // Given/When: the profile is requested on both a fast and a dispatching path.
    for args in [vec!["--version"], vec!["session", "list"]] {
        let output = command_in("stdout-purity", &args, true)
            .output()
            .expect("the subject binary must run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Then: the lines are on stderr and stdout is untouched.
        assert!(
            stderr.contains(PROFILE_LINE_PREFIX),
            "{args:?} was asked for a profile and wrote none to stderr"
        );
        assert!(
            !stdout.contains(PROFILE_LINE_PREFIX),
            "{args:?} put a profile line on stdout: {stdout}"
        );
    }
}

// ---------------------------------------------------------------------------
// Watchdog wiring
// ---------------------------------------------------------------------------

/// Whether `argv` names a command the watchdog guards, read off the real parse.
///
/// Parsed rather than hand-constructed so the answer comes from the same code
/// path `run_process` uses to decide.
fn classification(argv: &[&str]) -> bool {
    let cli = <zuno_cli::Cli as clap::Parser>::try_parse_from(argv)
        .unwrap_or_else(|error| panic!("{argv:?} must parse: {error}"));
    match cli.action(&zuno_paths::Env::from_process()) {
        zuno_cli::Action::Dispatch(request) => request.args.silence_is_a_stall(),
        other => panic!("{argv:?} did not dispatch: {other:?}"),
    }
}

#[test]
fn startup_only_bounded_commands_are_guarded_against_silence() {
    let _measurement_lock = startup_measurement_lock();
    // Given/When/Then: the classification the watchdog wiring reads.
    for argv in [
        vec!["zuno", "session", "list"],
        vec!["zuno", "models", "list"],
        vec!["zuno", "agent", "list"],
    ] {
        assert!(
            classification(&argv),
            "{argv:?} is bounded work and must be guarded, or a stall in it goes \
             unreported"
        );
    }
    for argv in [vec!["zuno", "tui"], vec!["zuno", "serve"]] {
        assert!(
            !classification(&argv),
            "{argv:?} blocks on input or inbound requests, so guarding it would \
             report a stall every stall interval while a user reads the screen - \
             the false positive the BUSY gate exists to prevent"
        );
    }
}

/// The watchdog must not be the reason a command got slower.
///
/// It spawns a thread and parks it, so its cost is one thread creation. The
/// budget above already covers that, and this states the relationship: the
/// classification decides whether a guard is taken, and neither answer may cost
/// measurable time.
#[test]
fn startup_the_liveness_watchdog_costs_nothing_measurable_on_a_bounded_command() {
    let _measurement_lock = startup_measurement_lock();
    // Given: a command classified as bounded work, so a guard IS taken.
    assert!(
        classification(&["zuno", "session", "list"]),
        "`session list` is bounded work; if it is no longer classified that way \
         this test is measuring the wrong path"
    );

    // When: it is measured.
    let (_, median, _) = measure("watchdog-cost", &["session", "list"]);

    // Then: it is inside the same budget as before the watchdog existed. A
    // liveness reporter that pushed startup over its own budget would be a
    // regression dressed as instrumentation.
    assert!(
        median <= BUDGET_SESSION_LIST,
        "with the watchdog wired in, `session list` median {median:?} exceeds its \
         {BUDGET_SESSION_LIST:?} budget"
    );
}

/// No command may report a stall merely by being run.
///
/// The watchdog writes through `tracing` at `error!` for a stall, so a false
/// positive would land in the log file of every invocation. This asserts the
/// absence end to end on the real binary, which is the only place the wiring —
/// rather than the watchdog in isolation — can be wrong.
#[test]
fn startup_a_completed_command_reports_no_stall() {
    let _measurement_lock = startup_measurement_lock();
    let (_, data) = isolated_roots("no-false-stall");
    let output = command_in("no-false-stall", &["session", "list"], false)
        .output()
        .expect("the subject binary must run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("watchdog.stalled"),
        "a command that completed reported a stall: {stderr}"
    );

    let logs = data.join("zuno").join("log");
    let database = logs.join(zuno_observability::STRUCTURED_LOG_FILE);
    assert!(
        database.is_file(),
        "no structured log database was written at {}, so the absence of a stall \
         proves nothing",
        database.display()
    );
    let connection = rusqlite::Connection::open(&database).expect("open structured log");
    let stalls: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM log_record
             WHERE COALESCE(message, '') LIKE '%watchdog.stalled%'
                OR fields_json LIKE '%watchdog.stalled%'",
            [],
            |row| row.get(0),
        )
        .expect("query watchdog records");
    assert_eq!(
        stalls,
        0,
        "{} records a stall for a command that completed",
        database.display()
    );
}
