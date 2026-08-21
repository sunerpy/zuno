//! The four conditional tools against a real `todo` table and the real binary's
//! measured exposure conditions.
//!
//! # What this file adds over the unit tests
//!
//! `src/exposure.rs`, `src/todo.rs`, `src/question.rs`, `src/invalid.rs` and
//! `src/plan_exit.rs` each carry their own `#[cfg(test)]` module, and those cover the
//! predicates and the rendering against an in-memory store. Three claims cannot be
//! made there and are made here:
//!
//! 1. **`position` ordering is real SQL, not a `Vec` index.** The unit tests use
//!    [`zuno_tools::MemoryTodoStore`], which preserves order because it holds a `Vec`.
//!    Only a query against the actual table proves the column is written and that
//!    `ORDER BY position` recovers the model's array order.
//! 2. **The primary key `(session_id, position)` is satisfied by the replace
//!    strategy.** A whole-list rewrite that did not delete first would raise
//!    `UNIQUE constraint failed`, which an in-memory map cannot detect.
//! 3. **The foreign key to `session` is enforced.** `zuno-db` issues
//!    `PRAGMA foreign_keys = ON`, so a write for an unknown session must fail rather
//!    than orphan rows. Asserted, not assumed from the DDL — the pragma's default
//!    varies by SQLite build, which is precisely why `zuno-db` sets it explicitly.
//!
//! The exposure assertions are repeated here in the shape a differential compares —
//! flag configuration in, tool-id list out — because that is the surface todo 44
//! consumes, and a unit test of the predicates does not exercise it.

use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use zuno_db::{Pool, migration, open};
use zuno_error::ToolError;
use zuno_paths::DbLocation;
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, erase};
use zuno_tools::exposure::{
    ENV_CLIENT, ENV_ENABLE_QUESTION_TOOL, ENV_EXPERIMENTAL, ENV_EXPERIMENTAL_PLAN_MODE,
    ExposureFlags, exposed_conditional_tools,
};
use zuno_tools::invalid::InvalidTool;
use zuno_tools::plan_exit::{PlanExitTool, RecordingHost};
use zuno_tools::question::{QuestionAsker, QuestionTool, ScriptedAnswers};
use zuno_tools::todo::{
    SqliteTodoStore, TodoPriority, TodoStatus, TodoStore, TodoStoreError, TodoWriteTool,
};

const SESSION: &str = "ses_t43000000000000000000000000";
const OTHER_SESSION: &str = "ses_t43000000000000000000000001";

/// A pool on a fresh file database with the real schema and two seeded sessions.
///
/// A file rather than `:memory:` because `Pool` opens a new connection per checkout
/// and a plain in-memory database is per-connection — the second checkout would find
/// no tables. The `TempDir` is returned so the caller keeps it alive; dropping it
/// deletes the database mid-test.
fn seeded_pool() -> (TempDir, Arc<Pool>) {
    let directory = TempDir::new().expect("a temporary directory");
    let path = directory.path().join("opencode.db");

    let mut connection = open::open_at(&path).expect("open the database");
    migration::apply(&mut connection).expect("apply the real schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-t43', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION}', 'project-t43', 'a', '/workspace', 'a', '1', 1, 1);
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{OTHER_SESSION}', 'project-t43', 'b', '/workspace', 'b', '1', 1, 1);"
        ))
        .expect("seed a project and two sessions");
    drop(connection);

    let pool = Arc::new(Pool::open(&DbLocation::File(path)).expect("open a pool"));
    (directory, pool)
}

/// Every `(content, status, priority, position)` for `session_id`, in stored order.
///
/// Read with an explicit `ORDER BY position` because that is the claim under test: a
/// query without it would pass by coincidence on a small table.
fn stored_rows(pool: &Pool, session_id: &str) -> Vec<(String, String, String, i64)> {
    let connection = pool.get().expect("check out a connection");
    let mut statement = connection
        .prepare(
            "SELECT `content`, `status`, `priority`, `position` FROM `todo` \
             WHERE `session_id` = ?1 ORDER BY `position` ASC",
        )
        .expect("prepare the read");
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .expect("run the read");
    rows.map(|row| row.expect("decode a row")).collect()
}

/// Rows in the order SQLite happens to return them, with no `ORDER BY`.
fn rows_in_natural_order(pool: &Pool, session_id: &str) -> Vec<(String, i64)> {
    let connection = pool.get().expect("check out a connection");
    let mut statement = connection
        .prepare("SELECT `content`, `position` FROM `todo` WHERE `session_id` = ?1")
        .expect("prepare the read");
    let rows = statement
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .expect("run the read");
    rows.map(|row| row.expect("decode a row")).collect()
}

fn context(session_id: &str) -> ToolContext {
    ToolContext::new(
        session_id,
        "msg_t43",
        "call_t43",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn todo_tool(pool: &Arc<Pool>) -> Arc<dyn Tool> {
    erase(TodoWriteTool::with_pool(Arc::clone(pool)))
}

/// A list whose statuses and priorities span the whole documented value space.
fn five_todos() -> Value {
    json!({ "todos": [
        { "content": "alpha",   "status": "in_progress", "priority": "high" },
        { "content": "bravo",   "status": "pending",     "priority": "medium" },
        { "content": "charlie", "status": "pending",     "priority": "low" },
        { "content": "delta",   "status": "completed",   "priority": "high" },
        { "content": "echo",    "status": "cancelled",   "priority": "low" },
    ] })
}

/// Renders an error and every cause in its chain, so an assertion can look at the
/// message a reporter would actually show.
fn chain(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str(" -> ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

fn flags(pairs: &[(&str, &str)]) -> ExposureFlags {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    ExposureFlags::from_lookup(|key| {
        owned
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    })
}

// ---------------------------------------------------------------------------
// position ordering, against the real table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_todo_write_persists_rows_with_position_matching_the_array_index() {
    let (_directory, pool) = seeded_pool();

    todo_tool(&pool)
        .execute(five_todos(), context(SESSION))
        .await
        .expect("a valid list");

    let rows = stored_rows(&pool, SESSION);
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows,
        vec![
            (
                "alpha".to_owned(),
                "in_progress".to_owned(),
                "high".to_owned(),
                0
            ),
            (
                "bravo".to_owned(),
                "pending".to_owned(),
                "medium".to_owned(),
                1
            ),
            (
                "charlie".to_owned(),
                "pending".to_owned(),
                "low".to_owned(),
                2
            ),
            (
                "delta".to_owned(),
                "completed".to_owned(),
                "high".to_owned(),
                3
            ),
            (
                "echo".to_owned(),
                "cancelled".to_owned(),
                "low".to_owned(),
                4
            ),
        ],
        "position must be the item's index in the array the model sent"
    );
}

#[tokio::test]
async fn the_position_column_is_what_carries_the_order_not_the_insert_sequence() {
    let (_directory, pool) = seeded_pool();
    todo_tool(&pool)
        .execute(five_todos(), context(SESSION))
        .await
        .expect("a valid list");

    // The positions must be exactly 0..n with no gaps and no duplicates, whatever
    // order the rows come back in. A store that wrote a constant, or reused a
    // position, would break the primary key or lose the order silently.
    let mut positions: Vec<i64> = rows_in_natural_order(&pool, SESSION)
        .into_iter()
        .map(|(_, position)| position)
        .collect();
    positions.sort_unstable();
    assert_eq!(positions, vec![0, 1, 2, 3, 4]);
}

#[tokio::test]
async fn reading_the_list_back_through_the_store_recovers_the_array_order() {
    let (_directory, pool) = seeded_pool();
    todo_tool(&pool)
        .execute(five_todos(), context(SESSION))
        .await
        .expect("a valid list");

    let store = SqliteTodoStore::new(Arc::clone(&pool));
    let items = store.list(SESSION).expect("read the list back");
    let contents: Vec<&str> = items.iter().map(|item| item.content.as_str()).collect();
    assert_eq!(contents, vec!["alpha", "bravo", "charlie", "delta", "echo"]);
    assert_eq!(items[0].status, TodoStatus::InProgress);
    assert_eq!(items[4].status, TodoStatus::Cancelled);
    assert_eq!(items[1].priority, TodoPriority::Medium);
}

#[tokio::test]
async fn a_shorter_second_list_does_not_collide_with_the_primary_key() {
    // `(session_id, position)` is the primary key, so rewriting a 5-item list as a
    // 2-item one only works because the replace deletes first. Without the delete this
    // is `UNIQUE constraint failed: todo.session_id, todo.position`.
    let (_directory, pool) = seeded_pool();
    let tool = todo_tool(&pool);

    tool.execute(five_todos(), context(SESSION))
        .await
        .expect("the long list");
    tool.execute(
        json!({ "todos": [
            { "content": "only",  "status": "pending",   "priority": "high" },
            { "content": "other", "status": "completed", "priority": "low" },
        ] }),
        context(SESSION),
    )
    .await
    .expect("the short list must replace, not collide");

    let rows = stored_rows(&pool, SESSION);
    assert_eq!(rows.len(), 2, "the old rows are gone, not merged");
    assert_eq!(rows[0].0, "only");
    assert_eq!(rows[0].3, 0);
    assert_eq!(rows[1].3, 1);
}

#[tokio::test]
async fn an_empty_list_deletes_every_row_for_the_session() {
    let (_directory, pool) = seeded_pool();
    let tool = todo_tool(&pool);

    tool.execute(five_todos(), context(SESSION))
        .await
        .expect("the long list");
    tool.execute(json!({ "todos": [] }), context(SESSION))
        .await
        .expect("an empty list is a clear, not a no-op");

    assert!(stored_rows(&pool, SESSION).is_empty());
}

#[tokio::test]
async fn a_write_is_scoped_to_its_own_session() {
    let (_directory, pool) = seeded_pool();
    let tool = todo_tool(&pool);

    tool.execute(five_todos(), context(SESSION))
        .await
        .expect("session a");
    tool.execute(
        json!({ "todos": [{ "content": "theirs", "status": "pending", "priority": "high" }] }),
        context(OTHER_SESSION),
    )
    .await
    .expect("session b");

    assert_eq!(stored_rows(&pool, SESSION).len(), 5);
    let other = stored_rows(&pool, OTHER_SESSION);
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].0, "theirs");
}

#[tokio::test]
async fn the_timestamp_columns_are_populated_because_they_are_not_null() {
    let (_directory, pool) = seeded_pool();
    todo_tool(&pool)
        .execute(
            json!({ "todos": [{ "content": "a", "status": "pending", "priority": "high" }] }),
            context(SESSION),
        )
        .await
        .expect("a valid list");

    let connection = pool.get().expect("check out a connection");
    let (created, updated): (i64, i64) = connection
        .query_row(
            "SELECT `time_created`, `time_updated` FROM `todo` WHERE `session_id` = ?1",
            [SESSION],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("both columns are NOT NULL and must have been written");

    assert!(created > 0, "time_created must be a real epoch millisecond");
    assert_eq!(
        created, updated,
        "one timestamp for the batch, as the oracle's insert does"
    );
}

// ---------------------------------------------------------------------------
// the foreign key is enforced, not decorative
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_write_for_an_unknown_session_is_refused_by_the_foreign_key() {
    let (_directory, pool) = seeded_pool();

    let error = todo_tool(&pool)
        .execute(
            json!({ "todos": [{ "content": "orphan", "status": "pending", "priority": "high" }] }),
            context("ses_does_not_exist"),
        )
        .await
        .expect_err("todo.session_id references session(id)");

    assert!(matches!(error, ToolError::Failed { .. }));
    assert_eq!(error.tool(), "todowrite");
    let rendered = chain(&error).to_ascii_lowercase();
    assert!(
        rendered.contains("foreign key"),
        "the failure must name the constraint that refused it: {rendered}"
    );
    assert!(stored_rows(&pool, "ses_does_not_exist").is_empty());
}

#[test]
fn the_foreign_keys_pragma_is_actually_on_for_a_pooled_connection() {
    // Without this the test above would pass for the wrong reason on one SQLite build
    // and fail on another. `zuno-db` sets the pragma explicitly; this confirms the value
    // a checked-out connection sees.
    let (_directory, pool) = seeded_pool();
    let connection = pool.get().expect("check out a connection");
    let enabled: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .expect("read the pragma back");
    assert_eq!(enabled, 1);
}

#[tokio::test]
async fn deleting_a_session_cascades_its_todos_away() {
    let (_directory, pool) = seeded_pool();
    todo_tool(&pool)
        .execute(five_todos(), context(SESSION))
        .await
        .expect("a valid list");
    assert_eq!(stored_rows(&pool, SESSION).len(), 5);

    pool.transaction(|transaction| {
        transaction
            .execute("DELETE FROM `session` WHERE `id` = ?1", [SESSION])
            .map_err(zuno_db::map_error)?;
        Ok(())
    })
    .expect("delete the session");

    assert!(
        stored_rows(&pool, SESSION).is_empty(),
        "ON DELETE CASCADE must take the todos with the session"
    );
}

// ---------------------------------------------------------------------------
// the priority-0 rejection, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conditional_a_numeric_priority_is_refused_naming_the_allowed_strings() {
    let (_directory, pool) = seeded_pool();

    let error = todo_tool(&pool)
        .execute(
            json!({ "todos": [{ "content": "a", "status": "pending", "priority": 0 }] }),
            context(SESSION),
        )
        .await
        .expect_err("the schema is string-valued; 0 is not a priority");

    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert!(error.is_model_correctable());

    let rendered = chain(&error);
    for allowed in TodoPriority::ALLOWED {
        assert!(
            rendered.contains(allowed),
            "the rejection must name {allowed}: {rendered}"
        );
    }
    assert!(
        stored_rows(&pool, SESSION).is_empty(),
        "a rejected call must write nothing"
    );

    // The literal message, recorded in the evidence file.
    println!("priority-0 rejection: {rendered}");
}

#[tokio::test]
async fn a_numeric_status_is_refused_the_same_way() {
    let (_directory, pool) = seeded_pool();

    let error = todo_tool(&pool)
        .execute(
            json!({ "todos": [{ "content": "a", "status": 1, "priority": "high" }] }),
            context(SESSION),
        )
        .await
        .expect_err("status is string-valued too");

    let rendered = chain(&error);
    for allowed in TodoStatus::ALLOWED {
        assert!(
            rendered.contains(allowed),
            "the rejection must name {allowed}: {rendered}"
        );
    }
}

#[test]
fn a_stored_value_this_port_would_not_write_is_named_rather_than_coerced() {
    // The TypeScript binary accepts any string in these columns, so a shared database
    // can hold values the enums refuse. Reading such a row must say which column and
    // which value, not guess.
    let (_directory, pool) = seeded_pool();
    pool.transaction(|transaction| {
        transaction
            .execute(
                "INSERT INTO `todo` \
                 (`session_id`, `content`, `status`, `priority`, `position`, `time_created`, `time_updated`) \
                 VALUES (?1, 'a', 'pending', 'urgent', 0, 1, 1)",
                [SESSION],
            )
            .map_err(zuno_db::map_error)?;
        Ok(())
    })
    .expect("write a row the TypeScript binary would accept");

    let error = SqliteTodoStore::new(Arc::clone(&pool))
        .list(SESSION)
        .expect_err("urgent is not one of high, medium, low");

    match error {
        TodoStoreError::UnknownValue {
            field,
            value,
            position,
        } => {
            assert_eq!(field, "priority");
            assert_eq!(value, "urgent");
            assert_eq!(position, 0);
        }
        other => panic!("expected an unknown-value failure, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// exposure, in the shape todo 44's differential compares
// ---------------------------------------------------------------------------

/// One invocation of the real binary: the environment it ran under, and the four
/// tools this task owns that appeared in its resolved `tools` map.
type MeasuredCase = (
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

#[test]
fn conditional_invalid_is_offered_in_every_configuration() {
    for configuration in [
        flags(&[]),
        flags(&[(ENV_CLIENT, "tui")]),
        flags(&[(ENV_CLIENT, "")]),
        flags(&[(ENV_EXPERIMENTAL, "true")]),
        flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "true")]),
    ] {
        assert!(
            exposed_conditional_tools(&configuration).contains(&"invalid"),
            "invalid must be offered for {configuration:?}"
        );
    }
}

#[test]
fn conditional_todowrite_is_offered_in_every_configuration() {
    for configuration in [
        flags(&[]),
        flags(&[(ENV_CLIENT, "tui")]),
        flags(&[(ENV_CLIENT, "unknown")]),
        flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "true")]),
    ] {
        assert!(
            exposed_conditional_tools(&configuration).contains(&"todowrite"),
            "todowrite must be offered for {configuration:?}"
        );
    }
}

#[test]
fn conditional_question_is_offered_only_to_an_interactive_client_or_under_its_flag() {
    // Present: the three interactive clients, and any client with the override.
    for client in ["cli", "app", "desktop"] {
        assert!(
            exposed_conditional_tools(&flags(&[(ENV_CLIENT, client)])).contains(&"question"),
            "{client} must be offered question"
        );
    }
    assert!(
        exposed_conditional_tools(&flags(&[
            (ENV_CLIENT, "tui"),
            (ENV_ENABLE_QUESTION_TOOL, "true"),
        ]))
        .contains(&"question")
    );

    // Absent: a client that cannot render one, without the override.
    for client in ["tui", "CLI", "", "headless"] {
        assert!(
            !exposed_conditional_tools(&flags(&[(ENV_CLIENT, client)])).contains(&"question"),
            "{client:?} must not be offered question"
        );
    }
}

#[test]
fn conditional_plan_exit_is_offered_only_under_plan_mode_with_a_cli_client() {
    // Present: the task's happy path.
    assert!(
        exposed_conditional_tools(&flags(&[
            (ENV_CLIENT, "cli"),
            (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
        ]))
        .contains(&"plan_exit")
    );

    // Absent, first half of the conjunction: a CLI client without plan mode.
    assert!(!exposed_conditional_tools(&flags(&[(ENV_CLIENT, "cli")])).contains(&"plan_exit"));

    // Absent, second half: plan mode on a client that is not the CLI.
    for client in ["tui", "app", "desktop", "CLI", ""] {
        assert!(
            !exposed_conditional_tools(&flags(&[
                (ENV_CLIENT, client),
                (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
            ]))
            .contains(&"plan_exit"),
            "{client:?} must not be offered plan_exit"
        );
    }
}

#[test]
fn conditional_the_exposed_set_matches_the_measured_binary_for_each_case() {
    // Each row is one invocation of the real 1.18.12 binary, filtered to the four
    // tools this task owns. The full transcript, including the tools other tasks own,
    // is in `.omo/evidence/task-43-opencode-rust.txt`.
    let cases: &[MeasuredCase] = &[
        // case 1: a bare invocation.
        (&[], &["invalid", "question", "todowrite"]),
        // case 2: ZUNO_CLIENT=tui.
        (&[(ENV_CLIENT, "tui")], &["invalid", "todowrite"]),
        // case 3: tui with the question override.
        (
            &[(ENV_CLIENT, "tui"), (ENV_ENABLE_QUESTION_TOOL, "true")],
            &["invalid", "question", "todowrite"],
        ),
        // cases 4 and 5: app and desktop.
        (
            &[(ENV_CLIENT, "app")],
            &["invalid", "question", "todowrite"],
        ),
        (
            &[(ENV_CLIENT, "desktop")],
            &["invalid", "question", "todowrite"],
        ),
        // case 8: plan mode on the CLI.
        (
            &[(ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "question", "todowrite", "plan_exit"],
        ),
        // case 9: plan mode on a tui host.
        (
            &[(ENV_CLIENT, "tui"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "todowrite"],
        ),
        // case 10: plan mode on an app host — question yes, plan_exit no.
        (
            &[(ENV_CLIENT, "app"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "question", "todowrite"],
        ),
        // case 12: the blanket switch turns plan mode on.
        (
            &[(ENV_EXPERIMENTAL, "true")],
            &["invalid", "question", "todowrite", "plan_exit"],
        ),
        // case 13: an explicit false beats the blanket switch.
        (
            &[
                (ENV_EXPERIMENTAL, "true"),
                (ENV_EXPERIMENTAL_PLAN_MODE, "false"),
            ],
            &["invalid", "question", "todowrite"],
        ),
        // case 14: an explicit true survives a false blanket switch.
        (
            &[
                (ENV_EXPERIMENTAL, "false"),
                (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
            ],
            &["invalid", "question", "todowrite", "plan_exit"],
        ),
        // cases 15 and 16: numeric spellings.
        (
            &[(ENV_EXPERIMENTAL_PLAN_MODE, "1")],
            &["invalid", "question", "todowrite", "plan_exit"],
        ),
        (
            &[(ENV_EXPERIMENTAL_PLAN_MODE, "0")],
            &["invalid", "question", "todowrite"],
        ),
        // cases 17 and 18: the client match is case-sensitive and not defaulted.
        (
            &[(ENV_CLIENT, "CLI"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "todowrite"],
        ),
        (
            &[(ENV_CLIENT, ""), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "todowrite"],
        ),
    ];

    // A floor assertion: a table that silently shrank to nothing would otherwise pass.
    assert!(
        cases.len() >= 15,
        "the measured matrix must not have shrunk: {} cases",
        cases.len()
    );

    for (environment, expected) in cases {
        let mut offered = exposed_conditional_tools(&flags(environment));
        offered.sort_unstable();
        let mut want: Vec<&str> = expected.to_vec();
        want.sort_unstable();
        assert_eq!(
            offered, want,
            "measured binary disagrees for environment {environment:?}"
        );
    }
}

#[test]
fn conditional_each_tool_reports_its_own_exposure_through_the_same_predicate() {
    // The tools expose `exposed_under` so a caller need not know which predicate goes
    // with which tool; those must not drift from `exposure`'s answer.
    let plan_mode_cli = flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "true")]);
    let headless = flags(&[(ENV_CLIENT, "tui")]);

    assert!(TodoWriteTool::exposed_under(&plan_mode_cli));
    assert!(TodoWriteTool::exposed_under(&headless));
    assert!(QuestionTool::exposed_under(&plan_mode_cli));
    assert!(!QuestionTool::exposed_under(&headless));
    assert!(PlanExitTool::exposed_under(&plan_mode_cli));
    assert!(!PlanExitTool::exposed_under(&headless));
}

// ---------------------------------------------------------------------------
// the four tools coexist in one registry-shaped list
// ---------------------------------------------------------------------------

#[test]
fn conditional_all_four_tools_erase_into_one_list_with_distinct_wire_ids() {
    let (_directory, pool) = seeded_pool();
    let asker: Arc<dyn QuestionAsker> = Arc::new(ScriptedAnswers::selecting("Yes"));

    let registry: Vec<Arc<dyn Tool>> = vec![
        erase(InvalidTool::new()),
        erase(QuestionTool::new(Arc::clone(&asker))),
        todo_tool(&pool),
        erase(PlanExitTool::new(asker, Arc::new(RecordingHost::default()))),
    ];

    let ids: Vec<&str> = registry.iter().map(|tool| tool.id()).collect();
    assert_eq!(ids, vec!["invalid", "question", "todowrite", "plan_exit"]);

    for tool in &registry {
        let definition = tool.definition();
        assert_eq!(
            definition.parameters["type"], "object",
            "{} must derive an object schema",
            definition.id
        );
        assert!(
            !definition.description.is_empty(),
            "{} must carry a description",
            definition.id
        );
    }
}
