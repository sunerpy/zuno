//! G1/G2 compare released TUI process trees through the frozen performance runner.
//!
//! The runner's name is TypeScript-specific, but its public binary override is
//! not: `ZUNO_TESTKIT_ORACLE` is resolved before any workload starts. Two
//! sequential passes route each frozen launch through one immediate dispatcher;
//! their public reports are split by [`interleaved_pair_order`] into five AB/BA
//! pairs. The private workload, database snapshot, windows, process-tree walk and
//! aggregation rule therefore stay single-sourced in `zuno_testkit::perf`.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use zuno_testkit::perf::{
    BaselineReport, BaselineRunOptions, FrozenThresholds, PairedSide, RunMeasurement,
    W_REAL_RECAPTURE, W_REAL_SUBJECT, WorkloadName, interleaved_pair_order,
    load_committed_baseline, measure_typescript_baseline, verify_pinned_database,
};

const MEMORY_GATE_MODE: &str = "ZUNO_MEMORY_GATE_MODE";
const MEMORY_GATE_WORKER_OUTPUT: &str = "ZUNO_MEMORY_GATE_WORKER_OUTPUT";
const MEMORY_GATE_DATABASE: &str = "ZUNO_MEMORY_GATE_DATABASE";
const MEMORY_GATE_ALLOCATOR: &str = "ZUNO_MEMORY_GATE_ALLOCATOR";
const WORKER_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllocatorConfiguration {
    Jemalloc,
    System,
}

impl AllocatorConfiguration {
    fn from_environment() -> Self {
        match std::env::var(MEMORY_GATE_ALLOCATOR).as_deref() {
            Ok("jemalloc") | Err(std::env::VarError::NotPresent) => Self::Jemalloc,
            Ok("system") => Self::System,
            Ok(other) => panic!(
                "unsupported {MEMORY_GATE_ALLOCATOR}={other:?}; accepted values are `jemalloc` and `system`"
            ),
            Err(std::env::VarError::NotUnicode(_)) => {
                panic!("{MEMORY_GATE_ALLOCATOR} must be valid UTF-8")
            }
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Jemalloc => "jemalloc",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GateVerdict {
    rust_median_kib: u64,
    baseline_median_kib: u64,
    ceiling_mib: f64,
    ratio: f64,
    passed: bool,
}

impl GateVerdict {
    fn evaluate(rust_median_kib: u64, baseline_median_kib: u64, ceiling_mib: f64) -> Self {
        let rust_mib = rust_median_kib as f64 / 1024.0;
        Self {
            rust_median_kib,
            baseline_median_kib,
            ceiling_mib,
            ratio: rust_median_kib as f64 / baseline_median_kib as f64,
            passed: rust_mib <= ceiling_mib,
        }
    }

    fn ceiling_kib(self) -> u64 {
        (self.ceiling_mib * 1024.0).round() as u64
    }
}

#[derive(Debug)]
struct SubjectRuns {
    idle: Vec<RunMeasurement>,
    real: Vec<RunMeasurement>,
}

impl SubjectRuns {
    fn runs(&self, workload: WorkloadName) -> &[RunMeasurement] {
        match workload {
            WorkloadName::WIdle => &self.idle,
            WorkloadName::WReal => &self.real,
            WorkloadName::WSoak => panic!("W-soak belongs to todo 89"),
        }
    }

    fn median(&self, workload: WorkloadName) -> u64 {
        median_of_runs(self.runs(workload))
    }
}

#[derive(Debug)]
struct Worker {
    label: &'static str,
    child: Child,
    log: PathBuf,
}

/// Aggregate test runs must stay practical, while the literal target invocation
/// is the operator's explicit request to spend roughly two hours measuring.
#[test]
fn g1_and_g2_use_at_most_half_the_committed_typescript_baseline() {
    let baseline = load_committed_baseline()
        .expect("benchmarks/ts-baseline.json is mandatory; a missing baseline must fail closed");
    assert_w_real_provenance("committed TypeScript baseline", &baseline);

    if !should_run_expensive_gate() {
        eprintln!(
            "SKIP expensive G1/G2 measurement in aggregate test run; \
             `cargo test --test memory` runs the real paired gate"
        );
        return;
    }

    let workspace = workspace_root();
    let target = target_dir(&workspace);
    let allocator = AllocatorConfiguration::from_environment();
    let rust_release = build_release_subject(&workspace, &target, allocator);
    let rust_version = binary_version(&rust_release);
    let database = database_path();
    assert_pinned_database(&database);

    let typescript_binary = baseline.machine.typescript_binary.clone();
    assert!(
        typescript_binary.is_file(),
        "committed TypeScript binary is missing at {}",
        typescript_binary.display()
    );

    let root = prepare_measurement_root(
        &target,
        &rust_release,
        &typescript_binary,
        &database,
        allocator,
    );
    let rust_as_opencode = root.join("rust-subject/opencode");
    copy_subject(&rust_release, &rust_as_opencode);

    let test_binary = std::env::current_exe().expect("locate memory test binary");
    let mut reports = Vec::with_capacity(2);
    for pass in 0..2 {
        let state = root.join(format!("pass-{pass}.state"));
        let schedule = root.join(format!("pass-{pass}.schedule"));
        let wrapper = root.join(format!("pass-{pass}-dispatcher"));
        let report_path = root.join(format!("pass-{pass}.json"));
        let log = root.join(format!("pass-{pass}.log"));
        if report_path.is_file() {
            assert_dispatch_count(&state);
            eprintln!("RESUME completed {} measurement pass", pass + 1);
            reports.push(BaselineReport::load(&report_path).expect("load completed frozen pass"));
            continue;
        }
        std::fs::write(&state, b"0\n").expect("initialize dispatcher state");
        write_pass_schedule(&schedule, pass);
        write_dispatcher(
            &wrapper,
            &typescript_binary,
            &rust_as_opencode,
            &state,
            &schedule,
        );
        let worker = spawn_worker(
            if pass == 0 { "first" } else { "second" },
            &test_binary,
            &wrapper,
            &database,
            &report_path,
            &log,
        );
        wait_for_worker(worker).expect("frozen measurement pass must finish successfully");
        assert_dispatch_count(&state);
        reports.push(BaselineReport::load(&report_path).expect("load frozen pass report"));
    }

    let first = &reports[0];
    let second = &reports[1];
    assert_same_machine(first, second);
    assert_w_real_provenance("first paired pass", first);
    assert_w_real_provenance("second paired pass", second);
    let (paired_ts, rust) = split_subject_runs(first, second);

    let thresholds = FrozenThresholds::from_baseline(&baseline)
        .expect("committed baseline must produce the frozen thresholds");
    let baseline_idle = baseline_median(&baseline, WorkloadName::WIdle);
    let baseline_real = baseline_median(&baseline, WorkloadName::WReal);
    let paired_ts_idle = paired_ts.median(WorkloadName::WIdle);
    let paired_ts_real = paired_ts.median(WorkloadName::WReal);
    let rust_idle = rust.median(WorkloadName::WIdle);
    let rust_real = rust.median(WorkloadName::WReal);
    let g1 = GateVerdict::evaluate(rust_idle, baseline_idle, thresholds.g1_max_mib);
    let g2 = GateVerdict::evaluate(rust_real, baseline_real, thresholds.g2_max_mib);

    write_gate_artifact(
        &target,
        &baseline,
        first,
        second,
        &paired_ts,
        &rust,
        g1,
        g2,
        &rust_release,
        &rust_version,
        &database,
        allocator,
    );
    eprintln!(
        "G1: Rust={} KiB, paired TS={} KiB, committed TS={} KiB, ceiling={} KiB, Rust/committed={:.4}, paired/committed={:.4}, verdict={}",
        rust_idle,
        paired_ts_idle,
        baseline_idle,
        g1.ceiling_kib(),
        g1.ratio,
        paired_ts_idle as f64 / baseline_idle as f64,
        if g1.passed { "PASS" } else { "FAIL" }
    );
    eprintln!(
        "G2: Rust={} KiB, paired TS={} KiB, committed TS={} KiB, ceiling={} KiB, Rust/committed={:.4}, paired/committed={:.4}, verdict={}",
        rust_real,
        paired_ts_real,
        baseline_real,
        g2.ceiling_kib(),
        g2.ratio,
        paired_ts_real as f64 / baseline_real as f64,
        if g2.passed { "PASS" } else { "FAIL" }
    );

    assert!(
        g1.passed,
        "G1 failed: Rust {} KiB is {:.4}x the committed TypeScript {} KiB baseline and exceeds \
         the {} KiB ceiling",
        g1.rust_median_kib,
        g1.ratio,
        g1.baseline_median_kib,
        g1.ceiling_kib()
    );
    assert!(
        g2.passed,
        "G2 failed: Rust {} KiB is {:.4}x the committed TypeScript {} KiB baseline and exceeds \
         the {} KiB ceiling",
        g2.rust_median_kib,
        g2.ratio,
        g2.baseline_median_kib,
        g2.ceiling_kib()
    );
}

/// A worker is a separate process because Rust 2024 correctly makes mutating a
/// process-global environment unsafe while another async task may read it.
#[tokio::test]
async fn frozen_measurement_worker() {
    let Some(output) = std::env::var_os(MEMORY_GATE_WORKER_OUTPUT) else {
        return;
    };
    let options = BaselineRunOptions::todo_93(PathBuf::from(output));
    measure_typescript_baseline(&options)
        .await
        .expect("the frozen runner must complete every G1/G2 repetition");
}

#[test]
fn the_committed_baseline_measures_the_pinned_w_real_subject() {
    // Given: the committed TypeScript baseline that supplies G2's ceiling.
    let baseline = load_committed_baseline().expect("committed baseline");

    // When/Then: its recorded subject is the pinned one, so `0.50 x` its W-real
    // median is a ceiling for the same workload the gate will run.
    assert_w_real_provenance("committed TypeScript baseline", &baseline);
}

#[test]
fn a_report_about_another_session_fails_the_provenance_check() {
    // Given: the committed baseline with its W-real session id replaced.
    let mut substituted = load_committed_baseline().expect("committed baseline");
    let real = substituted
        .workloads
        .iter_mut()
        .find(|workload| workload.name == WorkloadName::WReal)
        .expect("W-real must exist");
    real.session_id = Some("ses_024892384ffe0oExC895WV7lhE".to_owned());
    real.part_data_bytes = Some(299_771_941);

    // When: the provenance check runs against it.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_w_real_provenance("substituted pass", &substituted);
    }));

    // Then: it fails rather than reporting a note, and the message names the pin,
    // the substituted session and the recapture procedure. A gate that accepted
    // this would compare a heavier workload against an unchanged ceiling.
    let payload = outcome.expect_err("a substituted subject must fail the gate");
    let message = payload
        .downcast_ref::<String>()
        .map_or_else(String::new, String::clone);
    assert!(message.contains(W_REAL_SUBJECT.session_id), "{message}");
    assert!(
        message.contains("ses_024892384ffe0oExC895WV7lhE"),
        "{message}"
    );
    assert!(message.contains("W_REAL_SUBJECT"), "{message}");
    assert!(message.contains("benchmarks/ts-baseline.json"), "{message}");
}

#[test]
fn a_missing_baseline_is_an_error_not_a_permissive_threshold() {
    let root = tempfile::tempdir().expect("temporary missing-baseline path");
    let missing = root.path().join("baseline.json");
    let error = BaselineReport::load(&missing).expect_err("an absent baseline must fail");
    assert!(error.to_string().contains("baseline.json"), "{error}");
}

#[test]
fn deliberately_inflated_rust_measurements_fail_both_gates() {
    let baseline = load_committed_baseline().expect("committed baseline");
    let thresholds = FrozenThresholds::from_baseline(&baseline).expect("frozen thresholds");
    let idle = baseline_median(&baseline, WorkloadName::WIdle);
    let real = baseline_median(&baseline, WorkloadName::WReal);

    let inflated_g1 = GateVerdict::evaluate(idle.saturating_add(1), idle, thresholds.g1_max_mib);
    let inflated_g2 = GateVerdict::evaluate(real.saturating_add(1), real, thresholds.g2_max_mib);

    assert!(
        !inflated_g1.passed,
        "an inflated W-idle measurement was waived"
    );
    assert!(
        !inflated_g2.passed,
        "an inflated W-real measurement was waived"
    );
}

#[test]
fn only_a_literal_memory_target_requests_the_expensive_measurement() {
    assert!(command_requests_memory_target(&[
        "cargo".to_owned(),
        "test".to_owned(),
        "--test".to_owned(),
        "memory".to_owned(),
    ]));
    assert!(!command_requests_memory_target(&[
        "cargo".to_owned(),
        "test".to_owned(),
        "--workspace".to_owned(),
        "--offline".to_owned(),
    ]));
}

#[test]
fn two_dispatch_passes_reconstruct_the_public_pair_order() {
    let first = pass_gate_sides(0);
    let second = pass_gate_sides(1);
    assert_eq!(first.len(), 10);
    assert_eq!(second.len(), 10);

    let reconstructed: Vec<PairedSide> = first
        .chunks_exact(2)
        .chain(second.chunks_exact(2))
        .map(|pair| {
            assert_eq!(pair[0], pair[1]);
            pair[0]
        })
        .collect();
    assert_eq!(reconstructed, interleaved_pair_order(5));
}

#[test]
fn dispatcher_routes_every_launch_without_a_waiting_window() {
    let root = tempfile::tempdir().expect("temporary dispatcher directory");
    let state = root.path().join("state");
    let schedule = root.path().join("schedule");
    let log = root.path().join("order.log");
    std::fs::write(&state, b"0\n").expect("initialize dispatcher state");
    write_pass_schedule(&schedule, 0);

    let ts_actual = root.path().join("ts-actual");
    let rust_actual = root.path().join("rust-actual");
    write_logging_subject(&ts_actual, "typescript", &log);
    write_logging_subject(&rust_actual, "rust", &log);
    let wrapper = root.path().join("dispatcher");
    write_dispatcher(&wrapper, &ts_actual, &rust_actual, &state, &schedule);

    for _ in 0..11 {
        let status = Command::new(&wrapper)
            .status()
            .expect("launch dispatched subject");
        assert!(status.success(), "dispatched subject failed: {status}");
    }
    let observed = std::fs::read_to_string(&log).expect("read observed dispatch order");
    let expected = std::fs::read_to_string(&schedule).expect("read expected dispatch order");
    assert_eq!(observed, expected);
}

#[test]
fn a_completed_pass_survives_preparing_the_same_measurement_root_again() {
    let target = tempfile::tempdir().expect("temporary measurement target");
    let rust = target.path().join("zuno");
    let typescript = target.path().join("opencode");
    let database = target.path().join("opencode.db");
    std::fs::write(&rust, b"rust-v1").expect("write Rust fixture");
    std::fs::write(&typescript, b"typescript-v1").expect("write TypeScript fixture");
    std::fs::write(&database, b"database-v1").expect("write database fixture");
    let root = prepare_measurement_root(
        target.path(),
        &rust,
        &typescript,
        &database,
        AllocatorConfiguration::Jemalloc,
    );
    let completed = root.join("pass-0.json");
    std::fs::write(&completed, b"completed\n").expect("write completed-pass marker");

    let resumed = prepare_measurement_root(
        target.path(),
        &rust,
        &typescript,
        &database,
        AllocatorConfiguration::Jemalloc,
    );

    assert_eq!(resumed, root);
    assert!(
        completed.is_file(),
        "an interrupted 100-minute schedule must not discard a completed pass"
    );
}

#[test]
fn changing_a_measured_binary_invalidates_completed_passes() {
    let target = tempfile::tempdir().expect("temporary measurement target");
    let rust = target.path().join("zuno");
    let typescript = target.path().join("opencode");
    let database = target.path().join("opencode.db");
    std::fs::write(&rust, b"rust-v1").expect("write Rust fixture");
    std::fs::write(&typescript, b"typescript-v1").expect("write TypeScript fixture");
    std::fs::write(&database, b"database-v1").expect("write database fixture");
    let root = prepare_measurement_root(
        target.path(),
        &rust,
        &typescript,
        &database,
        AllocatorConfiguration::Jemalloc,
    );
    let completed = root.join("pass-0.json");
    std::fs::write(&completed, b"completed\n").expect("write completed-pass marker");

    std::fs::write(&rust, b"rust-v2").expect("replace Rust fixture");
    prepare_measurement_root(
        target.path(),
        &rust,
        &typescript,
        &database,
        AllocatorConfiguration::Jemalloc,
    );

    assert!(
        !completed.exists(),
        "a report from different executable bytes must never satisfy this gate"
    );
}

fn should_run_expensive_gate() -> bool {
    match std::env::var(MEMORY_GATE_MODE).as_deref() {
        Ok("run") => true,
        Ok("skip") => false,
        Ok(other) => {
            panic!("unsupported {MEMORY_GATE_MODE}={other:?}; accepted values are `run` and `skip`")
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("{MEMORY_GATE_MODE} must be valid UTF-8")
        }
        Err(std::env::VarError::NotPresent) => {
            parent_cargo_command().is_some_and(|args| command_requests_memory_target(&args))
        }
    }
}

fn parent_cargo_command() -> Option<Vec<String>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let parent = status.lines().find_map(|line| {
        line.strip_prefix("PPid:")
            .and_then(|value| value.trim().parse::<u32>().ok())
    })?;
    let bytes = std::fs::read(format!("/proc/{parent}/cmdline")).ok()?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

fn command_requests_memory_target(args: &[String]) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == "--test" && pair[1] == "memory")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonicalize workspace root")
}

fn target_dir(workspace: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(value) if Path::new(&value).is_absolute() => PathBuf::from(value),
        Some(value) => workspace.join(value),
        None => workspace.join("target"),
    }
}

fn prepare_measurement_root(
    target: &Path,
    rust_binary: &Path,
    typescript_binary: &Path,
    database: &Path,
    allocator: AllocatorConfiguration,
) -> PathBuf {
    let root = target.join(format!("perf/task-88-work-{}", allocator.label()));
    let context = measurement_context(rust_binary, typescript_binary, database);
    let context_path = root.join("context.json");
    if root.exists() && !std::fs::read(&context_path).is_ok_and(|existing| existing == context) {
        std::fs::remove_dir_all(&root)
            .expect("remove G1/G2 measurements from a different executable context");
    }
    std::fs::create_dir_all(&root).expect("create durable G1/G2 measurement directory");
    std::fs::write(context_path, context).expect("record G1/G2 executable context");
    root
}

fn measurement_context(rust_binary: &Path, typescript_binary: &Path, database: &Path) -> Vec<u8> {
    let database_metadata = std::fs::metadata(database).expect("read W-real database metadata");
    let gate_harness = std::env::current_exe().expect("locate memory gate test binary");
    serde_json::to_vec_pretty(&serde_json::json!({
        "gate_harness_sha256": file_sha256(&gate_harness),
        "rust_sha256": file_sha256(rust_binary),
        "typescript_sha256": file_sha256(typescript_binary),
        "database": database.canonicalize().expect("canonicalize W-real database"),
        "database_bytes": database_metadata.len(),
        "database_sha256": file_sha256(database),
    }))
    .expect("encode G1/G2 executable context")
}

fn file_sha256(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("sha256sum must be installed for resumable memory measurements");
    assert!(
        output.status.success(),
        "sha256sum failed for {}: {output:?}",
        path.display()
    );
    String::from_utf8(output.stdout)
        .expect("sha256sum output must be UTF-8")
        .split_whitespace()
        .next()
        .expect("sha256sum output must contain a digest")
        .to_owned()
}

fn build_release_subject(
    workspace: &Path,
    target: &Path,
    allocator: AllocatorConfiguration,
) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args([
            OsStr::new("build"),
            OsStr::new("--release"),
            OsStr::new("-p"),
            OsStr::new("zuno-cli"),
            OsStr::new("--bin"),
            OsStr::new("zuno"),
            OsStr::new("--offline"),
        ])
        .current_dir(workspace);
    if allocator == AllocatorConfiguration::System {
        command.arg("--no-default-features");
    }
    let status = command
        .status()
        .expect("spawn release build for the Rust subject");
    assert!(status.success(), "Rust release build failed with {status}");
    let binary = target.join("release/zuno");
    assert!(
        binary.is_file(),
        "release binary missing at {}",
        binary.display()
    );
    binary
}

fn binary_version(binary: &Path) -> String {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .expect("probe Rust release version");
    assert!(
        output.status.success(),
        "Rust version probe failed: {output:?}"
    );
    String::from_utf8(output.stdout)
        .expect("Rust version must be UTF-8")
        .trim()
        .to_owned()
}

fn database_path() -> PathBuf {
    std::env::var_os(MEMORY_GATE_DATABASE)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(W_REAL_SUBJECT.database_path))
}

/// Reject an unpinned database in seconds instead of after a 100-minute pass.
///
/// Delegates to the same comparison the capture path uses, so this fast-fail
/// cannot disagree with the check that actually guards the measurement. The
/// override names a path, never an identity: a byte-identical copy elsewhere is
/// accepted and a mutated database at the pinned path is not.
fn assert_pinned_database(database: &Path) {
    if let Err(error) = verify_pinned_database(database, &W_REAL_SUBJECT) {
        panic!("{error}\nset {MEMORY_GATE_DATABASE} to the pinned snapshot");
    }
}

fn copy_subject(source: &Path, target: &Path) {
    std::fs::create_dir_all(target.parent().expect("subject target parent"))
        .expect("create Rust subject directory");
    std::fs::copy(source, target).expect("copy Rust release binary as opencode");
}

fn write_pass_schedule(path: &Path, pass: usize) {
    let mut sides = pass_gate_sides(pass);
    sides.push(if pass == 0 {
        PairedSide::TypeScript
    } else {
        PairedSide::Rust
    });
    let body = sides
        .into_iter()
        .map(side_label)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{body}\n")).expect("write dispatcher schedule");
}

fn pass_gate_sides(pass: usize) -> Vec<PairedSide> {
    let order = interleaved_pair_order(5);
    let start = pass.checked_mul(5).expect("pass index overflow");
    let selected = order
        .get(start..start + 5)
        .expect("the paired gate has exactly two passes");
    selected.iter().flat_map(|side| [*side, *side]).collect()
}

const fn side_label(side: PairedSide) -> &'static str {
    match side {
        PairedSide::TypeScript => "typescript",
        PairedSide::Rust => "rust",
    }
}

fn write_dispatcher(path: &Path, typescript: &Path, rust: &Path, state: &Path, schedule: &Path) {
    let script = format!(
        r#"#!/bin/sh
TYPESCRIPT={typescript}
RUST={rust}
STATE={state}
SCHEDULE={schedule}

if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then
  exec "$TYPESCRIPT" "$@"
fi

step=$(/bin/cat "$STATE")
side=$(/usr/bin/sed -n "$((step + 1))p" "$SCHEDULE")
printf "%s\n" "$((step + 1))" > "$STATE"
case "$side" in
  typescript) exec "$TYPESCRIPT" "$@" ;;
  rust) exec "$RUST" "$@" ;;
  *) echo "dispatcher has no subject for step $((step + 1))" >&2; exit 200 ;;
esac
"#,
        typescript = shell_quote(typescript),
        rust = shell_quote(rust),
        state = shell_quote(state),
        schedule = shell_quote(schedule),
    );
    std::fs::write(path, script).expect("write subject dispatcher");
    make_executable(path);
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn write_logging_subject(path: &Path, side: &str, log: &Path) {
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' {} >> {}\n",
        shell_quote(Path::new(side)),
        shell_quote(log)
    );
    std::fs::write(path, body).expect("write logging subject");
    make_executable(path);
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path)
            .expect("read executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make file executable");
    }
}

fn spawn_worker(
    label: &'static str,
    test_binary: &Path,
    wrapper: &Path,
    database: &Path,
    report: &Path,
    log_path: &Path,
) -> Worker {
    let log = File::options()
        .create(true)
        .append(true)
        .open(log_path)
        .expect("open durable measurement worker log");
    let stderr = log.try_clone().expect("clone measurement worker log");
    let child = Command::new(test_binary)
        .args([
            "--exact",
            "frozen_measurement_worker",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MEMORY_GATE_WORKER_OUTPUT, report)
        .env("ZUNO_TESTKIT_ORACLE", wrapper)
        .env("ZUNO_DB", database)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn frozen measurement worker");
    Worker {
        label,
        child,
        log: log_path.to_path_buf(),
    }
}

fn wait_for_worker(mut worker: Worker) -> io::Result<()> {
    let started = Instant::now();
    loop {
        if let Some(status) = worker.child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(worker_failure(&worker, status));
        }
        if started.elapsed() >= WORKER_TIMEOUT {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} G1/G2 pass exceeded two hours", worker.label),
            ));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn worker_failure(worker: &Worker, status: std::process::ExitStatus) -> io::Error {
    let log = std::fs::read_to_string(&worker.log)
        .unwrap_or_else(|error| format!("<worker log unreadable: {error}>"));
    io::Error::other(format!(
        "{} measurement worker failed with {status}\n{}\nprogress: {}",
        worker.label,
        log,
        worker.log.with_extension("state").display()
    ))
}

fn assert_dispatch_count(state: &Path) {
    let launched = std::fs::read_to_string(state)
        .expect("read dispatcher state")
        .trim()
        .parse::<usize>()
        .expect("dispatcher state must be numeric");
    assert_eq!(
        launched, 11,
        "each pass must launch ten G1/G2 runs plus its runner-owned W-soak smoke"
    );
}

fn split_subject_runs(
    first: &BaselineReport,
    second: &BaselineReport,
) -> (SubjectRuns, SubjectRuns) {
    let (ts_idle, rust_idle) = split_workload(first, second, WorkloadName::WIdle);
    let (ts_real, rust_real) = split_workload(first, second, WorkloadName::WReal);
    (
        SubjectRuns {
            idle: ts_idle,
            real: ts_real,
        },
        SubjectRuns {
            idle: rust_idle,
            real: rust_real,
        },
    )
}

fn split_workload(
    first: &BaselineReport,
    second: &BaselineReport,
    workload: WorkloadName,
) -> (Vec<RunMeasurement>, Vec<RunMeasurement>) {
    let first_runs = &first.workload(workload).expect("first workload").runs;
    let second_runs = &second.workload(workload).expect("second workload").runs;
    assert_eq!(first_runs.len(), 5, "first {workload:?} pass");
    assert_eq!(second_runs.len(), 5, "second {workload:?} pass");
    let mut typescript = Vec::with_capacity(5);
    let mut rust = Vec::with_capacity(5);
    for (side, run) in interleaved_pair_order(5)
        .into_iter()
        .zip(first_runs.iter().chain(second_runs))
    {
        let destination = match side {
            PairedSide::TypeScript => &mut typescript,
            PairedSide::Rust => &mut rust,
        };
        let mut run = run.clone();
        run.repetition = destination.len() + 1;
        destination.push(run);
    }
    assert_eq!(typescript.len(), 5, "TypeScript {workload:?} repetitions");
    assert_eq!(rust.len(), 5, "Rust {workload:?} repetitions");
    (typescript, rust)
}

fn median_of_runs(runs: &[RunMeasurement]) -> u64 {
    assert_eq!(runs.len(), 5, "a frozen gate median requires five runs");
    let mut peaks: Vec<u64> = runs.iter().map(|run| run.peak_rss_kib).collect();
    peaks.sort_unstable();
    peaks[2]
}

fn baseline_median(report: &BaselineReport, workload: WorkloadName) -> u64 {
    report
        .workload(workload)
        .expect("frozen workload must exist")
        .median_peak_rss_kib
        .expect("G1/G2 workload must retain a median")
}

fn assert_same_machine(left: &BaselineReport, right: &BaselineReport) {
    assert_eq!(left.machine.kernel, right.machine.kernel, "kernel drift");
    assert_eq!(left.machine.hostname, right.machine.hostname, "host drift");
    assert_eq!(left.machine.cpu_model, right.machine.cpu_model, "CPU drift");
    assert_eq!(
        left.machine.logical_cpus, right.machine.logical_cpus,
        "CPU visibility drift"
    );
    assert_eq!(left.machine.ram_kib, right.machine.ram_kib, "RAM drift");
}

/// Compare a report's recorded subject against the pin, naming both on mismatch.
///
/// Applied to the committed baseline as well as to each measured pass, because
/// G2's ceiling is `0.50 x` the TypeScript median **for the pinned subject**: a
/// report about any other session is not comparable to it at all.
fn assert_w_real_provenance(label: &str, report: &BaselineReport) {
    let real = report
        .workload(WorkloadName::WReal)
        .expect("W-real must exist");
    let found = (
        real.session_id.as_deref(),
        real.message_count,
        real.part_count,
        real.part_data_bytes,
    );
    let expected = (
        Some(W_REAL_SUBJECT.session_id),
        Some(W_REAL_SUBJECT.message_count),
        Some(W_REAL_SUBJECT.part_count),
        Some(W_REAL_SUBJECT.part_data_bytes),
    );
    assert_eq!(
        found, expected,
        "{label} records a W-real subject the pin does not describe; expected {expected:?}, \
         found {found:?}. {W_REAL_RECAPTURE}"
    );
}

#[allow(clippy::too_many_arguments)]
fn write_gate_artifact(
    target: &Path,
    baseline: &BaselineReport,
    first: &BaselineReport,
    second: &BaselineReport,
    paired_ts: &SubjectRuns,
    rust: &SubjectRuns,
    g1: GateVerdict,
    g2: GateVerdict,
    rust_binary: &Path,
    rust_version: &str,
    database: &Path,
    allocator: AllocatorConfiguration,
) {
    let directory = target.join("perf");
    std::fs::create_dir_all(&directory).expect("create performance artifact directory");
    let path = directory.join(format!("task-88-memory-{}.json", allocator.label()));
    let gate = |verdict: GateVerdict, paired_ts_median: u64| {
        serde_json::json!({
            "rust_median_peak_rss_kib": verdict.rust_median_kib,
            "committed_typescript_median_peak_rss_kib": verdict.baseline_median_kib,
            "paired_typescript_median_peak_rss_kib": paired_ts_median,
            "rust_ceiling_rss_kib": verdict.ceiling_kib(),
            "rust_to_committed_typescript_ratio": verdict.ratio,
            "rust_to_paired_typescript_ratio": verdict.rust_median_kib as f64 / paired_ts_median as f64,
            "paired_to_committed_typescript_ratio": paired_ts_median as f64 / verdict.baseline_median_kib as f64,
            "passed": verdict.passed,
        })
    };
    let subject = |runs: &SubjectRuns| {
        serde_json::json!({
            "w_idle_runs": runs.idle,
            "w_idle_median_peak_rss_kib": runs.median(WorkloadName::WIdle),
            "w_real_runs": runs.real,
            "w_real_median_peak_rss_kib": runs.median(WorkloadName::WReal),
        })
    };
    let artifact = serde_json::json!({
        "topology": "released TypeScript TUI vs release-profile Rust TUI under the frozen real PTY runner",
        "pairing": "five AB/BA pairs per workload from interleaved_pair_order(5)",
        "rust_allocator": allocator.label(),
        "rust_binary": rust_binary,
        "rust_version": rust_version,
        "w_real_database": database,
        "w_real_provenance": {
            "session_id": W_REAL_SUBJECT.session_id,
            "message_count": W_REAL_SUBJECT.message_count,
            "part_count": W_REAL_SUBJECT.part_count,
            "part_data_bytes": W_REAL_SUBJECT.part_data_bytes,
            "pinned_database_path": W_REAL_SUBJECT.database_path,
            "pinned_database_bytes": W_REAL_SUBJECT.database_bytes,
            "pinned_database_sha256": W_REAL_SUBJECT.database_sha256,
        },
        "g1": gate(g1, paired_ts.median(WorkloadName::WIdle)),
        "g2": gate(g2, paired_ts.median(WorkloadName::WReal)),
        "committed_typescript_baseline": baseline,
        "raw_frozen_passes": [first, second],
        "paired_typescript_measurement": subject(paired_ts),
        "rust_measurement": subject(rust),
    });
    let bytes = serde_json::to_vec_pretty(&artifact).expect("encode G1/G2 artifact");
    let mut file = File::create(&path).expect("create G1/G2 artifact");
    file.write_all(&bytes).expect("write G1/G2 artifact");
    file.write_all(b"\n").expect("terminate G1/G2 artifact");
}
