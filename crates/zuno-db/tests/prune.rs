//! Destructive session pruning is preview-only unless every explicit safety gate passes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use zuno_db::prune::{
    ArchiveChange, DELETE_ORDER, PRUNE_TABLES, PruneError, PruneMode, PruneRequest, RemoteUnshare,
    SharedSession, UnshareError, delete_in_transaction, execute, preview,
};
use zuno_db::retention::{
    DAY_MILLIS, Liveness, LivenessProbe, RetentionReport, RetentionRequest, RetentionScope, select,
};
use zuno_db::{Connection, Pool, TransactionBehavior, migration, open};
use zuno_paths::DbLocation;

const NOW: i64 = 200 * DAY_MILLIS;
const OLD: i64 = 10 * DAY_MILLIS;
const NEW: i64 = 190 * DAY_MILLIS;
const SELECTED: [&str; 3] = ["ses_root", "ses_child", "ses_grandchild"];

struct Reachable;

impl LivenessProbe for Reachable {
    fn probe(&self) -> Liveness {
        Liveness::Reachable {
            active_session_ids: BTreeSet::new(),
        }
    }
}

#[derive(Default)]
struct FakeRemote {
    calls: RefCell<Vec<String>>,
    fail: Cell<bool>,
}

impl FakeRemote {
    fn failing() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail: Cell::new(true),
        }
    }
}

impl RemoteUnshare for FakeRemote {
    fn unshare(&self, session: &SharedSession) -> Result<(), UnshareError> {
        self.calls.borrow_mut().push(session.session_id.clone());
        if self.fail.get() {
            return Err(UnshareError::new("remote share service unreachable"));
        }
        Ok(())
    }
}

struct Fixture {
    pool: Pool,
    selection: RetentionReport,
}

impl Fixture {
    fn build() -> Self {
        let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
        {
            let mut connection = pool.get().expect("check out connection");
            migration::apply(&mut connection).expect("apply schema");
            seed(&connection);
        }
        let selection = {
            let connection = pool.get().expect("check out connection");
            select(
                &connection,
                &RetentionRequest::new(90, RetentionScope::AllProjects, NOW).including_shared(),
                &Reachable,
            )
            .expect("select descendant-closed retention set")
        };
        assert_eq!(
            selection
                .selected
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(SELECTED),
            "todo 81 must hand pruning the whole subtree"
        );
        Self { pool, selection }
    }

    fn connection(&self) -> zuno_db::PooledConnection<'_> {
        self.pool.get().expect("check out connection")
    }
}

fn seed(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_a', '/srv/a', 1, 1, '[]'),
                    ('prj_b', '/srv/b', 1, 1, '[]');",
        )
        .expect("insert projects");

    insert_session(connection, "ses_root", "prj_a", None, OLD, 1.25, 1);
    insert_session(
        connection,
        "ses_child",
        "prj_a",
        Some("ses_root"),
        NEW,
        2.5,
        2,
    );
    insert_session(
        connection,
        "ses_grandchild",
        "prj_a",
        Some("ses_child"),
        NEW,
        3.75,
        3,
    );
    insert_session(connection, "ses_bystander", "prj_b", None, NEW, 99.0, 99);

    for (index, session_id) in ["ses_root", "ses_child", "ses_grandchild", "ses_bystander"]
        .iter()
        .enumerate()
    {
        let seq = i64::try_from(index).expect("small fixture index");
        let message_id = format!("msg_{session_id}");
        let project_id: String = connection
            .query_row(
                "SELECT project_id FROM session WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .expect("read fixture project");
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, 1, 1, ?3)",
                rusqlite::params![
                    message_id,
                    session_id,
                    format!(r#"{{"session":"{session_id}"}}"#)
                ],
            )
            .expect("insert message");
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, 1, 1, ?4)",
                rusqlite::params![
                    format!("prt_{session_id}"),
                    message_id,
                    session_id,
                    format!(r#"{{"text":"{session_id}"}}"#)
                ],
            )
            .expect("insert part");
        connection
            .execute(
                "INSERT INTO session_message
                   (id, session_id, type, seq, time_created, time_updated, data)
                 VALUES (?1, ?2, 'user', ?3, 1, 1, '{}')",
                rusqlite::params![format!("smsg_{session_id}"), session_id, seq],
            )
            .expect("insert session_message");
        connection
            .execute(
                "INSERT INTO session_input
                   (id, session_id, prompt, delivery, state, revision, admitted_seq,
                    promoted_seq, error, time_created, time_updated)
                 VALUES (?1, ?2, '{}', 'queue', 'queued', 1, ?3, NULL, NULL, 1, 1)",
                rusqlite::params![format!("sinp_{session_id}"), session_id, seq],
            )
            .expect("insert session_input");
        let subject_payload = format!(
            r#"{{"kind":"productAgent","runID":"run_{session_id}","product":"codex","instance":"codex","tool":"subagent_codex"}}"#
        );
        connection
            .execute(
                "INSERT INTO agent_job
                   (id, parent_session_id, logical_key, subject_kind, subject_payload, status,
                    report_delivery, evidence_start_rowid, report_input_id, created_seq,
                    time_created, time_updated)
                 VALUES (?1, ?2, ?1, 'product-agent', ?3, 'completed', 'quiet', 0, ?4, ?5, 1, 1)",
                rusqlite::params![
                    format!("job_{session_id}"),
                    session_id,
                    subject_payload,
                    format!("sinp_{session_id}"),
                    seq,
                ],
            )
            .expect("insert agent job");
        connection
            .execute(
                "INSERT INTO work_plan
                   (session_id, id, revision, title, steps, time_created, time_updated)
                 VALUES (?1, ?2, 1, 'ship', '[]', 1, 1)",
                rusqlite::params![session_id, format!("plan_{session_id}")],
            )
            .expect("insert work plan");
        connection
            .execute(
                "INSERT INTO work_plan_archive
                   (id, session_id, stack_depth, revision, title, steps, state,
                    time_created, time_updated, time_archived)
                 VALUES (?1, ?2, 0, 1, 'previous ship plan', '[]', 'superseded', 1, 1, 1)",
                rusqlite::params![format!("archived_plan_{session_id}"), session_id],
            )
            .expect("insert archived work plan");
        connection
            .execute(
                "INSERT INTO work_item
                   (id, session_id, subject, description, status, priority, dependencies,
                    revision, time_created, time_updated)
                 VALUES (?1, ?2, 'verify', 'verify the result', 'pending', 'high', '[]', 1, 1, 1)",
                rusqlite::params![format!("item_{session_id}"), session_id],
            )
            .expect("insert work item");
        connection
            .execute(
                "INSERT INTO memory_reflection_delivery
                   (session_id, source_message_id, ordinal, recovered, negative_learning, time_created)
                 VALUES (?1, ?2, 1, 0, 0, 1)",
                rusqlite::params![session_id, message_id],
            )
            .expect("insert reflection delivery");
        connection
            .execute(
                "INSERT INTO memory_reflection_job
                   (id, session_id, source_message_id, trigger, status, owner_id,
                    lease_expires, time_created, time_updated, time_completed)
                 VALUES (?1, ?2, ?3, 'periodic', 'completed', 'test', 1, 1, 1, 1)",
                rusqlite::params![format!("reflection_{session_id}"), session_id, message_id],
            )
            .expect("insert reflection job");
        connection
            .execute(
                "INSERT INTO learning_job
                   (id, project_id, session_id, source_message_id, kind, extractor_version,
                    idempotency_key, status, attempt, scheduled_at, payload, result,
                    time_created, time_updated, time_completed)
                 VALUES (?1, ?2, ?3, ?4, 'extraction', 'extractor-v1', ?5,
                         'completed', 1, 1, '{}', '{}', 1, 1, 1)",
                rusqlite::params![
                    format!("learning_{session_id}"),
                    project_id,
                    session_id,
                    message_id,
                    format!("learning:{session_id}"),
                ],
            )
            .expect("insert learning job");
        connection
            .execute(
                "INSERT INTO message_feedback
                   (message_id, session_id, rating, note, revision, time_created, time_updated)
                 VALUES (?1, ?2, 1, 'useful', 1, 1, 1)",
                rusqlite::params![message_id, session_id],
            )
            .expect("insert message feedback");
        connection
            .execute(
                "INSERT INTO session_context_epoch (session_id, baseline, snapshot, baseline_seq)
                 VALUES (?1, 'baseline', '{}', 0)",
                [session_id],
            )
            .expect("insert session_context_epoch");
        insert_event_aggregate(connection, session_id);
        insert_event_aggregate(connection, &format!("sse:{session_id}"));
    }

    connection
        .execute(
            "INSERT INTO session_share (session_id, id, secret, url, time_created, time_updated)
             VALUES ('ses_root', 'shr_root', 'secret', 'https://share.example/root', 1, 1)",
            [],
        )
        .expect("insert share row");

    // Both rows point at a surviving message, so the message cascade cannot see
    // them. One belongs to the selected child; the other is already globally
    // orphaned and can only be removed by the final orphan sweep.
    for (part_id, session_id) in [
        ("prt_cross_selected", "ses_child"),
        ("prt_global_orphan", "ses_missing"),
    ] {
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, 'msg_ses_bystander', ?2, 1, 1, '{}')",
                [part_id, session_id],
            )
            .expect("insert cascade-invisible part");
    }
}

fn insert_session(
    connection: &Connection,
    id: &str,
    project_id: &str,
    parent_id: Option<&str>,
    updated: i64,
    cost: f64,
    token_factor: i64,
) {
    connection
        .execute(
            "INSERT INTO session
               (id, project_id, parent_id, slug, directory, title, version, share_url,
                cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read,
                tokens_cache_write, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?1, '/srv', ?1, 'test', ?4,
                     ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            rusqlite::params![
                id,
                project_id,
                parent_id,
                (id == "ses_root").then_some("https://share.example/root"),
                cost,
                token_factor,
                token_factor * 2,
                token_factor * 3,
                token_factor * 4,
                token_factor * 5,
                updated,
            ],
        )
        .expect("insert session");
}

fn insert_event_aggregate(connection: &Connection, aggregate_id: &str) {
    connection
        .execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, 1)",
            [aggregate_id],
        )
        .expect("insert event sequence");
    connection
        .execute(
            "INSERT INTO event (id, aggregate_id, seq, type, data)
             VALUES (?1, ?2, 1, 'session.updated', '{}')",
            rusqlite::params![
                format!("evt_{}", aggregate_id.replace(':', "_")),
                aggregate_id
            ],
        )
        .expect("insert event");
}

fn count(connection: &Connection, table: &str) -> u64 {
    let rows: i64 = connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count table rows");
    u64::try_from(rows).expect("row count is non-negative")
}

fn all_table_counts(connection: &Connection) -> BTreeMap<String, u64> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table inventory");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query table inventory")
        .map(|table| {
            let table = table.expect("read table name");
            let rows = count(connection, &table);
            (table, rows)
        })
        .collect()
}

#[test]
fn prune_default_preview_is_inert_across_every_real_table() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let before = all_table_counts(&connection);
    assert_eq!(
        before.len(),
        zuno_db::schema::TABLE_COUNT + 1,
        "current schema tables plus migration"
    );
    let remote = FakeRemote::default();

    let outcome = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::default(),
        &remote,
    )
    .expect("preview by default");

    assert_eq!(outcome.mode, PruneMode::Preview);
    assert_eq!(all_table_counts(&connection), before);
    assert!(remote.calls.borrow().is_empty(), "preview never unshares");
    assert_eq!(outcome.preview.tables.len(), PRUNE_TABLES.len());
    assert_eq!(outcome.preview.total_rows, 57);
    assert!(outcome.preview.total_bytes > 0);
    assert_eq!(outcome.preview.cost, 7.5);
    assert_eq!(outcome.preview.tokens.input, 6);
    assert_eq!(outcome.preview.tokens.output, 12);
    assert_eq!(outcome.preview.tokens.reasoning, 18);
    assert_eq!(outcome.preview.tokens.cache_read, 24);
    assert_eq!(outcome.preview.tokens.cache_write, 30);
}

#[test]
fn prune_counts_and_deletes_additive_continuity_rows_when_present() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    zuno_db::continuity::ensure_schema(&connection).expect("create continuity tables");
    for session_id in ["ses_root", "ses_bystander"] {
        connection
            .execute(
                "INSERT INTO session_note
                   (session_id, agent, name, revision, content, content_sha256,
                    time_created, time_updated)
                 VALUES (?1, 'build', 'evidence', 1, ?1, 'sha', 1, 1)",
                [session_id],
            )
            .expect("insert note");
        connection
            .execute(
                "INSERT INTO session_note_operation
                   (session_id, agent, call_id, request_sha256, action, name,
                    result_revision, result_content_sha256, time_created)
                 VALUES (?1, 'build', ?2, 'request-sha', 'write', 'evidence',
                         1, 'sha', 1)",
                rusqlite::params![session_id, format!("call_{session_id}")],
            )
            .expect("insert note operation");
    }
    let expected = preview(&connection, &fixture.selection).expect("preview continuity rows");
    assert_eq!(
        expected
            .tables
            .iter()
            .find(|impact| impact.table == "session_note")
            .expect("note impact")
            .rows,
        1
    );
    assert_eq!(
        expected
            .tables
            .iter()
            .find(|impact| impact.table == "session_note_operation")
            .expect("note operation impact")
            .rows,
        1
    );

    let outcome = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::delete().confirmed(),
        &FakeRemote::default(),
    )
    .expect("delete selected continuity rows");

    assert_eq!(outcome.preview, expected);
    assert_eq!(count(&connection, "session_note"), 1);
    assert_eq!(count(&connection, "session_note_operation"), 1);
}

#[test]
fn prune_delete_requires_confirmation_before_remote_or_local_side_effects() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let before = all_table_counts(&connection);
    let remote = FakeRemote::default();

    let error = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::delete(),
        &remote,
    )
    .expect_err("delete without confirmation must be refused");

    assert!(matches!(error, PruneError::ConfirmationRequired));
    assert_eq!(all_table_counts(&connection), before);
    assert!(
        remote.calls.borrow().is_empty(),
        "confirmation is checked before remote unshare"
    );
}

#[test]
fn prune_preview_counts_exactly_match_the_subsequent_transactional_delete() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let before = all_table_counts(&connection);
    let expected = preview(&connection, &fixture.selection).expect("preview");
    let remote = FakeRemote::default();

    let outcome = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::delete().confirmed(),
        &remote,
    )
    .expect("confirmed delete");
    let after = all_table_counts(&connection);

    assert_eq!(outcome.preview, expected);
    assert_eq!(outcome.changed_sessions, 3);
    for impact in &expected.tables {
        assert_eq!(
            before.get(impact.table).copied().unwrap_or(0)
                - after.get(impact.table).copied().unwrap_or(0),
            impact.rows,
            "preview count for {} must equal rows actually removed",
            impact.table
        );
    }
    assert_eq!(remote.calls.borrow().as_slice(), ["ses_root"]);
    assert_eq!(count(&connection, "session"), 1, "bystander survives");

    for (table, column) in [
        ("memory_reflection_job", "session_id"),
        ("memory_reflection_delivery", "session_id"),
        ("learning_job", "session_id"),
        ("message_feedback", "session_id"),
        ("agent_job", "parent_session_id"),
        ("work_item", "session_id"),
        ("work_plan", "session_id"),
        ("work_plan_archive", "session_id"),
        ("session_context_epoch", "session_id"),
        ("session_input", "session_id"),
        ("session_message", "session_id"),
        ("part", "session_id"),
        ("message", "session_id"),
        ("session_share", "session_id"),
        ("session", "id"),
    ] {
        let remaining: i64 = connection
            .query_row(
                &format!(
                    "SELECT count(*) FROM {table} WHERE {column} IN
                     ('ses_root', 'ses_child', 'ses_grandchild')"
                ),
                [],
                |row| row.get(0),
            )
            .expect("count selected rows");
        assert_eq!(remaining, 0, "{table} retained selected rows");
    }
    let orphaned_parts: i64 = connection
        .query_row(
            "SELECT count(*) FROM part p
             WHERE NOT EXISTS (SELECT 1 FROM session s WHERE s.id = p.session_id)",
            [],
            |row| row.get(0),
        )
        .expect("count orphaned parts");
    assert_eq!(orphaned_parts, 0, "orphan sweep must be global");
    for table in ["event", "event_sequence"] {
        let remaining: i64 = connection
            .query_row(
                &format!(
                    "SELECT count(*) FROM {table}
                     WHERE aggregate_id IN ('ses_root', 'ses_child', 'ses_grandchild',
                                            'sse:ses_root', 'sse:ses_child',
                                            'sse:ses_grandchild')"
                ),
                [],
                |row| row.get(0),
            )
            .expect("count durable events");
        assert_eq!(remaining, 0, "{table} retained durable session events");
    }
}

#[test]
fn prune_archive_is_reversible_without_deleting_session_data() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let before = all_table_counts(&connection);
    let remote = FakeRemote::default();

    let archived = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::archive_at(123_456),
        &remote,
    )
    .expect("archive selected sessions");
    assert_eq!(archived.changed_sessions, 3);
    let archived_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM session
             WHERE id IN ('ses_root', 'ses_child', 'ses_grandchild')
               AND time_archived = 123456",
            [],
            |row| row.get(0),
        )
        .expect("count archived rows");
    assert_eq!(archived_count, 3);
    assert_eq!(all_table_counts(&connection), before);

    let restored = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::restore_archive(),
        &remote,
    )
    .expect("restore selected sessions");
    assert_eq!(restored.changed_sessions, 3);
    let still_archived: i64 = connection
        .query_row(
            "SELECT count(*) FROM session
             WHERE id IN ('ses_root', 'ses_child', 'ses_grandchild')
               AND time_archived IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count restored rows");
    assert_eq!(still_archived, 0);
    assert!(remote.calls.borrow().is_empty(), "archive never unshares");
}

#[test]
fn prune_shared_session_is_refused_when_remote_unshare_is_unreachable() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let before = all_table_counts(&connection);
    let remote = FakeRemote::failing();

    let error = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::delete().confirmed(),
        &remote,
    )
    .expect_err("unreachable remote must refuse shared-session deletion");

    assert!(matches!(
        error,
        PruneError::RemoteUnshareFailed { ref session_id, .. } if session_id == "ses_root"
    ));
    assert_eq!(remote.calls.borrow().as_slice(), ["ses_root"]);
    assert_eq!(all_table_counts(&connection), before);
}

#[test]
fn prune_force_proceeds_with_a_verbatim_remote_survival_warning() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let remote = FakeRemote::failing();
    let outcome = execute(
        &mut connection,
        &fixture.selection,
        &PruneRequest::delete().confirmed().forced(),
        &remote,
    )
    .expect("force permits local deletion after warning");

    assert_eq!(
        outcome.warnings,
        vec![String::from(
            "remote unshare failed for shared session ses_root: remote share service unreachable; \
             local rows were deleted because --force was supplied and the remote copy may survive"
        )]
    );
    assert_eq!(count(&connection, "session"), 1);
}

#[test]
fn prune_rolled_back_delete_preserves_the_original_preview() {
    let fixture = Fixture::build();
    let mut connection = fixture.connection();
    let before = preview(&connection, &fixture.selection).expect("original preview");
    let remote = FakeRemote::default();

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin delete transaction");
    let deleted = delete_in_transaction(&transaction, &fixture.selection, true, false, &remote)
        .expect("run delete before simulated process abort");
    assert_eq!(deleted.changed_sessions, 3);
    transaction.rollback().expect("simulate aborted process");

    let after = preview(&connection, &fixture.selection).expect("preview after rollback");
    assert_eq!(
        after, before,
        "an aborted transaction must expose no partial delete"
    );
}

#[test]
fn prune_delete_order_and_true_related_table_count_are_pinned() {
    assert_eq!(
        PRUNE_TABLES.len(),
        20,
        "every session-owned schema table must be explicit"
    );
    assert_eq!(
        DELETE_ORDER,
        [
            "session_note_operation",
            "session_note",
            "memory_reflection_job",
            "memory_reflection_delivery",
            "learning_job",
            "message_feedback",
            "agent_job",
            "work_item",
            "work_plan",
            "work_plan_archive",
            "session_context_epoch",
            "session_input",
            "session_message",
            "part",
            "message",
            "session_share",
            "session",
            "event_sequence",
            "event",
            "verification_receipt",
        ]
    );
    assert_eq!(
        PruneRequest::archive_at(1).mode,
        PruneMode::Archive(ArchiveChange::Set { at_ms: 1 })
    );
    assert_eq!(
        PruneRequest::restore_archive().mode,
        PruneMode::Archive(ArchiveChange::Clear)
    );
}

#[test]
fn prune_public_api_compiles_against_a_plain_configured_connection() {
    let mut connection = open::open(&DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    let report = select(
        &connection,
        &RetentionRequest::new(1, RetentionScope::AllProjects, NOW),
        &Reachable,
    )
    .expect("select empty set");
    let outcome = execute(
        &mut connection,
        &report,
        &PruneRequest::default(),
        &FakeRemote::default(),
    )
    .expect("preview empty database");
    assert_eq!(outcome.preview.total_rows, 0);
}
