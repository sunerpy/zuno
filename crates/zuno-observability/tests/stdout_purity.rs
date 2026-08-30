use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const LOG_ONLY_MARKERS: &[&str] = &[
    "probe-trace",
    "probe-debug",
    "probe-info",
    "probe-warn",
    "probe-error",
    "probe-provider",
    "TOOL_LIFECYCLE",
    "toolu_probe",
];

#[derive(Debug)]
struct Record {
    level: String,
    message: Option<String>,
    fields: Value,
    spans: Value,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    process_uuid: String,
    pid: u32,
}

impl Record {
    fn contains(&self, needle: &str) -> bool {
        self.message
            .as_deref()
            .is_some_and(|value| value.contains(needle))
            || self.fields.to_string().contains(needle)
            || self.spans.to_string().contains(needle)
    }
}

struct ProbeRun {
    output: Output,
    records: Vec<Record>,
    database_path: PathBuf,
    plaintext_path: Option<PathBuf>,
    plaintext: String,
    _directory: TempDir,
}

impl ProbeRun {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn frames(&self) -> Vec<Value> {
        frames(&self.output.stdout)
    }

    fn frame(&self, method: &str) -> Value {
        self.frames()
            .into_iter()
            .find(|frame| frame.get("method").and_then(Value::as_str) == Some(method))
            .unwrap_or_else(|| panic!("missing frame {method:?} in stdout:\n{}", self.stdout()))
    }

    fn has(&self, marker: &str) -> bool {
        self.records.iter().any(|record| record.contains(marker))
    }
}

fn command(log_dir: &Path, env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zuno-log-probe"));
    command
        .env_remove("RUST_LOG")
        .env_remove("ZUNO_LOG_LEVEL")
        .env_remove("ZUNO_PRINT_LOGS")
        .env_remove("ZUNO_PLAINTEXT_LOGS")
        .env_remove("ZUNO_PROBE_DIRECTIVES")
        .env_remove("ZUNO_PROBE_PLAINTEXT")
        .env("ZUNO_PROBE_LOG_DIR", log_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

fn run_probe(env: &[(&str, &str)]) -> ProbeRun {
    let directory = TempDir::new().expect("temporary log root");
    let log_dir = directory.path().join("log");
    let output = command(&log_dir, env).output().expect("probe runs");
    let parsed = frames(&output.stdout);
    let ready = parsed
        .iter()
        .find(|frame| frame.get("method").and_then(Value::as_str) == Some("probe/ready"));
    let database_path = ready
        .and_then(|frame| frame.pointer("/params/database"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| log_dir.join(zuno_observability::STRUCTURED_LOG_FILE));
    let plaintext_path = ready
        .and_then(|frame| frame.pointer("/params/plaintext"))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let records = read_records(&database_path);
    let plaintext = plaintext_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    ProbeRun {
        output,
        records,
        database_path,
        plaintext_path,
        plaintext,
        _directory: directory,
    }
}

fn frames(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout is not framed JSON ({error}): {line:?}"))
        })
        .collect()
}

fn read_records(path: &Path) -> Vec<Record> {
    let Ok(connection) = Connection::open(path) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT level, message, fields_json, spans_json, session_id, turn_id,
                tool_call_id, process_uuid, pid
         FROM log_record ORDER BY id",
    ) else {
        return Vec::new();
    };
    statement
        .query_map([], |row| {
            let fields: String = row.get(2)?;
            let spans: String = row.get(3)?;
            Ok(Record {
                level: row.get(0)?,
                message: row.get(1)?,
                fields: serde_json::from_str(&fields).unwrap_or(Value::Null),
                spans: serde_json::from_str(&spans).unwrap_or(Value::Null),
                session_id: row.get(4)?,
                turn_id: row.get(5)?,
                tool_call_id: row.get(6)?,
                process_uuid: row.get(7)?,
                pid: row.get(8)?,
            })
        })
        .expect("query log records")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode log records")
}

fn assert_stdout_is_pure(run: &ProbeRun) {
    let stdout = run.stdout();
    for marker in LOG_ONLY_MARKERS {
        assert!(
            !stdout.contains(marker),
            "log marker {marker:?} reached stdout:\n{stdout}"
        );
    }
    assert!(
        run.frames().len() >= 5,
        "missing protocol frames:\n{stdout}"
    );
}

#[test]
fn every_level_reaches_the_structured_store_and_never_stdout() {
    let run = run_probe(&[("ZUNO_PROBE_DIRECTIVES", "trace")]);
    assert!(
        run.output.status.success(),
        "probe failed:\n{}",
        run.stderr()
    );
    assert_stdout_is_pure(&run);
    for (level, marker) in [
        ("TRACE", "probe-trace"),
        ("DEBUG", "probe-debug"),
        ("INFO", "probe-info"),
        ("WARN", "probe-warn"),
        ("ERROR", "probe-error"),
    ] {
        assert!(run.has(marker), "{marker} is absent from structured logs");
        assert!(
            run.records
                .iter()
                .any(|record| record.level == level && record.contains(marker)),
            "{marker} was not stored at {level}"
        );
    }
}

#[test]
fn info_is_default_and_trace_is_a_supported_environment_level() {
    let default = run_probe(&[]);
    assert!(default.output.status.success(), "{}", default.stderr());
    assert!(default.has("probe-info"));
    assert!(!default.has("probe-debug"));
    assert!(!default.has("probe-trace"));

    let trace = run_probe(&[("ZUNO_LOG_LEVEL", "trace")]);
    assert!(trace.output.status.success(), "{}", trace.stderr());
    assert_eq!(
        trace.frame("probe/ready")["params"]["level"].as_str(),
        Some("TRACE")
    );
    assert!(trace.has("probe-trace"));

    let invalid = run_probe(&[("ZUNO_LOG_LEVEL", "verbose")]);
    assert!(invalid.output.status.success(), "{}", invalid.stderr());
    assert_eq!(
        invalid.frame("probe/ready")["params"]["level"].as_str(),
        Some("INFO")
    );
}

#[test]
fn rust_log_controls_target_aware_filtering() {
    let run = run_probe(&[("RUST_LOG", "zuno_log_probe=trace,zuno_observability=warn")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert!(run.has("probe-trace"));
    assert!(
        !run.records
            .iter()
            .any(|record| record.contains("process.started")),
        "the zuno_observability target should be filtered below WARN"
    );
}

#[test]
fn zuno_log_level_takes_precedence_over_rust_log() {
    let run = run_probe(&[("ZUNO_LOG_LEVEL", "DEBUG"), ("RUST_LOG", "error")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert!(run.has("probe-debug"));
    assert!(run.has("probe-info"));
}

#[test]
fn stderr_is_additive_and_stdout_remains_protocol_only() {
    let run = run_probe(&[("ZUNO_PRINT_LOGS", "1")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert_stdout_is_pure(&run);
    assert!(run.stderr().contains("probe emitted at info"));
    assert!(run.has("probe-info"));

    let rejected = run_probe(&[("ZUNO_PRINT_LOGS", "true")]);
    assert!(rejected.output.status.success(), "{}", rejected.stderr());
    assert!(!rejected.stderr().contains("probe emitted at info"));
}

#[test]
fn a_second_init_is_quiet_and_does_not_replace_the_store() {
    let run = run_probe(&[]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert_eq!(
        run.frame("probe/ready")["params"]["installed"].as_bool(),
        Some(true)
    );
    assert_eq!(
        run.frame("probe/second-init")["params"]["installed"].as_bool(),
        Some(false)
    );
    assert!(run.has("probe-info"));
}

#[test]
fn provider_records_carry_session_turn_and_attempt_context() {
    let run = run_probe(&[("ZUNO_LOG_LEVEL", "DEBUG")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    let record = run
        .records
        .iter()
        .find(|record| record.contains("probe-provider-finished"))
        .expect("provider completion record");
    assert_eq!(record.session_id.as_deref(), Some("ses_probe"));
    assert_eq!(record.turn_id.as_deref(), Some("turn_probe"));
    let spans = record.spans.to_string();
    for expected in [
        "\"turn\"",
        "\"provider_request\"",
        "\"attempt\":1",
        "\"provider\":\"anthropic\"",
        "\"outcome\":\"completed\"",
    ] {
        assert!(
            spans.contains(expected),
            "provider record lacks {expected}: {spans}"
        );
    }
}

#[test]
fn tool_lifecycle_is_correlated_and_terminal() {
    let run = run_probe(&[("ZUNO_LOG_LEVEL", "DEBUG")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    for phase in [
        "pending",
        "running",
        "completed",
        "blocked",
        "error",
        "abandoned",
    ] {
        assert!(
            run.records
                .iter()
                .any(|record| { record.contains("TOOL_LIFECYCLE") && record.contains(phase) }),
            "missing tool phase {phase}"
        );
    }
    assert!(
        run.records.iter().any(|record| {
            record.tool_call_id.as_deref() == Some("toolu_probe") && record.contains("completed")
        }),
        "tool span did not populate tool_call_id"
    );
}

#[test]
fn sensitive_fields_are_redacted_before_persistence() {
    let run = run_probe(&[]);
    assert!(run.output.status.success(), "{}", run.stderr());
    let record = run
        .records
        .iter()
        .find(|record| record.contains("probe-sensitive"))
        .expect("sensitive-field record");
    assert_eq!(record.fields["command"].as_str(), Some("[redacted]"));
    let database_bytes = std::fs::read(&run.database_path).expect("read database");
    assert!(
        !database_bytes
            .windows(b"never-store-this-command".len())
            .any(|window| window == b"never-store-this-command"),
        "the raw command reached the SQLite file"
    );
}

#[test]
fn plaintext_is_opt_in_process_specific_and_private() {
    let default = run_probe(&[]);
    assert!(default.output.status.success(), "{}", default.stderr());
    assert!(default.plaintext_path.is_none());

    let enabled = run_probe(&[("ZUNO_PROBE_PLAINTEXT", "1")]);
    assert!(enabled.output.status.success(), "{}", enabled.stderr());
    let path = enabled.plaintext_path.as_ref().expect("plaintext path");
    assert!(enabled.plaintext.contains("probe emitted at info"));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    assert!(name.starts_with("zuno."));
    assert!(name.ends_with(".log"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        assert_eq!(
            std::fs::metadata(path)
                .expect("plaintext metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&enabled.database_path)
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn concurrent_processes_share_sqlite_without_sharing_process_identity() {
    let directory = TempDir::new().expect("temporary log root");
    let log_dir = directory.path().join("log");
    let children = (0..12)
        .map(|_| command(&log_dir, &[]).spawn().expect("spawn probe"))
        .collect::<Vec<_>>();
    for child in children {
        let output = child.wait_with_output().expect("wait for probe");
        assert!(
            output.status.success(),
            "concurrent probe failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            serde_json::from_str::<Value>(line).expect("stdout frame remains JSON");
        }
    }

    let records = read_records(&log_dir.join(zuno_observability::STRUCTURED_LOG_FILE));
    let processes = records
        .iter()
        .filter(|record| record.contains("probe-info"))
        .map(|record| (record.process_uuid.clone(), record.pid))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        processes.len(),
        12,
        "expected one correlated process identity per probe: {processes:?}"
    );
}

#[test]
fn an_unusable_log_directory_fails_loudly_without_corrupting_stdout() {
    let directory = TempDir::new().expect("temporary root");
    let blocker = directory.path().join("log");
    std::fs::write(&blocker, b"not a directory").expect("write blocker");
    let output = command(&blocker, &[]).output().expect("probe runs");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&blocker.to_string_lossy().into_owned())
    );
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        serde_json::from_str::<Value>(line).expect("stdout remains framed JSON");
    }
}

#[test]
fn the_probe_refuses_to_run_without_a_log_directory() {
    let output = Command::new(env!("CARGO_BIN_EXE_zuno-log-probe"))
        .env_remove("ZUNO_PROBE_LOG_DIR")
        .output()
        .expect("probe runs");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ZUNO_PROBE_LOG_DIR"));
}
