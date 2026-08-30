//! Session CRUD, the three list scopes, and the subtree delete.
//!
//! The delete tests carry most of the weight here, because the failure they
//! guard against is silent: `session.parent_id` has no foreign key, so a
//! one-row `DELETE` succeeds, reports success, and leaves descendants pointing
//! at a parent that is gone. Nothing in SQLite complains and no later read
//! fails — the rows simply stop being reachable while still occupying every
//! count. So the fixture builds a three-level tree with every dependent table
//! populated, records row counts before and after, and asserts the counts
//! rather than the absence of an error.

use serde_json::json;
use std::path::Path;
use zuno_db::message::{MessageRecord, MessageStore};
use zuno_db::session::{
    ArchivedFilter, Creation, ListQuery, ListScope, MessageUsage, SessionCreate, SessionSort,
    SortDirection, Store, TokenAccounting, session_path,
};
use zuno_db::{Connection, Pool, migration, session};
use zuno_error::DbError;
use zuno_paths::DbLocation;

const VERSION: &str = "1.18.13";
const WORKTREE: &str = "/srv/app";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An in-memory pool with the current schema applied.
fn pool() -> Pool {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    {
        let mut connection = pool.get().expect("check out a connection");
        migration::apply(&mut connection).expect("apply the schema");
    }
    pool
}

fn insert_project(connection: &Connection, id: &str, worktree: &str, name: Option<&str>) {
    connection
        .execute(
            "INSERT INTO project (id, worktree, name, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, ?3, 1, 1, '[]')",
            rusqlite::params![id, worktree, name],
        )
        .expect("insert project");
}

fn insert_message(connection: &Connection, id: &str, session_id: &str) {
    connection
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, 1, 1, '{}')",
            [id, session_id],
        )
        .expect("insert message");
}

fn insert_part(connection: &Connection, id: &str, message_id: &str, session_id: &str) {
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, 1, 1, '{}')",
            [id, message_id, session_id],
        )
        .expect("insert part");
}

fn insert_work_state(connection: &Connection, session_id: &str) {
    connection
        .execute(
            "INSERT INTO work_plan \
             (session_id, id, revision, title, steps, time_created, time_updated) \
             VALUES (?1, ?2, 1, 'ship', '[]', 1, 1)",
            rusqlite::params![session_id, format!("plan_{session_id}")],
        )
        .expect("insert work plan");
    connection
        .execute(
            "INSERT INTO work_item \
             (id, session_id, subject, description, status, priority, dependencies, revision, \
              time_created, time_updated) \
             VALUES (?1, ?2, 'verify', 'verify the result', 'pending', 'high', '[]', 1, 1, 1)",
            rusqlite::params![format!("item_{session_id}"), session_id],
        )
        .expect("insert work item");
}

fn insert_session_message(connection: &Connection, id: &str, session_id: &str, seq: i64) {
    connection
        .execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, \
             data) VALUES (?1, ?2, 'user', ?3, 1, 1, '{}')",
            rusqlite::params![id, session_id, seq],
        )
        .expect("insert session_message");
}

fn insert_session_input(connection: &Connection, id: &str, session_id: &str, seq: i64) {
    connection
        .execute(
            "INSERT INTO session_input \
             (id, session_id, prompt, delivery, state, revision, admitted_seq, promoted_seq, \
              error, time_created, time_updated) \
             VALUES (?1, ?2, '{}', 'queue', 'queued', 1, ?3, NULL, NULL, 1, 1)",
            rusqlite::params![id, session_id, seq],
        )
        .expect("insert session_input");
}

fn insert_context_epoch(connection: &Connection, session_id: &str) {
    connection
        .execute(
            "INSERT INTO session_context_epoch (session_id, baseline, snapshot, baseline_seq) \
             VALUES (?1, '{}', '{}', 0)",
            [session_id],
        )
        .expect("insert session_context_epoch");
}

fn insert_share(connection: &Connection, session_id: &str) {
    connection
        .execute(
            "INSERT INTO session_share (session_id, id, secret, url, time_created, time_updated) \
             VALUES (?1, 'shr_1', 'secret', 'https://share.example/1', 1, 1)",
            [session_id],
        )
        .expect("insert session_share");
}

/// Seed the event log for one aggregate: the sequence row plus two events.
fn insert_events(connection: &Connection, aggregate_id: &str) {
    connection
        .execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, 2)",
            [aggregate_id],
        )
        .expect("insert event_sequence");
    for seq in 0..2 {
        connection
            .execute(
                "INSERT INTO event (id, aggregate_id, seq, type, data) \
                 VALUES (?1, ?2, ?3, 'session.updated', '{}')",
                rusqlite::params![format!("evt_{aggregate_id}_{seq}"), aggregate_id, seq],
            )
            .expect("insert event");
    }
}

fn count(connection: &Connection, sql: &str) -> i64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("run a count")
}

fn count_for(connection: &Connection, sql: &str, binding: &str) -> i64 {
    connection
        .query_row(sql, [binding], |row| row.get(0))
        .expect("run a count")
}

fn draft(id: &str, project_id: &str, directory: &str, title: &str) -> SessionCreate {
    SessionCreate::new(
        id,
        format!("slug-{id}"),
        project_id,
        WORKTREE,
        directory,
        title,
        VERSION,
    )
}

/// A three-level tree — `ses_root` -> `ses_child` -> `ses_grandchild` — with
/// every table that hangs off a session populated for all three, plus a
/// bystander session in a second project that must survive untouched, plus the
/// one `part` row the cascade cannot see.
struct Tree {
    pool: Pool,
}

impl Tree {
    fn build() -> Self {
        let pool = pool();
        {
            let connection = pool.get().expect("check out a connection");
            insert_project(&connection, "prj_a", WORKTREE, Some("app"));
            insert_project(&connection, "prj_b", "/srv/other", Some("other"));
        }
        let store = Store::new(&pool);
        // The root is the *oldest* row in the tree: it was created first and then
        // sat idle while its children were worked on. That is what makes an
        // age-based retention pass single it out and leave its descendants.
        store
            .create(&draft("ses_root", "prj_a", WORKTREE, "root").at(100))
            .expect("create root");
        store
            .create(
                &draft("ses_child", "prj_a", "/srv/app/pkg", "child")
                    .with_parent("ses_root")
                    .at(200),
            )
            .expect("create child");
        store
            .create(
                &draft("ses_grandchild", "prj_a", "/srv/app/pkg/core", "grandchild")
                    .with_parent("ses_child")
                    .at(300),
            )
            .expect("create grandchild");
        store
            .create(&draft("ses_bystander", "prj_b", "/srv/other", "bystander").at(400))
            .expect("create bystander");

        {
            let connection = pool.get().expect("check out a connection");
            for (index, id) in ["ses_root", "ses_child", "ses_grandchild", "ses_bystander"]
                .iter()
                .enumerate()
            {
                let seq = i64::try_from(index).expect("small index");
                insert_message(&connection, &format!("msg_{id}"), id);
                insert_part(&connection, &format!("prt_{id}"), &format!("msg_{id}"), id);
                insert_work_state(&connection, id);
                insert_session_message(&connection, &format!("smsg_{id}"), id, seq);
                insert_session_input(&connection, &format!("sinp_{id}"), id, seq);
                insert_context_epoch(&connection, id);
                insert_events(&connection, id);
            }
            insert_share(&connection, "ses_root");

            // The part the cascade cannot reach. Its `session_id` names the
            // child, but its `message_id` names a message owned by the
            // bystander — which survives the delete, so `ON DELETE CASCADE` on
            // `part.message_id` never fires for it. `part.session_id` is only an
            // index (`part_session_idx`), never a foreign key, so nothing in the
            // schema removes this row when its session goes.
            insert_part(&connection, "prt_orphan", "msg_ses_bystander", "ses_child");
        }
        Self { pool }
    }

    fn store(&self) -> Store<'_> {
        Store::new(&self.pool)
    }

    fn connection(&self) -> zuno_db::PooledConnection<'_> {
        self.pool.get().expect("check out a connection")
    }
}

// ---------------------------------------------------------------------------
// create / get / touch
// ---------------------------------------------------------------------------

#[test]
fn create_writes_every_column_the_caller_supplied() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, Some("app"));
    }
    let store = Store::new(&pool);
    let input = SessionCreate {
        agent: Some(String::from("build")),
        model: Some(String::from(r#"{"providerID":"anthropic","id":"claude"}"#)),
        metadata: Some(String::from(r#"{"source":"cli"}"#)),
        permission: Some(String::from(r#"[{"edit":"allow"}]"#)),
        workspace_id: None,
        ..draft("ses_a", "prj_a", "/srv/app/pkg/core", "review the parser").at(1_700)
    };
    let created = store.create(&input).expect("create the session");
    assert!(created.was_inserted());

    let session = store.get("ses_a").expect("read the session back");
    assert_eq!(session.id, "ses_a");
    assert_eq!(session.slug, "slug-ses_a");
    assert_eq!(session.project_id, "prj_a");
    assert_eq!(session.directory, "/srv/app/pkg/core");
    assert_eq!(session.path.as_deref(), Some("pkg/core"));
    assert_eq!(session.subpath(), Some("pkg/core"));
    assert_eq!(session.title, "review the parser");
    assert_eq!(session.version, VERSION);
    assert_eq!(session.agent.as_deref(), Some("build"));
    assert_eq!(
        session.model.as_deref(),
        Some(r#"{"providerID":"anthropic","id":"claude"}"#)
    );
    assert_eq!(session.metadata.as_deref(), Some(r#"{"source":"cli"}"#));
    assert_eq!(session.permission.as_deref(), Some(r#"[{"edit":"allow"}]"#));
    assert_eq!(session.time_created, 1_700);
    assert_eq!(session.time_updated, 1_700);
    assert_eq!(session.usage.cost, 0.0);
    assert_eq!(session.usage.tokens.input, 0);
    assert_eq!(session.usage.tokens.cache_write, 0);
    assert!(!session.usage.known);
    assert_eq!(session.summary, None);
    assert_eq!(session.share_url, None);
    assert!(session.is_root());
    assert!(!session.is_archived());
}

#[test]
fn create_stores_the_empty_string_for_a_session_at_the_worktree_root() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "root"))
        .expect("create the session");

    let session = store.get("ses_a").expect("read it back");
    assert_eq!(
        session.path.as_deref(),
        Some(""),
        "the column holds the empty string path.relative produces, not NULL"
    );
    assert_eq!(
        session.subpath(),
        None,
        "the API reports that empty string as absent"
    );

    let connection = pool.get().expect("check out a connection");
    let nulls = count(
        &connection,
        "SELECT count(*) FROM session WHERE path IS NULL",
    );
    assert_eq!(nulls, 0);
}

#[test]
fn assistant_usage_reconciliation_is_normalized_and_idempotent() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_usage", "prj_a", WORKTREE, "usage"))
        .expect("create the session");

    let checkpoint = |input: i64, output: i64, read: i64, write: i64| {
        MessageRecord::from_json(json!({
            "id": "msg_usage",
            "sessionID": "ses_usage",
            "role": "assistant",
            "time": {"created": 10},
            "cost": 0.25,
            "tokens": {
                "input": input,
                "output": output,
                "reasoning": 3,
                "cache": {"read": read, "write": write},
                "accounting": "cache-inside-input"
            }
        }))
        .expect("valid assistant message")
    };

    for message in [checkpoint(100, 20, 15, 10), checkpoint(100, 20, 15, 10)] {
        pool.transaction(|transaction| {
            let messages = MessageStore::new(transaction);
            let previous = messages
                .find_message("msg_usage")?
                .map(|stored| MessageUsage::from_data(&stored.data));
            messages.put_message_at(&message, 20)?;
            session::reconcile_usage(
                transaction,
                "ses_usage",
                previous,
                MessageUsage::from_data(&message.data),
                Some(128_000),
            )
        })
        .expect("reconcile checkpoint");
    }

    let usage = store.get("ses_usage").expect("read usage").usage;
    assert!(usage.known);
    assert_eq!(usage.accounting, Some(TokenAccounting::CacheInsideInput));
    assert_eq!(usage.tokens.input, 75);
    assert_eq!(usage.tokens.output, 20);
    assert_eq!(usage.tokens.reasoning, 3);
    assert_eq!(usage.tokens.cache_read, 15);
    assert_eq!(usage.tokens.cache_write, 10);
    assert_eq!(usage.last_prompt_tokens, Some(100));
    assert_eq!(usage.context_limit, Some(128_000));
    assert_eq!(usage.cost, 0.25);

    let replacement = checkpoint(140, 30, 20, 10);
    pool.transaction(|transaction| {
        let messages = MessageStore::new(transaction);
        let previous = messages
            .find_message("msg_usage")?
            .map(|stored| MessageUsage::from_data(&stored.data));
        messages.put_message_at(&replacement, 30)?;
        session::reconcile_usage(
            transaction,
            "ses_usage",
            previous,
            MessageUsage::from_data(&replacement.data),
            Some(128_000),
        )
    })
    .expect("reconcile replacement");

    let usage = store.get("ses_usage").expect("read replacement").usage;
    assert_eq!(usage.tokens.input, 110);
    assert_eq!(usage.tokens.output, 30);
    assert_eq!(usage.tokens.cache_read, 20);
    assert_eq!(usage.tokens.cache_write, 10);
    assert_eq!(usage.last_prompt_tokens, Some(140));
    assert_eq!(usage.cost, 0.25);
}

#[test]
fn failed_request_keeps_confirmed_usage_and_persists_the_local_estimate() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft(
            "ses_failed_usage",
            "prj_a",
            WORKTREE,
            "failed usage",
        ))
        .expect("create the session");

    let checkpoint = MessageRecord::from_json(json!({
        "id": "msg_confirmed",
        "sessionID": "ses_failed_usage",
        "role": "assistant",
        "time": {"created": 10},
        "cost": 0.5,
        "tokens": {
            "input": 4_200,
            "output": 500,
            "reasoning": 100,
            "cache": {"read": 700, "write": 0},
            "accounting": "cache-inside-input"
        }
    }))
    .expect("confirmed checkpoint");
    pool.transaction(|transaction| {
        MessageStore::new(transaction).put_message_at(&checkpoint, 10)?;
        session::reconcile_usage(
            transaction,
            "ses_failed_usage",
            None,
            MessageUsage::from_data(&checkpoint.data),
            Some(10_000),
        )
    })
    .expect("reconcile confirmed usage");
    let confirmed = store
        .get("ses_failed_usage")
        .expect("confirmed session")
        .usage
        .snapshot();

    let connection = pool.get().expect("request connection");
    session::record_provider_request_started(&connection, "ses_failed_usage", 8_500, Some(10_000))
        .expect("record local estimate");
    session::record_turn_failure(&connection, "ses_failed_usage").expect("record failed turn");

    let failed = store
        .get("ses_failed_usage")
        .expect("failed session")
        .usage
        .snapshot();
    assert_eq!(failed.confirmed, confirmed.confirmed);
    assert_eq!(failed.last_prompt_tokens, confirmed.last_prompt_tokens);
    assert_eq!(failed.estimated_pending_prompt_tokens, Some(8_500));
    assert_eq!(failed.failed_turns, 1);
    assert!(failed.last_failed_at.is_some());
    assert!(failed.confirmed_known);
}

#[test]
fn assistant_usage_without_accounting_keeps_the_session_unknown() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_unknown", "prj_a", WORKTREE, "unknown"))
        .expect("create the session");

    let unclassified = MessageRecord::from_json(json!({
        "id": "msg_unclassified",
        "sessionID": "ses_unknown",
        "role": "assistant",
        "time": {"created": 1},
        "cost": 0.0,
        "tokens": {
            "input": 100,
            "output": 20,
            "reasoning": 0,
            "cache": {"read": 50, "write": 0}
        }
    }))
    .expect("unclassified message");
    MessageStore::new(&pool.get().expect("connection"))
        .put_message_at(&unclassified, 1)
        .expect("insert unclassified message");

    let current = MessageRecord::from_json(json!({
        "id": "msg_current",
        "sessionID": "ses_unknown",
        "role": "assistant",
        "time": {"created": 2},
        "cost": 0.0,
        "tokens": {
            "input": 40,
            "output": 10,
            "reasoning": 0,
            "cache": {"read": 5, "write": 0},
            "accounting": "cache-beside-input"
        }
    }))
    .expect("current message");
    pool.transaction(|transaction| {
        MessageStore::new(transaction).put_message_at(&current, 2)?;
        session::reconcile_usage(
            transaction,
            "ses_unknown",
            None,
            MessageUsage::from_data(&current.data),
            Some(200_000),
        )
    })
    .expect("reconcile known tail");

    let usage = store.get("ses_unknown").expect("read usage").usage;
    assert!(!usage.known);
    assert_eq!(usage.last_prompt_tokens, Some(45));
    assert_eq!(usage.context_limit, Some(200_000));
}

#[test]
fn create_of_an_id_that_already_exists_keeps_the_stored_row() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "first").at(10))
        .expect("first create");
    let second = store
        .create(&draft("ses_a", "prj_a", WORKTREE, "second").at(20))
        .expect("second create");

    assert!(!second.was_inserted());
    assert!(matches!(second, Creation::AlreadyExists(_)));
    assert_eq!(second.session().title, "first");
    assert_eq!(second.into_session().time_created, 10);

    let connection = pool.get().expect("check out a connection");
    assert_eq!(count(&connection, "SELECT count(*) FROM session"), 1);
}

#[test]
fn create_against_a_missing_project_is_rejected_by_the_foreign_key() {
    let pool = pool();
    let store = Store::new(&pool);
    let error = store
        .create(&draft("ses_a", "prj_missing", WORKTREE, "orphan"))
        .expect_err("the project foreign key must reject this");
    assert!(matches!(error, DbError::Query { .. }), "{error:?}");
}

#[test]
fn get_of_a_missing_session_names_the_table_and_the_id() {
    let pool = pool();
    let store = Store::new(&pool);
    assert_eq!(store.find("ses_nope").expect("find"), None);
    let error = store.get("ses_nope").expect_err("must not be found");
    match error {
        DbError::NotFound { table, id } => {
            assert_eq!(table, "session");
            assert_eq!(id, "ses_nope");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn touch_moves_time_updated_and_leaves_time_created_alone() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "root").at(1_000))
        .expect("create");

    let written = store.touch_at("ses_a", 5_000).expect("touch");
    assert_eq!(written, 5_000);

    let session = store.get("ses_a").expect("read back");
    assert_eq!(session.time_created, 1_000);
    assert_eq!(session.time_updated, 5_000);

    let now = store.touch("ses_a").expect("touch with the clock");
    assert!(now > 5_000, "the clock must move it forward, got {now}");
}

#[test]
fn touch_of_a_missing_session_reports_not_found_rather_than_succeeding() {
    let pool = pool();
    let store = Store::new(&pool);
    let error = store.touch("ses_nope").expect_err("must not be found");
    assert!(matches!(error, DbError::NotFound { .. }), "{error:?}");
}

#[test]
fn session_mutation_fields_and_revert_commit_are_atomic() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "root").at(1_000))
        .expect("create");
    {
        let connection = pool.get().expect("check out a connection");
        for (index, id) in ["msg_1", "msg_2", "msg_3"].iter().enumerate() {
            let seq = i64::try_from(index + 1).expect("small sequence");
            connection
                .execute(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data) \
                     VALUES (?1, 'ses_a', ?2, ?2, '{\"role\":\"user\"}')",
                    rusqlite::params![id, seq],
                )
                .expect("insert legacy message");
            insert_session_message(&connection, id, "ses_a", seq);
            insert_session_input(&connection, &format!("input_{seq}"), "ses_a", seq);
        }
        insert_context_epoch(&connection, "ses_a");
    }

    store
        .switch_agent_at("ses_a", "msg_agent", "explore", 2_000)
        .expect("set agent");
    let model = session::model_reference("provider", "model");
    store
        .switch_model_at("ses_a", "msg_model", &model, 3_000)
        .expect("set model");
    store
        .clear_revert_at("ses_a", 3_500)
        .expect("clearing an unstaged revert is a no-op");
    assert_eq!(
        store.get("ses_a").expect("session").time_updated,
        3_000,
        "an unstaged clear must not make the session observably newer"
    );

    let revert = r#"{"messageID":"msg_2"}"#;
    store
        .stage_revert_at("ses_a", "msg_2", revert, 4_000)
        .expect("stage revert");
    assert_eq!(
        store.get("ses_a").expect("session").revert.as_deref(),
        Some(revert)
    );
    assert!(
        store
            .commit_revert_at("ses_a", 5_000)
            .expect("commit revert")
    );

    let connection = pool.get().expect("check out a connection");
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM message WHERE session_id = ?1",
            "ses_a"
        ),
        2
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM session_message WHERE session_id = ?1",
            "ses_a"
        ),
        2
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM session_input WHERE session_id = ?1",
            "ses_a"
        ),
        2
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM session_context_epoch WHERE session_id = ?1",
            "ses_a"
        ),
        0
    );
    drop(connection);
    let session = store.get("ses_a").expect("session");
    assert_eq!(session.agent.as_deref(), Some("explore"));
    assert_eq!(session.model.as_deref(), Some(model.as_str()));
    assert_eq!(session.revert, None);
    assert_eq!(session.time_updated, 5_000);
    assert!(
        !store
            .commit_revert_at("ses_a", 6_000)
            .expect("guarded no-op")
    );
}

#[test]
fn staging_revert_requires_a_real_boundary_message() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "root"))
        .expect("create");
    let error = store
        .stage_revert_at(
            "ses_a",
            "msg_missing",
            r#"{"messageID":"msg_missing"}"#,
            2_000,
        )
        .expect_err("missing boundary must fail");
    assert!(matches!(error, DbError::NotFound { ref table, .. } if table == "message"));
    assert_eq!(store.get("ses_a").expect("session").revert, None);
}

#[test]
fn setting_a_session_title_updates_the_row_and_its_activity_time() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "old title").at(1))
        .expect("create");

    let updated = store.set_title("ses_a", "new title").expect("rename");
    let renamed = store.get("ses_a").expect("session");

    assert_eq!(renamed.title, "new title");
    assert_eq!(renamed.time_updated, updated);
    assert!(updated > 1);
}

#[test]
fn setting_session_metadata_persists_an_opaque_runtime_identity() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "child").at(1))
        .expect("create");

    let metadata = r#"{"kind":"zuno.child","schemaVersion":1}"#;
    let updated = store
        .set_metadata("ses_a", metadata)
        .expect("persist session metadata");
    let session = store.get("ses_a").expect("session");

    assert_eq!(session.metadata.as_deref(), Some(metadata));
    assert_eq!(session.time_updated, updated);
    assert!(updated > 1);
}

#[test]
fn session_path_matches_the_oracles_relative_computation() {
    assert_eq!(
        session_path(Path::new(WORKTREE), Path::new("/srv/app/pkg/core")),
        "pkg/core"
    );
    assert_eq!(session_path(Path::new(WORKTREE), Path::new(WORKTREE)), "");
    assert_eq!(
        session_path(Path::new(WORKTREE), Path::new("/srv/elsewhere")),
        "../elsewhere"
    );
}

// ---------------------------------------------------------------------------
// The three list scopes
// ---------------------------------------------------------------------------

fn listing_pool() -> Pool {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, Some("app"));
        insert_project(&connection, "prj_b", "/srv/other", None);
    }
    let store = Store::new(&pool);
    // prj_a, three subpaths and the root.
    store
        .create(&draft("ses_a_root", "prj_a", WORKTREE, "a root").at(500))
        .expect("create");
    store
        .create(&draft("ses_a_pkg", "prj_a", "/srv/app/pkg", "a pkg").at(400))
        .expect("create");
    store
        .create(&draft("ses_a_pkg_core", "prj_a", "/srv/app/pkg/core", "a pkg core").at(300))
        .expect("create");
    store
        .create(&draft("ses_a_pkgx", "prj_a", "/srv/app/pkgx", "a pkgx").at(200))
        .expect("create");
    // prj_b.
    store
        .create(&draft("ses_b_root", "prj_b", "/srv/other", "b root").at(600))
        .expect("create");
    pool
}

fn ids(sessions: &[session::Session]) -> Vec<&str> {
    sessions.iter().map(|session| session.id.as_str()).collect()
}

#[test]
fn the_global_scope_returns_every_session_across_every_project() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    let rows = store.list(&ListQuery::global()).expect("list globally");
    assert_eq!(
        ids(&rows),
        vec![
            "ses_b_root",
            "ses_a_root",
            "ses_a_pkg",
            "ses_a_pkg_core",
            "ses_a_pkgx",
        ]
    );
}

#[test]
fn the_directory_scope_matches_one_directory_exactly() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    let rows = store
        .list(&ListQuery::directory("/srv/app/pkg"))
        .expect("list by directory");
    assert_eq!(ids(&rows), vec!["ses_a_pkg"]);

    let none = store
        .list(&ListQuery::directory("/srv/app/pk"))
        .expect("list by a directory prefix");
    assert!(
        none.is_empty(),
        "the directory scope is exact, not a prefix: {:?}",
        ids(&none)
    );
}

#[test]
fn the_project_scope_returns_the_whole_project_when_no_subpath_is_given() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    let rows = store
        .list(&ListQuery::project("prj_a"))
        .expect("list by project");
    assert_eq!(
        ids(&rows),
        vec!["ses_a_root", "ses_a_pkg", "ses_a_pkg_core", "ses_a_pkgx"]
    );
}

#[test]
fn the_project_scope_with_a_subpath_actually_filters() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    let query = ListQuery::project("prj_a").with_subpath("pkg");
    assert!(query.subpath_applies());

    let rows = store.list(&query).expect("list by project and subpath");
    assert_eq!(
        ids(&rows),
        vec!["ses_a_pkg", "ses_a_pkg_core"],
        "a subpath must return that directory and its descendants, and nothing else"
    );

    let unfiltered = store
        .list(&ListQuery::project("prj_a"))
        .expect("list by project");
    assert_eq!(
        unfiltered.len(),
        4,
        "the same project without a subpath returns more rows, so the filter is doing work"
    );
    assert!(
        !ids(&rows).contains(&"ses_a_pkgx"),
        "`pkgx` shares a prefix with `pkg` but is not beneath it"
    );
    assert!(
        !ids(&rows).contains(&"ses_a_root"),
        "the worktree root stores the empty path and is not beneath `pkg`"
    );
}

#[test]
fn a_subpath_naming_a_leaf_returns_only_that_leaf() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    let rows = store
        .list(&ListQuery::project("prj_a").with_subpath("pkg/core"))
        .expect("list by project and subpath");
    assert_eq!(ids(&rows), vec!["ses_a_pkg_core"]);
}

#[test]
fn a_subpath_never_crosses_a_project_boundary() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
        insert_project(&connection, "prj_b", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", "/srv/app/pkg", "a").at(100))
        .expect("create");
    store
        .create(&draft("ses_b", "prj_b", "/srv/app/pkg", "b").at(200))
        .expect("create");

    let rows = store
        .list(&ListQuery::project("prj_a").with_subpath("pkg"))
        .expect("list");
    assert_eq!(ids(&rows), vec!["ses_a"]);
}

#[test]
fn a_subpath_containing_a_like_wildcard_is_not_treated_as_a_pattern() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_under", "prj_a", "/srv/app/a_b/x", "underscore").at(100))
        .expect("create");
    store
        .create(&draft("ses_any", "prj_a", "/srv/app/axb/x", "wildcard match").at(200))
        .expect("create");

    let rows = store
        .list(&ListQuery::project("prj_a").with_subpath("a_b"))
        .expect("list");
    assert_eq!(
        ids(&rows),
        vec!["ses_under"],
        "`_` is a LIKE wildcard; the subpath predicate must not read it as one"
    );
}

#[test]
fn the_three_scopes_are_mutually_exclusive_by_construction() {
    // The scope is one enum value, so there is no way to ask for two at once —
    // and a subpath handed to a non-project scope is dropped rather than
    // silently reinterpreted.
    let directory = ListQuery::directory("/srv/app").with_subpath("pkg");
    assert_eq!(
        directory.scope,
        ListScope::Directory {
            directory: String::from("/srv/app")
        }
    );
    assert!(!directory.subpath_applies());

    let global = ListQuery::global().with_subpath("pkg");
    assert_eq!(global.scope, ListScope::Global);
    assert!(!global.subpath_applies());
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[test]
fn the_default_order_is_updated_descending_with_id_as_the_tie_break() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    // Three sessions sharing one millisecond, so only the id tie-break can
    // order them, plus one that is strictly newer.
    for id in ["ses_a", "ses_b", "ses_c"] {
        store
            .create(&draft(id, "prj_a", WORKTREE, id).at(1_000))
            .expect("create");
    }
    store
        .create(&draft("ses_d", "prj_a", WORKTREE, "newer").at(2_000))
        .expect("create");

    let rows = store.list(&ListQuery::global()).expect("list");
    assert_eq!(ids(&rows), vec!["ses_d", "ses_c", "ses_b", "ses_a"]);
}

#[test]
fn a_touch_moves_a_session_to_the_front_of_the_default_order() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    store.touch_at("ses_a_pkgx", 9_000).expect("touch");
    let rows = store.list(&ListQuery::global()).expect("list");
    assert_eq!(ids(&rows)[0], "ses_a_pkgx");
}

#[test]
fn the_created_sort_is_opt_in_and_ignores_time_updated() {
    let pool = listing_pool();
    let store = Store::new(&pool);
    // `ses_a_pkgx` is the oldest by creation; touching it moves it to the front
    // of the default order but must not move it under the created sort.
    store.touch_at("ses_a_pkgx", 9_000).expect("touch");

    let updated = store.list(&ListQuery::global()).expect("list by updated");
    assert_eq!(ids(&updated)[0], "ses_a_pkgx");

    let created = store
        .list(&ListQuery::global().created_order())
        .expect("list by created");
    assert_eq!(
        ids(&created),
        vec![
            "ses_b_root",
            "ses_a_root",
            "ses_a_pkg",
            "ses_a_pkg_core",
            "ses_a_pkgx",
        ]
    );
}

#[test]
fn the_ascending_direction_reverses_both_the_sort_column_and_the_tie_break() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    for id in ["ses_a", "ses_b"] {
        store
            .create(&draft(id, "prj_a", WORKTREE, id).at(1_000))
            .expect("create");
    }
    let rows = store
        .list(&ListQuery {
            direction: SortDirection::Ascending,
            ..ListQuery::global()
        })
        .expect("list");
    assert_eq!(ids(&rows), vec!["ses_a", "ses_b"]);
}

// ---------------------------------------------------------------------------
// The narrowing filters
// ---------------------------------------------------------------------------

#[test]
fn the_narrowing_filters_each_reduce_the_result() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_root", "prj_a", WORKTREE, "review the parser").at(100))
        .expect("create");
    store
        .create(
            &draft("ses_child", "prj_a", WORKTREE, "child work")
                .with_parent("ses_root")
                .at(200),
        )
        .expect("create");
    store
        .create(&draft("ses_late", "prj_a", WORKTREE, "later review").at(900))
        .expect("create");
    {
        let connection = pool.get().expect("check out a connection");
        connection
            .execute(
                "UPDATE session SET time_archived = 950 WHERE id = 'ses_late'",
                [],
            )
            .expect("archive one session");
    }

    let roots = store
        .list(&ListQuery {
            roots: true,
            ..ListQuery::global()
        })
        .expect("list roots");
    assert_eq!(ids(&roots), vec!["ses_late", "ses_root"]);

    let searched = store
        .list(&ListQuery {
            search: Some(String::from("review")),
            ..ListQuery::global()
        })
        .expect("list by search");
    assert_eq!(ids(&searched), vec!["ses_late", "ses_root"]);

    let started = store
        .list(&ListQuery {
            start: Some(200),
            ..ListQuery::global()
        })
        .expect("list from a lower bound");
    assert_eq!(ids(&started), vec!["ses_late", "ses_child"]);

    let cursored = store
        .list(&ListQuery {
            cursor: Some(200),
            ..ListQuery::global()
        })
        .expect("list before a cursor");
    assert_eq!(ids(&cursored), vec!["ses_root"]);

    let limited = store
        .list(&ListQuery::global().with_limit(1))
        .expect("list with a limit");
    assert_eq!(ids(&limited), vec!["ses_late"]);

    let active = store
        .list(&ListQuery::global().active_only())
        .expect("list active only");
    assert_eq!(ids(&active), vec!["ses_child", "ses_root"]);

    let archived = store
        .list(&ListQuery {
            archived: ArchivedFilter::Archived,
            ..ListQuery::global()
        })
        .expect("list archived only");
    assert_eq!(ids(&archived), vec!["ses_late"]);

    let any = store.list(&ListQuery::global()).expect("list everything");
    assert_eq!(
        any.len(),
        3,
        "the default hides nothing, archived rows included"
    );
}

#[test]
fn the_workspace_filter_applies_to_every_scope() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
        connection
            .execute(
                "INSERT INTO workspace (id, type, name, project_id, time_used) \
                 VALUES ('wrk_a', 'local', 'a', 'prj_a', 1)",
                [],
            )
            .expect("insert workspace");
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_plain", "prj_a", WORKTREE, "plain").at(100))
        .expect("create");
    store
        .create(
            &draft("ses_scoped", "prj_a", WORKTREE, "scoped")
                .with_workspace("wrk_a")
                .at(200),
        )
        .expect("create");

    let rows = store
        .list(&ListQuery {
            workspace_id: Some(String::from("wrk_a")),
            ..ListQuery::project("prj_a")
        })
        .expect("list");
    assert_eq!(ids(&rows), vec!["ses_scoped"]);
}

// ---------------------------------------------------------------------------
// Global listing with project summaries — the happy QA scenario
// ---------------------------------------------------------------------------

#[test]
fn a_global_listing_across_three_projects_carries_each_project_summary() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", "/srv/a", Some("Alpha"));
        insert_project(&connection, "prj_b", "/srv/b", Some("Beta"));
        // A project with no name, so the summary's optional name is exercised.
        insert_project(&connection, "prj_c", "/srv/c", None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", "/srv/a", "alpha work").at(100))
        .expect("create");
    store
        .create(&draft("ses_b", "prj_b", "/srv/b", "beta work").at(200))
        .expect("create");
    store
        .create(&draft("ses_c", "prj_c", "/srv/c", "gamma work").at(300))
        .expect("create");

    let rows = store
        .list_global(&ListQuery::global())
        .expect("list globally with summaries");
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|row| row.session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["ses_c", "ses_b", "ses_a"]
    );

    let summaries = rows
        .iter()
        .map(|row| {
            let project = row.project.as_ref().expect("a project summary");
            (
                row.session.id.clone(),
                project.id.clone(),
                project.name.clone(),
                project.worktree.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summaries,
        vec![
            (
                String::from("ses_c"),
                String::from("prj_c"),
                None,
                String::from("/srv/c")
            ),
            (
                String::from("ses_b"),
                String::from("prj_b"),
                Some(String::from("Beta")),
                String::from("/srv/b")
            ),
            (
                String::from("ses_a"),
                String::from("prj_a"),
                Some(String::from("Alpha")),
                String::from("/srv/a")
            ),
        ]
    );
}

#[test]
fn a_global_listing_of_nothing_asks_for_no_project_summaries() {
    let pool = pool();
    let store = Store::new(&pool);
    let rows = store.list_global(&ListQuery::global()).expect("list");
    assert!(rows.is_empty());
}

// ---------------------------------------------------------------------------
// The subtree delete
// ---------------------------------------------------------------------------

#[test]
fn children_returns_only_the_immediate_generation() {
    let tree = Tree::build();
    let store = tree.store();
    let kids = store.children("ses_root").expect("read children");
    assert_eq!(ids(&kids), vec!["ses_child"]);
    let grandkids = store.children("ses_child").expect("read grandchildren");
    assert_eq!(ids(&grandkids), vec!["ses_grandchild"]);
    assert!(
        store
            .children("ses_grandchild")
            .expect("read leaves")
            .is_empty()
    );
}

#[test]
fn the_subtree_walk_returns_children_before_their_parent() {
    let tree = Tree::build();
    let store = tree.store();
    assert_eq!(
        store.subtree("ses_root").expect("walk the subtree"),
        vec!["ses_grandchild", "ses_child", "ses_root"],
        "post-order, matching session.ts:619-622 recursing into children first"
    );
    assert_eq!(
        store.subtree("ses_child").expect("walk from the middle"),
        vec!["ses_grandchild", "ses_child"]
    );
    assert_eq!(
        store.subtree("ses_bystander").expect("walk a leaf"),
        vec!["ses_bystander"]
    );
}

#[test]
fn removing_a_parent_removes_the_whole_subtree_and_leaves_no_orphaned_parts() {
    let tree = Tree::build();
    let store = tree.store();

    let before = {
        let connection = tree.connection();
        (
            count(&connection, "SELECT count(*) FROM session"),
            count(&connection, "SELECT count(*) FROM message"),
            count(&connection, "SELECT count(*) FROM part"),
            count(&connection, "SELECT count(*) FROM work_plan"),
            count(&connection, "SELECT count(*) FROM work_item"),
            count(&connection, "SELECT count(*) FROM session_message"),
            count(&connection, "SELECT count(*) FROM session_input"),
            count(&connection, "SELECT count(*) FROM session_context_epoch"),
            count(&connection, "SELECT count(*) FROM session_share"),
            count(&connection, "SELECT count(*) FROM event_sequence"),
            count(&connection, "SELECT count(*) FROM event"),
        )
    };
    assert_eq!(
        before,
        (4, 4, 5, 4, 4, 4, 4, 4, 1, 4, 8),
        "fixture row counts before the delete"
    );

    let removed = store.remove("ses_root").expect("remove the subtree");
    assert_eq!(
        removed,
        vec!["ses_grandchild", "ses_child", "ses_root"],
        "every id in the subtree is reported, deepest first"
    );

    let connection = tree.connection();

    // The subtree is gone, and nothing points at a session that no longer
    // exists.
    assert_eq!(
        count(&connection, "SELECT count(*) FROM session"),
        1,
        "only the bystander survives"
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM session WHERE id IN ('ses_root', 'ses_child', 'ses_grandchild')"
        ),
        0,
        "zero descendants"
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM session s WHERE s.parent_id IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM session p WHERE p.id = s.parent_id)"
        ),
        0,
        "no session is left pointing at a missing parent"
    );

    // The orphan sweep. `prt_orphan` names a message that still exists, so no
    // cascade could have removed it.
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM part p WHERE NOT EXISTS \
             (SELECT 1 FROM session s WHERE s.id = p.session_id)"
        ),
        0,
        "zero orphaned part rows"
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM part WHERE id = ?1",
            "prt_orphan"
        ),
        0,
        "the part the cascade could not reach was swept explicitly"
    );

    // The declared cascades did their part.
    for table in [
        "message",
        "work_plan",
        "work_item",
        "session_message",
        "session_input",
        "session_context_epoch",
        "session_share",
    ] {
        let remaining = count_for(
            &connection,
            &format!("SELECT count(*) FROM {table} WHERE session_id = ?1"),
            "ses_child",
        );
        assert_eq!(remaining, 0, "{table} still holds rows for ses_child");
    }

    // The event log, which no cascade reaches from `session`.
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM event_sequence WHERE aggregate_id IN \
             ('ses_root', 'ses_child', 'ses_grandchild')"
        ),
        0,
        "zero event_sequence rows left behind"
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM event WHERE aggregate_id IN \
             ('ses_root', 'ses_child', 'ses_grandchild')"
        ),
        0,
        "zero event rows left behind"
    );

    // The bystander is untouched, including its own event log and the message
    // the swept orphan pointed at.
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM session WHERE id = ?1",
            "ses_bystander"
        ),
        1
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM part WHERE session_id = ?1",
            "ses_bystander"
        ),
        1
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM message WHERE session_id = ?1",
            "ses_bystander"
        ),
        1
    );
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM event WHERE aggregate_id = ?1",
            "ses_bystander"
        ),
        2
    );

    let after = (
        count(&connection, "SELECT count(*) FROM session"),
        count(&connection, "SELECT count(*) FROM message"),
        count(&connection, "SELECT count(*) FROM part"),
        count(&connection, "SELECT count(*) FROM work_plan"),
        count(&connection, "SELECT count(*) FROM work_item"),
        count(&connection, "SELECT count(*) FROM session_message"),
        count(&connection, "SELECT count(*) FROM session_input"),
        count(&connection, "SELECT count(*) FROM session_context_epoch"),
        count(&connection, "SELECT count(*) FROM session_share"),
        count(&connection, "SELECT count(*) FROM event_sequence"),
        count(&connection, "SELECT count(*) FROM event"),
    );
    assert_eq!(
        after,
        (1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 2),
        "row counts after the delete: only the bystander's rows remain"
    );
}

#[test]
fn removing_a_middle_session_keeps_its_parent_and_takes_its_child() {
    let tree = Tree::build();
    let store = tree.store();
    let removed = store.remove("ses_child").expect("remove the middle");
    assert_eq!(removed, vec!["ses_grandchild", "ses_child"]);

    let connection = tree.connection();
    assert_eq!(
        count_for(
            &connection,
            "SELECT count(*) FROM session WHERE id = ?1",
            "ses_root"
        ),
        1,
        "the parent above the removed node survives"
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM part p WHERE NOT EXISTS \
             (SELECT 1 FROM session s WHERE s.id = p.session_id)"
        ),
        0
    );
}

#[test]
fn an_age_based_delete_that_matched_only_the_parent_is_rejected_in_favour_of_the_subtree() {
    // The failure QA scenario. A retention pass that reasons "this session has
    // not been touched since T, delete it" matches the parent alone, because
    // the parent is the *oldest* row in the tree — its children were touched
    // later. Run as SQL it succeeds and orphans them.
    const AGE_PREDICATE: &str = "time_updated < 250";

    let naive = Tree::build();
    {
        let connection = naive.connection();
        let matched = connection
            .execute(&format!("DELETE FROM session WHERE {AGE_PREDICATE}"), [])
            .expect("the naive age-based delete succeeds");
        assert_eq!(
            matched, 2,
            "the age predicate matched the root and the child but not the grandchild"
        );
        assert_eq!(
            count(
                &connection,
                "SELECT count(*) FROM session s WHERE s.parent_id IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM session p WHERE p.id = s.parent_id)"
            ),
            1,
            "ses_grandchild now points at a parent that does not exist"
        );
        assert_eq!(
            count(
                &connection,
                "SELECT count(*) FROM part p WHERE NOT EXISTS \
                 (SELECT 1 FROM session s WHERE s.id = p.session_id)"
            ),
            1,
            "and prt_orphan survived, with a session_id naming nothing"
        );
        assert_eq!(
            count(
                &connection,
                "SELECT count(*) FROM event WHERE aggregate_id IN ('ses_root', 'ses_child')"
            ),
            4,
            "and the event log for both deleted sessions is still there"
        );
    }

    let served = Tree::build();
    let store = served.store();
    let roots_the_same_age_predicate_selects: Vec<String> = {
        let connection = served.connection();
        let mut statement = connection
            .prepare(&format!(
                "SELECT id FROM session WHERE {AGE_PREDICATE} AND parent_id IS NULL"
            ))
            .expect("prepare the age query");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("run the age query");
        rows.collect::<Result<Vec<_>, _>>().expect("read ids")
    };
    assert_eq!(
        roots_the_same_age_predicate_selects,
        vec![String::from("ses_root")]
    );

    let removed = store
        .remove("ses_root")
        .expect("remove through the service");
    assert_eq!(
        removed,
        vec!["ses_grandchild", "ses_child", "ses_root"],
        "the service removed the grandchild the raw DELETE would have stranded"
    );

    let connection = served.connection();
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM session s WHERE s.parent_id IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM session p WHERE p.id = s.parent_id)"
        ),
        0,
        "no orphaned session"
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM part p WHERE NOT EXISTS \
             (SELECT 1 FROM session s WHERE s.id = p.session_id)"
        ),
        0,
        "no orphaned part"
    );
}

#[test]
fn removing_a_missing_session_reports_not_found_and_changes_nothing() {
    let tree = Tree::build();
    let store = tree.store();
    let error = store.remove("ses_nope").expect_err("must not be found");
    match error {
        DbError::NotFound { table, id } => {
            assert_eq!(table, "session");
            assert_eq!(id, "ses_nope");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
    let connection = tree.connection();
    assert_eq!(count(&connection, "SELECT count(*) FROM session"), 4);
}

#[test]
fn a_failed_remove_rolls_the_whole_subtree_back() {
    let tree = Tree::build();
    let error = tree
        .pool
        .transaction(|transaction| {
            session::remove(transaction, "ses_root")?;
            // Fail after the deletes have run but before the commit.
            Err::<(), DbError>(DbError::NotFound {
                table: String::from("sentinel"),
                id: String::from("sentinel"),
            })
        })
        .expect_err("the transaction must fail");
    assert!(matches!(error, DbError::NotFound { .. }), "{error:?}");

    let connection = tree.connection();
    assert_eq!(
        count(&connection, "SELECT count(*) FROM session"),
        4,
        "a subtree delete is one unit: none of it survives a rollback"
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM part"), 5);
    assert_eq!(count(&connection, "SELECT count(*) FROM event"), 8);
}

#[test]
fn a_parent_id_cycle_terminates_instead_of_recursing_forever() {
    // `parent_id` has no foreign key, so nothing in the schema prevents a
    // corrupted pair from pointing at each other. A recursive walk would
    // overflow the stack; the iterative one with a visited set must not.
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_a", "prj_a", WORKTREE, "a").at(100))
        .expect("create");
    store
        .create(
            &draft("ses_b", "prj_a", WORKTREE, "b")
                .with_parent("ses_a")
                .at(200),
        )
        .expect("create");
    {
        let connection = pool.get().expect("check out a connection");
        connection
            .execute(
                "UPDATE session SET parent_id = 'ses_b' WHERE id = 'ses_a'",
                [],
            )
            .expect("introduce a cycle");
    }

    let walk = store.subtree("ses_a").expect("walk a cycle");
    assert_eq!(walk, vec!["ses_b", "ses_a"]);

    let removed = store.remove("ses_a").expect("remove a cycle");
    assert_eq!(removed, vec!["ses_b", "ses_a"]);
    let connection = pool.get().expect("check out a connection");
    assert_eq!(count(&connection, "SELECT count(*) FROM session"), 0);
}

#[test]
fn a_wide_and_deep_subtree_is_removed_completely() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let store = Store::new(&pool);
    store
        .create(&draft("ses_0", "prj_a", WORKTREE, "root").at(0))
        .expect("create root");
    // Five levels, two children each: 1 + 2 + 4 + 8 + 16 = 31 sessions.
    let mut frontier = vec![String::from("ses_0")];
    let mut total = 1;
    for level in 1..5 {
        let mut next = Vec::new();
        for (index, parent) in frontier.iter().enumerate() {
            for branch in 0..2 {
                let id = format!("ses_{level}_{index}_{branch}");
                store
                    .create(&draft(&id, "prj_a", WORKTREE, &id).with_parent(parent).at(0))
                    .expect("create child");
                next.push(id);
                total += 1;
            }
        }
        frontier = next;
    }
    assert_eq!(total, 31);

    let removed = store.remove("ses_0").expect("remove the tree");
    assert_eq!(removed.len(), 31);
    assert_eq!(
        removed.last().map(String::as_str),
        Some("ses_0"),
        "the root is removed last"
    );
    let connection = pool.get().expect("check out a connection");
    assert_eq!(count(&connection, "SELECT count(*) FROM session"), 0);
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

#[test]
fn the_free_functions_compose_inside_a_callers_own_transaction() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_a", WORKTREE, None);
    }
    let listed = pool
        .transaction(|transaction| {
            session::create(transaction, &draft("ses_a", "prj_a", WORKTREE, "a").at(100))?;
            session::create(
                transaction,
                &draft("ses_b", "prj_a", "/srv/app/pkg", "b").at(200),
            )?;
            session::touch_at(transaction, "ses_a", 300)?;
            let rows = session::list(transaction, &ListQuery::global())?;
            Ok(rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>())
        })
        .expect("run one transaction");
    assert_eq!(listed, vec!["ses_a", "ses_b"]);

    let store = Store::new(&pool);
    assert_eq!(
        store.get("ses_a").expect("read back").time_updated,
        300,
        "the transaction committed"
    );
}

#[test]
fn the_sort_and_limit_defaults_are_the_documented_ones() {
    let query = ListQuery::default();
    assert_eq!(query.sort, SessionSort::Updated);
    assert_eq!(query.direction, SortDirection::Descending);
    assert_eq!(query.archived, ArchivedFilter::Any);
    assert_eq!(query.limit, None);
    assert_eq!(query.scope, ListScope::Global);
    assert!(!query.roots);
    assert_eq!(session::UPSTREAM_LIST_LIMIT, 100);
}
