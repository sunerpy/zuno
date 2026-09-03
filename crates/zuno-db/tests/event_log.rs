//! Type-filtered reads over the append-only session event log.
//!
//! Stored types carry a version suffix (`plan.driver.phase.1`). A consumer that
//! rebuilds a projection must see every version ever written for its type — the
//! rows an older release logged do not disappear when the event schema is bumped
//! — and must never be handed a sibling type that merely shares the prefix.

use serde_json::{Map, Value, json};
use std::sync::Arc;
use zuno_db::event_log::{NewSessionEvent, SessionEvent, SessionEventLog};
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;

const SESSION_ID: &str = "ses_events";
const PHASE: &str = "plan.driver.phase";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open database"));
    let mut connection = pool.get().expect("database connection");
    migration::apply(&mut connection).expect("apply schema");
    drop(connection);
    pool
}

fn properties(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn append(log: &SessionEventLog, event_type: &str, pairs: &[(&str, Value)]) -> i64 {
    log.append(
        SESSION_ID,
        NewSessionEvent::new(event_type, properties(pairs)).expect("valid event"),
    )
    .expect("append event")
    .sequence
}

/// Write a row exactly as an older release did: same table, an older version
/// suffix, and the stream's sequence advanced so later appends do not collide.
fn insert_historical(pool: &Pool, seq: i64, stored_type: &str, data: &str) {
    let connection = pool.get().expect("database connection");
    connection
        .execute(
            "INSERT INTO event (id, aggregate_id, seq, type, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                format!("evt_historical_{seq}"),
                SESSION_ID,
                seq,
                stored_type,
                data
            ],
        )
        .expect("insert historical event");
    connection
        .execute(
            "UPDATE event_sequence SET seq = max(seq, ?2) WHERE aggregate_id = ?1",
            rusqlite::params![SESSION_ID, seq],
        )
        .expect("advance the stream sequence");
}

fn sequences(events: &[SessionEvent]) -> Vec<i64> {
    events.iter().map(|event| event.sequence).collect()
}

fn versions(events: &[SessionEvent]) -> Vec<u32> {
    events.iter().map(|event| event.version).collect()
}

#[test]
fn type_filtered_reads_return_every_stored_version_and_only_that_type() {
    let pool = initialized();
    let log = SessionEventLog::new(Arc::clone(&pool));

    // seq 0: the version this build appends.
    assert_eq!(
        append(
            &log,
            PHASE,
            &[("cycleId", json!("c1")), ("phase", json!("executing"))]
        ),
        0
    );
    // seq 1: the same type as an older release stored it.
    insert_historical(
        &pool,
        1,
        "plan.driver.phase.0",
        r#"{"cycleId":"c1","phase":"reconciling","legacy":true}"#,
    );
    // seq 2: a sibling type that shares the prefix must never match.
    insert_historical(&pool, 2, "plan.driver.phase.extra.1", r#"{"cycleId":"c1"}"#);
    // seq 3: an unrelated type.
    assert_eq!(
        append(
            &log,
            "session.input.admitted",
            &[("inputID", json!("input-1"))]
        ),
        3
    );
    // seq 4: another current-version row.
    assert_eq!(
        append(
            &log,
            PHASE,
            &[("cycleId", json!("c2")), ("phase", json!("terminal"))]
        ),
        4
    );

    let all = log
        .read_of_type_after(SESSION_ID, PHASE, None)
        .expect("read all versions");
    assert_eq!(sequences(&all), [0, 1, 4]);
    assert_eq!(versions(&all), [1, 0, 1]);
    assert!(all.iter().all(|event| event.event_type == PHASE));
    assert_eq!(all[1].properties["legacy"], true);

    let after = log
        .read_of_type_after(SESSION_ID, PHASE, Some(0))
        .expect("read after a cursor");
    assert_eq!(sequences(&after), [1, 4]);

    // The whole-stream read and the typed read agree on what the type's rows are.
    let unfiltered: Vec<SessionEvent> = log
        .read_after(SESSION_ID, None)
        .expect("read stream")
        .into_iter()
        .filter(|event| event.event_type == PHASE)
        .collect();
    assert_eq!(unfiltered, all);

    // A prefix of the type is not the type, and another session sees nothing.
    assert!(
        log.read_of_type_after(SESSION_ID, "plan.driver", None)
            .expect("read a prefix")
            .is_empty()
    );
    assert!(
        log.read_of_type_after("ses_other", PHASE, None)
            .expect("read another session")
            .is_empty()
    );
}

#[test]
fn latest_of_type_ignores_newer_siblings_and_accepts_an_older_version_as_newest() {
    let pool = initialized();
    let log = SessionEventLog::new(Arc::clone(&pool));
    assert_eq!(
        log.latest_of_type(SESSION_ID, PHASE).expect("empty stream"),
        None
    );

    append(&log, PHASE, &[("cycleId", json!("c1"))]); // seq 0
    append(&log, PHASE, &[("cycleId", json!("c2"))]); // seq 1
    append(&log, "session.input.admitted", &[]); // seq 2
    insert_historical(&pool, 3, "plan.driver.phase.extra.1", "{}"); // seq 3

    let latest = log
        .latest_of_type(SESSION_ID, PHASE)
        .expect("read latest")
        .expect("a phase event exists");
    assert_eq!(latest.sequence, 1);
    assert_eq!(latest.version, 1);
    assert_eq!(latest.properties["cycleId"], "c2");

    // A row an older release wrote after this one is still the newest of the type.
    insert_historical(&pool, 4, "plan.driver.phase.0", r#"{"cycleId":"c3"}"#);
    let latest = log
        .latest_of_type(SESSION_ID, PHASE)
        .expect("read latest")
        .expect("a phase event exists");
    assert_eq!(latest.sequence, 4);
    assert_eq!(latest.version, 0);
    assert_eq!(latest.properties["cycleId"], "c3");

    assert_eq!(
        log.latest_of_type(SESSION_ID, "plan.driver")
            .expect("read a prefix"),
        None
    );
    let error = log
        .latest_of_type(SESSION_ID, "")
        .expect_err("an empty type is not a query");
    assert!(
        matches!(error, zuno_error::DbError::Query { .. }),
        "{error:?}"
    );
}
