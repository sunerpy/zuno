//! Bounded, process-correlated operational logs in a dedicated SQLite database.
//!
//! The session database is the durable source of model-visible history. This store
//! has a different job: low-volume operational diagnosis across several concurrent
//! Zuno processes. Keeping it separate means log retention can be aggressive without
//! rewriting a session and SQLite WAL can arbitrate writers instead of several
//! processes appending and pruning the same text file.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, params};
use serde_json::{Map, Number, Value, json};
use tracing::field::{Field, Visit};
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

use crate::redact::Redacting;

pub const STRUCTURED_LOG_FILE: &str = "logs.sqlite";
pub const DEFAULT_MAX_RECORDS: usize = 50_000;
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_MAX_AGE_DAYS: u64 = 10;

const QUEUE_CAPACITY: usize = 8_192;
const BATCH_CAPACITY: usize = 128;
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const INITIALIZATION_RETRY_WINDOW: Duration = Duration::from_secs(5);
const INITIALIZATION_RETRY_MIN_DELAY: Duration = Duration::from_millis(5);
const INITIALIZATION_RETRY_MAX_DELAY: Duration = Duration::from_millis(100);
const CHECKPOINT_EVERY_BATCHES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_FIELD_BYTES: usize = 8 * 1024;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;

CREATE TABLE IF NOT EXISTS log_process (
    process_uuid TEXT PRIMARY KEY,
    pid INTEGER NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    executable TEXT,
    version TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS log_record (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp_ms INTEGER NOT NULL,
    timestamp_nanos INTEGER NOT NULL,
    level TEXT NOT NULL,
    target TEXT NOT NULL,
    message TEXT,
    fields_json TEXT NOT NULL,
    spans_json TEXT NOT NULL,
    session_id TEXT,
    turn_id TEXT,
    tool_call_id TEXT,
    process_uuid TEXT NOT NULL,
    pid INTEGER NOT NULL,
    thread_name TEXT,
    source_file TEXT,
    source_line INTEGER,
    estimated_bytes INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS log_record_timestamp
    ON log_record(timestamp_ms, id);
CREATE INDEX IF NOT EXISTS log_record_session
    ON log_record(session_id, timestamp_ms, id);
CREATE INDEX IF NOT EXISTS log_record_process
    ON log_record(process_uuid, timestamp_ms, id);
";

#[derive(Clone, Debug)]
pub(crate) struct ProcessIdentity {
    pub(crate) uuid: String,
    pub(crate) pid: u32,
    pub(crate) started_at_ms: i64,
}

impl ProcessIdentity {
    pub(crate) fn new() -> Self {
        Self {
            uuid: Uuid::new_v4().simple().to_string(),
            pid: std::process::id(),
            started_at_ms: unix_millis(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RetentionPolicy {
    pub(crate) max_records: usize,
    pub(crate) max_bytes: usize,
    pub(crate) max_age: Duration,
}

#[derive(Debug)]
pub(crate) struct StoreRuntime {
    pub(crate) layer: StructuredLogLayer,
    pub(crate) guard: StoreGuard,
    pub(crate) dropped: Arc<AtomicUsize>,
    pub(crate) failures: Arc<AtomicUsize>,
}

pub(crate) fn start(
    path: PathBuf,
    identity: ProcessIdentity,
    retention: RetentionPolicy,
) -> Result<StoreRuntime, Box<dyn std::error::Error + Send + Sync>> {
    secure_file(&path)?;
    let connection = open_connection(&path, &identity)?;

    let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
    let dropped = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let worker_failures = Arc::clone(&failures);
    let worker_identity = identity.clone();
    let join = std::thread::Builder::new()
        .name("zuno-log-store".to_owned())
        .spawn(move || {
            run_worker(
                connection,
                receiver,
                &worker_identity,
                retention,
                &worker_failures,
            );
        })?;

    Ok(StoreRuntime {
        layer: StructuredLogLayer {
            sender: sender.clone(),
            dropped: Arc::clone(&dropped),
            failures: Arc::clone(&failures),
        },
        guard: StoreGuard {
            sender: Some(sender),
            join: Some(join),
        },
        dropped,
        failures,
    })
}

fn open_connection(path: &Path, identity: &ProcessIdentity) -> rusqlite::Result<Connection> {
    let deadline = Instant::now() + INITIALIZATION_RETRY_WINDOW;
    let mut delay = INITIALIZATION_RETRY_MIN_DELAY;
    loop {
        match try_open_connection(path, identity) {
            Ok(connection) => return Ok(connection),
            Err(error) if is_busy(&error) && Instant::now() < deadline => {
                let jitter = Duration::from_millis(u64::from(identity.pid % 11));
                std::thread::sleep(delay.saturating_add(jitter));
                delay = delay.saturating_mul(2).min(INITIALIZATION_RETRY_MAX_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn try_open_connection(path: &Path, identity: &ProcessIdentity) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch(SCHEMA)?;
    connection.execute(
        "INSERT OR REPLACE INTO log_process
         (process_uuid, pid, started_at_ms, ended_at_ms, executable, version)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
        params![
            identity.uuid,
            identity.pid,
            identity.started_at_ms,
            std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            env!("CARGO_PKG_VERSION"),
        ],
    )?;
    Ok(connection)
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(inner.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn secure_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().create(true).append(true).open(path)?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct StoreGuard {
    sender: Option<SyncSender<Command>>,
    join: Option<JoinHandle<()>>,
}

impl StoreGuard {
    fn shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Command::Shutdown);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for StoreGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum Command {
    Record(Box<LogRecord>),
    Shutdown,
}

#[derive(Debug)]
struct LogRecord {
    timestamp_ms: i64,
    timestamp_nanos: u32,
    level: &'static str,
    target: &'static str,
    message: Option<String>,
    fields_json: String,
    spans_json: String,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    thread_name: Option<String>,
    source_file: Option<&'static str>,
    source_line: Option<u32>,
    estimated_bytes: i64,
}

#[derive(Debug)]
pub(crate) struct StructuredLogLayer {
    sender: SyncSender<Command>,
    dropped: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
}

impl<S> Layer<S> for StructuredLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &Id,
        context: Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let visitor = FieldVisitor::record(attributes);
        span.extensions_mut().insert(SpanFields {
            name: attributes.metadata().name(),
            fields: visitor.fields,
        });
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(fields) = extensions.get_mut::<SpanFields>() else {
            return;
        };
        fields.fields.extend(FieldVisitor::record(values).fields);
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let metadata = event.metadata();
        let visitor = FieldVisitor::record(event);

        let mut span_values = Vec::new();
        let mut inherited = BTreeMap::<String, String>::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                let extensions = span.extensions();
                let Some(fields) = extensions.get::<SpanFields>() else {
                    continue;
                };
                for (key, value) in &fields.fields {
                    if let Some(value) = value.as_str() {
                        inherited.insert(key.clone(), value.to_owned());
                    }
                }
                span_values.push(json!({
                    "name": fields.name,
                    "fields": fields.fields,
                }));
            }
        }

        let session_id = context_value(
            &visitor.fields,
            &inherited,
            &["session_id", "session", "session.id"],
        );
        let turn_id = context_value(&visitor.fields, &inherited, &["turn_id"]);
        let tool_call_id = context_value(
            &visitor.fields,
            &inherited,
            &["call_id", "tool_call_id", "tool.call_id"],
        );
        let message = visitor
            .message
            .map(|message| truncate(message, MAX_MESSAGE_BYTES));
        let fields_json =
            serde_json::to_string(&visitor.fields).unwrap_or_else(|_| "{}".to_owned());
        let spans_json = serde_json::to_string(&span_values).unwrap_or_else(|_| "[]".to_owned());
        let (timestamp_ms, timestamp_nanos) = timestamp();
        let estimated_bytes = message.as_ref().map_or(0, String::len)
            + fields_json.len()
            + spans_json.len()
            + metadata.target().len()
            + 128;
        let thread_name = std::thread::current().name().map(str::to_owned);
        let record = LogRecord {
            timestamp_ms,
            timestamp_nanos,
            level: metadata.level().as_str(),
            target: metadata.target(),
            message,
            fields_json,
            spans_json,
            session_id,
            turn_id,
            tool_call_id,
            thread_name,
            source_file: metadata.file(),
            source_line: metadata.line(),
            estimated_bytes: i64::try_from(estimated_bytes).unwrap_or(i64::MAX),
        };
        match self.sender.try_send(Command::Record(Box::new(record))) {
            Ok(()) => {}
            Err(TrySendError::Full(Command::Record(_))) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(Command::Record(_))) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(Command::Shutdown))
            | Err(TrySendError::Disconnected(Command::Shutdown)) => unreachable!(),
        }
    }
}

#[derive(Debug)]
struct SpanFields {
    name: &'static str,
    fields: Map<String, Value>,
}

struct FieldVisitor {
    fields: Map<String, Value>,
    message: Option<String>,
}

impl FieldVisitor {
    /// The only way to build a visitor, so every field pass into this store runs
    /// through the shared redaction proxy whether or not its author remembered.
    fn record(fields: impl RecordFields) -> Self {
        let mut visitor = Self {
            fields: Map::new(),
            message: None,
        };
        fields.record(&mut Redacting::new(&mut visitor));
        visitor
    }

    fn insert(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            self.message = value
                .as_str()
                .map(str::to_owned)
                .or_else(|| Some(value.to_string().trim_matches('"').to_owned()));
            return;
        }
        let value = match value {
            Value::String(value) => Value::String(truncate(value, MAX_FIELD_BYTES)),
            value => value,
        };
        self.fields.insert(field.name().to_owned(), value);
    }
}

impl Visit for FieldVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(
            field,
            Number::from_f64(value).map_or(Value::Null, Value::Number),
        );
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::Number(value.into()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::String(value.to_owned()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, Value::String(format!("{value:?}")));
    }
}

fn context_value(
    event: &Map<String, Value>,
    inherited: &BTreeMap<String, String>,
    names: &[&str],
) -> Option<String> {
    names
        .iter()
        .find_map(|name| event.get(*name).and_then(Value::as_str).map(str::to_owned))
        .or_else(|| names.iter().find_map(|name| inherited.get(*name).cloned()))
}

fn run_worker(
    mut connection: Connection,
    receiver: Receiver<Command>,
    identity: &ProcessIdentity,
    retention: RetentionPolicy,
    failures: &AtomicUsize,
) {
    let mut batch = Vec::with_capacity(BATCH_CAPACITY);
    let mut batches = 0_usize;
    let mut shutdown = false;
    while !shutdown {
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(Command::Record(record)) => batch.push(*record),
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => shutdown = true,
            Err(RecvTimeoutError::Timeout) => {}
        }
        while batch.len() < BATCH_CAPACITY && !shutdown {
            match receiver.try_recv() {
                Ok(Command::Record(record)) => batch.push(*record),
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => shutdown = true,
                Err(TryRecvError::Empty) => break,
            }
        }
        if !batch.is_empty() {
            let count = batch.len();
            if write_batch(&mut connection, identity, &batch).is_err() {
                failures.fetch_add(count, Ordering::Relaxed);
            }
            batch.clear();
            batches = batches.saturating_add(1);
            if prune(&connection, retention).is_err() {
                failures.fetch_add(1, Ordering::Relaxed);
            }
            if batches.is_multiple_of(CHECKPOINT_EVERY_BATCHES)
                && connection
                    .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
                    .is_err()
            {
                failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if connection
        .execute(
            "UPDATE log_process SET ended_at_ms = ?2 WHERE process_uuid = ?1",
            params![identity.uuid, unix_millis()],
        )
        .is_err()
    {
        failures.fetch_add(1, Ordering::Relaxed);
    }
    if connection
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .is_err()
    {
        failures.fetch_add(1, Ordering::Relaxed);
    }
}

fn write_batch(
    connection: &mut Connection,
    identity: &ProcessIdentity,
    records: &[LogRecord],
) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO log_record (
                timestamp_ms, timestamp_nanos, level, target, message,
                fields_json, spans_json, session_id, turn_id, tool_call_id,
                process_uuid, pid, thread_name, source_file, source_line, estimated_bytes
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16
             )",
        )?;
        for record in records {
            insert.execute(params![
                record.timestamp_ms,
                record.timestamp_nanos,
                record.level,
                record.target,
                record.message,
                record.fields_json,
                record.spans_json,
                record.session_id,
                record.turn_id,
                record.tool_call_id,
                identity.uuid,
                identity.pid,
                record.thread_name,
                record.source_file,
                record.source_line,
                record.estimated_bytes,
            ])?;
        }
    }
    transaction.commit()
}

fn prune(connection: &Connection, retention: RetentionPolicy) -> rusqlite::Result<()> {
    let age_ms = i64::try_from(retention.max_age.as_millis()).unwrap_or(i64::MAX);
    let cutoff = unix_millis().saturating_sub(age_ms);
    connection.execute("DELETE FROM log_record WHERE timestamp_ms < ?1", [cutoff])?;

    let offset = i64::try_from(retention.max_records).unwrap_or(i64::MAX);
    connection.execute(
        "DELETE FROM log_record
         WHERE id <= (
             SELECT id FROM log_record ORDER BY id DESC LIMIT 1 OFFSET ?1
         )",
        [offset],
    )?;

    let mut total: i64 = connection.query_row(
        "SELECT COALESCE(SUM(estimated_bytes), 0) FROM log_record",
        [],
        |row| row.get(0),
    )?;
    let max_bytes = i64::try_from(retention.max_bytes).unwrap_or(i64::MAX);
    while total > max_bytes {
        let mut statement = connection
            .prepare("SELECT id, estimated_bytes FROM log_record ORDER BY id ASC LIMIT 512")?;
        let rows =
            statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
        let mut last_id = None;
        let mut removed = 0_i64;
        for row in rows {
            let (id, bytes) = row?;
            last_id = Some(id);
            removed = removed.saturating_add(bytes);
            if total.saturating_sub(removed) <= max_bytes {
                break;
            }
        }
        let Some(last_id) = last_id else {
            break;
        };
        connection.execute("DELETE FROM log_record WHERE id <= ?1", [last_id])?;
        total = total.saturating_sub(removed);
    }
    connection.execute(
        "DELETE FROM log_process
         WHERE ended_at_ms IS NOT NULL
           AND ended_at_ms < ?1
           AND NOT EXISTS (
               SELECT 1 FROM log_record
               WHERE log_record.process_uuid = log_process.process_uuid
           )",
        [cutoff],
    )?;
    Ok(())
}

fn truncate(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.saturating_sub('…'.len_utf8());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push('…');
    value
}

fn timestamp() -> (i64, u32) {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        duration.subsec_nanos(),
    )
}

fn unix_millis() -> i64 {
    timestamp().0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The needle list itself is covered in `crate::redact`. What has to be pinned
    /// here is that this layer's own field pass still goes through it, on the real
    /// `on_event` path rather than a reimplementation of it.
    #[test]
    fn the_store_layer_redacts_on_the_real_event_path() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let (sender, receiver) = sync_channel(BATCH_CAPACITY);
        let subscriber = tracing_subscriber::registry().with(StructuredLogLayer {
            sender,
            dropped: Arc::new(AtomicUsize::new(0)),
            failures: Arc::new(AtomicUsize::new(0)),
        });
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("probe", prompt = "never-store-this-prompt");
            let _entered = span.enter();
            tracing::info!(
                command = "never-store-this-command",
                command_bytes = 24,
                "sensitive event"
            );
        });

        let records = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|command| match command {
                Command::Record(record) => *record,
                Command::Shutdown => panic!("the layer never sends Shutdown"),
            })
            .collect::<Vec<_>>();
        let record = records
            .iter()
            .find(|record| record.message.as_deref() == Some("sensitive event"))
            .expect("the sensitive event reached the store queue");
        assert!(
            record
                .fields_json
                .contains(&format!(r#""command":"{}""#, crate::redact::REDACTED)),
            "{}",
            record.fields_json
        );
        assert!(record.fields_json.contains(r#""command_bytes":24"#));
        assert!(
            !record.fields_json.contains("never-store-this-command"),
            "{}",
            record.fields_json
        );
        assert!(
            !record.spans_json.contains("never-store-this-prompt"),
            "{}",
            record.spans_json
        );
    }

    #[test]
    fn truncation_keeps_utf8_valid() {
        let value = "数".repeat(4_000);
        let truncated = truncate(value, 101);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.ends_with('…'));
        assert!(truncated.len() <= 101);
    }

    #[test]
    fn sqlite_lock_contention_is_the_only_retryable_initialization_failure() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_owned()),
        );
        let constraint = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("constraint failed".to_owned()),
        );
        assert!(is_busy(&busy));
        assert!(!is_busy(&constraint));
    }

    #[test]
    fn retention_enforces_record_and_byte_bounds() {
        let connection = populated(10, 100, unix_millis());
        prune(
            &connection,
            RetentionPolicy {
                max_records: 3,
                max_bytes: usize::MAX,
                max_age: Duration::from_secs(365 * 24 * 60 * 60),
            },
        )
        .expect("record retention");
        assert_eq!(record_count(&connection), 3);

        let connection = populated(10, 100, unix_millis());
        prune(
            &connection,
            RetentionPolicy {
                max_records: 100,
                max_bytes: 250,
                max_age: Duration::from_secs(365 * 24 * 60 * 60),
            },
        )
        .expect("byte retention");
        assert_eq!(record_count(&connection), 2);
    }

    #[test]
    fn retention_removes_records_older_than_the_window() {
        let connection = Connection::open_in_memory().expect("open in-memory log");
        connection.execute_batch(SCHEMA).expect("create log schema");
        insert_record(&connection, unix_millis().saturating_sub(10_000), 10);
        insert_record(&connection, unix_millis(), 10);
        prune(
            &connection,
            RetentionPolicy {
                max_records: 100,
                max_bytes: 10_000,
                max_age: Duration::from_secs(1),
            },
        )
        .expect("age retention");
        assert_eq!(record_count(&connection), 1);
    }

    fn populated(count: usize, bytes: i64, timestamp_ms: i64) -> Connection {
        let connection = Connection::open_in_memory().expect("open in-memory log");
        connection.execute_batch(SCHEMA).expect("create log schema");
        for _ in 0..count {
            insert_record(&connection, timestamp_ms, bytes);
        }
        connection
    }

    fn insert_record(connection: &Connection, timestamp_ms: i64, bytes: i64) {
        connection
            .execute(
                "INSERT INTO log_record (
                    timestamp_ms, timestamp_nanos, level, target, fields_json, spans_json,
                    process_uuid, pid, estimated_bytes
                 ) VALUES (?1, 0, 'INFO', 'test', '{}', '[]', 'process', 1, ?2)",
                params![timestamp_ms, bytes],
            )
            .expect("insert log record");
    }

    fn record_count(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COUNT(*) FROM log_record", [], |row| row.get(0))
            .expect("count log records")
    }
}
