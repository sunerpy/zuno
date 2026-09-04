use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

/// Raw values the probe emits under a sensitive field name. No sink may contain
/// them: not the SQLite store, not the plaintext file, not stderr.
const SENSITIVE_LITERALS: &[&str] = &[
    "never-store-this-command",
    "never-print-this-prompt",
    "never-print-this-report",
    // Raw subprocess output, under the field name a live emitter outside this crate
    // already uses at WARN.
    "never-print-this-git-stderr",
    // Names `docs/logging.md` promises are redacted.
    "never-print-this-bearer-token",
    "never-print-this-credential",
    // The payload word is not the last component of the field name.
    "never-print-this-prefix-prompt",
    "never-print-this-command-line",
    // The payload word is only visible after a camelCase split.
    "never-print-this-camel-token",
    // Plural spellings of a documented class: the natural Rust name for a collection.
    "never-print-this-cookie-jar",
    "never-print-this-argv",
    "never-print-this-outputs",
    // An MCP server's stderr line, with the field names zuno-mcp's stderr drain emits:
    // the ordinary line and the line truncated at the drain's byte bound.
    "sk-live-abc123",
    "sk-live-truncated-def456",
];

/// The placeholder is operator-visible output, so its spelling is pinned here once.
#[test]
fn the_redaction_placeholder_spelling_is_pinned() {
    assert_eq!(zuno_observability::REDACTED, "[redacted]");
}

const LOG_ONLY_MARKERS: &[&str] = &[
    "probe-trace",
    "probe-debug",
    "probe-info",
    "probe-warn",
    "probe-error",
    "probe-provider",
    "probe-subprocess",
    "probe-credential",
    "probe-plural",
    "probe-mcp-stderr",
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

    fn records_text(&self) -> String {
        self.records
            .iter()
            .map(|record| format!("{record:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The rendering a text sink produces for a scrubbed field. Built from the crate's
/// own constant, so changing the placeholder does not turn every assertion below into
/// an unexplained string mismatch.
fn redacted(field: &str) -> String {
    format!("{field}=\"{}\"", zuno_observability::REDACTED)
}

/// Fails if any raw sensitive value survived into `text`.
///
/// A substring scan rather than a field lookup, because the sinks under test render
/// fields as free text and the question is only whether the bytes are there.
fn assert_no_sensitive_literal(sink: &str, text: &str) {
    for literal in SENSITIVE_LITERALS {
        assert!(
            !text.contains(literal),
            "{literal:?} reached {sink} unredacted:\n{text}"
        );
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
    assert_no_sensitive_literal("stderr", &run.stderr());
    assert!(
        run.stderr().contains(&redacted("command")),
        "stderr never rendered the redacted field:\n{}",
        run.stderr()
    );

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
    assert_eq!(
        record.fields["command"].as_str(),
        Some(zuno_observability::REDACTED)
    );
    assert_database_has_no_sensitive_literal(&run.database_path);
}

/// One run with all three sinks live. Redaction is a property of the record, not of
/// the sink that happens to be enabled, so the same emission has to come out
/// scrubbed in SQLite, in the plaintext file, and on stderr.
#[test]
fn every_enabled_sink_redacts_the_same_record() {
    let run = run_probe(&[("ZUNO_PROBE_PLAINTEXT", "1"), ("ZUNO_PRINT_LOGS", "1")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert_stdout_is_pure(&run);

    // Each sink has to have received the emission, or absence proves nothing.
    assert!(run.has("probe-sensitive"), "the store missed the emission");
    assert!(
        run.has("probe-span-sensitive-event"),
        "the store missed the span emission"
    );
    assert!(
        run.plaintext.contains("probe-span-sensitive-event"),
        "the plaintext file missed the emission:\n{}",
        run.plaintext
    );
    assert!(
        run.stderr().contains("probe-span-sensitive-event"),
        "stderr missed the emission:\n{}",
        run.stderr()
    );

    assert_no_sensitive_literal("the plaintext file", &run.plaintext);
    assert_no_sensitive_literal("stderr", &run.stderr());
    assert_no_sensitive_literal("the decoded SQLite records", &run.records_text());
    assert_database_has_no_sensitive_literal(&run.database_path);

    for sink in [&run.plaintext, &run.stderr()] {
        assert!(
            sink.contains(&redacted("prompt")) && sink.contains(&redacted("report")),
            "a text sink dropped the sensitive span fields instead of redacting them:\n{sink}"
        );
    }
}

/// The classification, not just the plumbing. A payload can arrive under a name that
/// spells the sensitive word anywhere — a raw subprocess stream, a credential name
/// `docs/logging.md` promises is scrubbed, a prefix compound, or a camelCase word —
/// and the bounded measurement that exists so the payload never has to be logged has
/// to survive in the clear.
#[test]
fn a_subprocess_stream_and_a_credential_named_field_are_redacted_in_every_sink() {
    let run = run_probe(&[("ZUNO_PROBE_PLAINTEXT", "1"), ("ZUNO_PRINT_LOGS", "1")]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert_stdout_is_pure(&run);

    // Absence has to mean redaction rather than a missing emission.
    for marker in ["probe-subprocess", "probe-credential", "probe-plural"] {
        assert!(run.has(marker), "the store missed {marker}");
        assert!(
            run.plaintext.contains(marker),
            "the plaintext file missed {marker}:\n{}",
            run.plaintext
        );
        assert!(
            run.stderr().contains(marker),
            "stderr missed {marker}:\n{}",
            run.stderr()
        );
    }

    assert_no_sensitive_literal("the plaintext file", &run.plaintext);
    assert_no_sensitive_literal("stderr", &run.stderr());
    assert_no_sensitive_literal("the decoded SQLite records", &run.records_text());
    assert_database_has_no_sensitive_literal(&run.database_path);

    for sink in [&run.plaintext, &run.stderr()] {
        for field in [
            "stderr",
            "token",
            "credential",
            "prompt_text",
            "command_line",
            "accessToken",
            "cookies",
            "commands",
            "outputs",
        ] {
            let expected = redacted(field);
            assert!(
                sink.contains(&expected),
                "a text sink dropped the field instead of rendering {expected}:\n{sink}"
            );
        }
        assert!(
            sink.contains("stdout_bytes=12"),
            "a bounded measurement of a sensitive value was redacted too:\n{sink}"
        );
        assert!(
            sink.contains("prompt_tokens=1024"),
            "the documented token-count carve-out was redacted too:\n{sink}"
        );
        assert!(
            sink.contains("hash=\"0f1e2d3c\""),
            "an unrelated diagnostic field was redacted:\n{sink}"
        );
    }

    let record = run
        .records
        .iter()
        .find(|record| record.contains("probe-credential"))
        .expect("credential-named record");
    assert_eq!(
        record.fields["token"].as_str(),
        Some(zuno_observability::REDACTED)
    );
    assert_eq!(
        record.fields["credential"].as_str(),
        Some(zuno_observability::REDACTED)
    );
    assert_eq!(
        record.fields["accessToken"].as_str(),
        Some(zuno_observability::REDACTED)
    );
    assert_eq!(record.fields["stdout_bytes"].as_u64(), Some(12));

    // The SQLite sink is the third sink, and the plural spellings have to reach it
    // redacted too. `logs.sqlite` is where an operator looks after `--print-logs`.
    let record = run
        .records
        .iter()
        .find(|record| record.contains("probe-plural"))
        .expect("plural-named record");
    for field in ["cookies", "commands", "outputs"] {
        assert_eq!(
            record.fields[field].as_str(),
            Some(zuno_observability::REDACTED),
            "{field} was stored unredacted: {}",
            record.fields
        );
    }
    assert_eq!(record.fields["prompt_tokens"].as_u64(), Some(1024));
}

/// An MCP server's stderr is a peer-controlled stream, and the drain in
/// `crates/zuno-mcp/src/stdio.rs` logs every line of it at DEBUG. The probe emits the
/// drain's two events with the drain's field names, level, and event text — the `bytes`
/// and `limit` values are the probe's own, production truncates at 8 KiB — and a
/// secret-looking line as the payload. Redaction is by field name, so this is the pin
/// for the name the drain chose: `stderr`, which every sink renders as `[redacted]`
/// while `server`, `bytes`, `limit`, and `truncated` stay readable.
///
/// Measured before the drain renamed its field: recorded as `message`, the same line
/// rendered as `DEBUG …: MCP server stderr server=probe-mcp Traceback: API_KEY=sk-live-abc123`
/// in the plaintext file, on `--print-logs` stderr, and in the `message` column of
/// `logs.sqlite`, reading as the event's own sentence.
#[test]
fn an_mcp_server_stderr_line_is_redacted_in_every_sink() {
    let run = run_probe(&[
        ("ZUNO_LOG_LEVEL", "DEBUG"),
        ("ZUNO_PROBE_PLAINTEXT", "1"),
        ("ZUNO_PRINT_LOGS", "1"),
    ]);
    assert!(run.output.status.success(), "{}", run.stderr());
    assert_stdout_is_pure(&run);

    // Absence has to mean redaction rather than a missing emission: both drain events
    // reach all three sinks.
    let stored = run
        .records
        .iter()
        .filter(|record| record.contains("probe-mcp-stderr"))
        .collect::<Vec<_>>();
    assert_eq!(
        stored.len(),
        2,
        "the store missed an MCP stderr emission:\n{}",
        run.records_text()
    );
    for sink_name in ["MCP server stderr", "exceeded its bound and was truncated"] {
        assert!(
            run.plaintext.contains(sink_name),
            "the plaintext file missed {sink_name:?}:\n{}",
            run.plaintext
        );
        assert!(
            run.stderr().contains(sink_name),
            "stderr missed {sink_name:?}:\n{}",
            run.stderr()
        );
    }

    assert_no_sensitive_literal("the plaintext file", &run.plaintext);
    assert_no_sensitive_literal("stderr", &run.stderr());
    assert_no_sensitive_literal("the decoded SQLite records", &run.records_text());
    assert_database_has_no_sensitive_literal(&run.database_path);

    for sink in [&run.plaintext, &run.stderr()] {
        let lines = sink
            .lines()
            .filter(|line| line.contains("probe-mcp-stderr"))
            .collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            2,
            "a text sink missed one of the two drain events:\n{sink}"
        );
        for line in &lines {
            assert!(
                line.contains(&redacted("stderr")),
                "a text sink dropped the peer's line instead of rendering {}:\n{line}",
                redacted("stderr")
            );
            assert!(
                line.contains("server=probe-mcp"),
                "the server name of the same event was redacted too:\n{line}"
            );
        }
        let truncated = lines
            .iter()
            .find(|line| line.contains("was truncated"))
            .expect("the truncated drain event is one of the two lines");
        for readable in ["bytes=65536", "limit=65536", "truncated=true"] {
            assert!(
                truncated.contains(readable),
                "a bounded diagnostic of the same event was redacted too ({readable}):\n{truncated}"
            );
        }
    }

    for record in stored {
        assert_eq!(
            record.level, "DEBUG",
            "the drain logs at DEBUG; the probe must too, or the level filter is not the \
             one an operator runs with: {record:?}"
        );
        assert_eq!(
            record.fields["stderr"].as_str(),
            Some(zuno_observability::REDACTED),
            "the peer's line was stored unredacted or under another name: {}",
            record.fields
        );
        assert_eq!(record.fields["server"].as_str(), Some("probe-mcp"));
        assert!(
            record
                .message
                .as_deref()
                .is_some_and(|text| text.starts_with("MCP server stderr")),
            "the event text must be the drain's own literal: {:?}",
            record.message
        );
    }
}

/// The file bytes, not the decoded rows: a value could survive in a WAL page or in a
/// column the row decoder does not read back.
fn assert_database_has_no_sensitive_literal(path: &Path) {
    let bytes = std::fs::read(path).expect("read database");
    for literal in SENSITIVE_LITERALS {
        assert!(
            !bytes
                .windows(literal.len())
                .any(|window| window == literal.as_bytes()),
            "{literal:?} reached the SQLite file"
        );
    }
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
    assert_no_sensitive_literal("the plaintext file", &enabled.plaintext);
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
