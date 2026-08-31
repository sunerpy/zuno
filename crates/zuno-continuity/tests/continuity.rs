use std::sync::{Arc, Barrier};

use serde_json::{Value, json};
use zuno_continuity::{
    ContinuitySettings, HistoryProvider, NoteScope, NotesProvider, SqliteContinuityProvider,
    profile_overlay,
};
use zuno_db::Pool;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_harness::{HostPlanningCapability, ToolContributions, default_profile};
use zuno_runtime::HarnessRuntime;
use zuno_tool::{ToolConcurrencyPolicy, ToolEffect, ToolReplayPolicy};

fn database(session_ids: &[&str]) -> Arc<Pool> {
    database_at(zuno_paths::DbLocation::Memory, session_ids)
}

fn database_at(location: zuno_paths::DbLocation, session_ids: &[&str]) -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&location).expect("pool"));
    let mut connection = pool.open_connection().expect("connection");
    zuno_db::migration::apply(&mut connection).expect("schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('project', '/repo', 1, 1, '[]');",
        )
        .expect("project");
    for (index, session_id) in session_ids.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO session
                 (id, project_id, slug, directory, title, version, time_created, time_updated)
                 VALUES (?1, 'project', ?2, '/repo', ?2, 'test', ?3, ?3)",
                (
                    session_id,
                    format!("session-{index}"),
                    i64::try_from(index).unwrap_or(i64::MAX) + 1,
                ),
            )
            .expect("session");
    }
    pool
}

fn note_scope<'a>(session_id: &'a str, agent: &'a str, call_id: &'a str) -> NoteScope<'a> {
    NoteScope {
        session_id,
        agent,
        call_id,
    }
}

fn put_message(
    connection: &rusqlite::Connection,
    session_id: &str,
    id: &str,
    role: &str,
    time: i64,
    extra: Value,
) {
    let mut value = json!({
        "id": id,
        "sessionID": session_id,
        "role": role,
        "time": {"created": time},
        "agent": "build",
    });
    value
        .as_object_mut()
        .expect("message object")
        .extend(extra.as_object().expect("extra object").clone());
    let message = MessageRecord::from_json(value).expect("message");
    MessageStore::new(connection)
        .put_message(&message)
        .expect("persist message");
}

fn put_part(
    connection: &rusqlite::Connection,
    session_id: &str,
    message_id: &str,
    id: &str,
    time: i64,
    data: Value,
) {
    let mut value = json!({
        "id": id,
        "sessionID": session_id,
        "messageID": message_id,
    });
    value
        .as_object_mut()
        .expect("part object")
        .extend(data.as_object().expect("part data object").clone());
    let part = PartRecord::from_json(value, time).expect("part");
    MessageStore::new(connection)
        .put_part(&part)
        .expect("persist part");
}

#[test]
fn notes_are_revisioned_idempotent_and_agent_isolated() {
    let pool = database(&["ses_a", "ses_child"]);
    let provider = SqliteContinuityProvider::open(pool, true).expect("provider");
    let created = provider
        .write_file(
            note_scope("ses_a", "build", "call_create"),
            "work/evidence.md",
            "one",
            0,
        )
        .expect("create");
    assert_eq!(created["revision"], 1);

    let replayed = provider
        .write_file(
            note_scope("ses_a", "build", "call_create"),
            "work/evidence.md",
            "one",
            0,
        )
        .expect("idempotent replay");
    assert_eq!(replayed["revision"], 1);
    assert_eq!(replayed["replayed"], true);

    let conflict = provider.write_file(
        note_scope("ses_a", "build", "call_conflict"),
        "work/evidence.md",
        "two",
        0,
    );
    assert!(conflict.is_err());

    let appended = provider
        .append_to_file(
            note_scope("ses_a", "build", "call_append"),
            "work/evidence.md",
            "\ntwo",
            1,
        )
        .expect("append");
    assert_eq!(appended["revision"], 2);
    let read = provider
        .read_file(note_scope("ses_a", "build", "read"), "work/evidence.md")
        .expect("read");
    assert_eq!(read["content"], "one\ntwo");
    assert!(
        provider
            .read_file(note_scope("ses_a", "plan", "read"), "work/evidence.md")
            .is_err(),
        "another Agent cannot read the document"
    );
    assert!(
        provider
            .read_file(note_scope("ses_child", "build", "read"), "work/evidence.md")
            .is_err(),
        "a child session cannot read its parent session's note"
    );
}

#[test]
fn notes_reject_invalid_names_quotas_and_stale_concurrent_writes() {
    let pool = database(&["ses_a"]);
    let provider = Arc::new(SqliteContinuityProvider::open(pool, true).expect("provider"));
    assert!(
        provider
            .write_file(note_scope("ses_a", "build", "invalid"), "../host", "x", 0,)
            .is_err()
    );
    assert!(
        provider
            .write_file(
                note_scope("ses_a", "build", "large"),
                "large.txt",
                &"x".repeat(256 * 1024 + 1),
                0,
            )
            .is_err()
    );
    provider
        .write_file(
            note_scope("ses_a", "build", "initial"),
            "race.txt",
            "base",
            0,
        )
        .expect("initial");

    let barrier = Arc::new(Barrier::new(3));
    let workers = ["left", "right"].map(|name| {
        let provider = Arc::clone(&provider);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            provider.write_file(note_scope("ses_a", "build", name), "race.txt", name, 1)
        })
    });
    barrier.wait();
    let results = workers.map(|worker| worker.join().expect("worker"));
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
}

#[test]
fn notes_enforce_document_count_aggregate_bytes_and_cursor_scope() {
    let pool = database(&["ses_a"]);
    let provider = SqliteContinuityProvider::open(pool, true).expect("provider");
    for index in 0..zuno_continuity::MAX_NOTE_DOCUMENTS {
        provider
            .write_file(
                note_scope("ses_a", "build", &format!("count-{index}")),
                &format!("count/{index:03}.md"),
                "",
                0,
            )
            .expect("fill document quota");
    }
    assert!(
        provider
            .write_file(
                note_scope("ses_a", "build", "count-overflow"),
                "count/overflow.md",
                "",
                0,
            )
            .is_err()
    );

    let full_document = "x".repeat(zuno_continuity::MAX_NOTE_DOCUMENT_BYTES as usize);
    for index in 0..4 {
        provider
            .write_file(
                note_scope("ses_a", "deep", &format!("bytes-{index}")),
                &format!("bytes/{index}.txt"),
                &full_document,
                0,
            )
            .expect("fill aggregate quota");
    }
    assert!(
        provider
            .write_file(
                note_scope("ses_a", "deep", "bytes-overflow"),
                "bytes/overflow.txt",
                "x",
                0,
            )
            .is_err()
    );

    let first = provider
        .list_files_by_prefix(
            note_scope("ses_a", "build", "list"),
            Some("count/"),
            None,
            Some(1),
        )
        .expect("first page");
    let cursor = first["next_cursor"].as_str().expect("next cursor");
    let second = provider
        .list_files_by_prefix(
            note_scope("ses_a", "build", "list"),
            Some("count/"),
            Some(cursor),
            Some(1),
        )
        .expect("second page");
    assert_ne!(first["files"][0]["name"], second["files"][0]["name"]);
    assert!(
        provider
            .list_files_by_prefix(
                note_scope("ses_a", "plan", "list"),
                Some("count/"),
                Some(cursor),
                Some(1),
            )
            .is_err(),
        "a page cursor cannot cross Agent scopes"
    );
}

#[test]
fn history_paginates_repeated_compaction_windows_and_survives_reopen() {
    let directory = tempfile::tempdir().expect("database directory");
    let location = zuno_paths::DbLocation::File(directory.path().join("continuity.db"));
    let pool = database_at(location.clone(), &["ses_a", "ses_b"]);
    {
        let connection = pool.get().expect("connection");
        for (index, id) in ["msg_a", "msg_b", "msg_c", "msg_d"].into_iter().enumerate() {
            let time = i64::try_from(index).expect("small index") + 1;
            put_message(&connection, "ses_a", id, "user", time, json!({}));
            put_part(
                &connection,
                "ses_a",
                id,
                &format!("part_{id}"),
                time,
                json!({"type": "text", "text": format!("evidence {id}")}),
            );
        }
        for (ordinal, tail) in [(1, "msg_b"), (2, "msg_c")] {
            let marker = format!("marker_{ordinal}");
            put_message(
                &connection,
                "ses_a",
                &marker,
                "assistant",
                10 + ordinal,
                json!({}),
            );
            put_part(
                &connection,
                "ses_a",
                &marker,
                &format!("part_marker_{ordinal}"),
                10 + ordinal,
                json!({"type": "compaction", "tail_start_id": tail}),
            );
            put_message(
                &connection,
                "ses_a",
                &format!("summary_{ordinal}"),
                "assistant",
                20 + ordinal,
                json!({
                    "parentID": marker,
                    "mode": "compaction",
                    "summary": true,
                    "finish": "stop",
                }),
            );
            put_part(
                &connection,
                "ses_a",
                &format!("summary_{ordinal}"),
                &format!("part_summary_{ordinal}"),
                20 + ordinal,
                json!({"type": "text", "text": "summary"}),
            );
        }
    }
    let provider = SqliteContinuityProvider::open(Arc::clone(&pool), false).expect("provider");
    let windows = provider
        .list_windows("ses_a", None, Some(1))
        .expect("first page");
    assert_eq!(windows["windows"].as_array().expect("windows").len(), 1);
    let current_window = windows["windows"][0]["window_id"]
        .as_str()
        .expect("current window")
        .to_owned();
    let cursor = windows["next_cursor"].as_str().expect("window cursor");
    let second = provider
        .list_windows("ses_a", Some(cursor), Some(1))
        .expect("second page");
    assert_eq!(second["windows"].as_array().expect("windows").len(), 1);
    let oldest = provider
        .list_windows("ses_a", second["next_cursor"].as_str(), Some(1))
        .expect("third page");
    assert_eq!(oldest["windows"][0]["boundary"], "session_start");

    let first_item = provider
        .list_items("ses_a", &current_window, None, Some(1))
        .expect("first item page");
    let item_cursor = first_item["next_cursor"].as_str().expect("item cursor");
    let second_item = provider
        .list_items("ses_a", &current_window, Some(item_cursor), Some(1))
        .expect("second item page");
    assert_ne!(
        first_item["items"][0]["message_id"],
        second_item["items"][0]["message_id"]
    );
    assert!(
        provider
            .list_items("ses_b", &current_window, None, Some(1))
            .is_err()
    );
    drop(provider);
    drop(pool);

    let reopened_pool = Arc::new(Pool::open(&location).expect("reopen pool"));
    let reopened = SqliteContinuityProvider::open(reopened_pool, false).expect("reopened provider");
    let matches =
        HistoryProvider::search_contents(&reopened, "ses_a", "evidence msg_a", None, Some(1))
            .expect("search after reopen");
    assert_eq!(matches["matches"].as_array().expect("matches").len(), 1);
    let item_id = matches["matches"][0]["item_id"].as_str().expect("item id");
    assert_eq!(
        reopened
            .read_item("ses_a", item_id)
            .expect("read after reopen")["message_id"],
        "msg_a"
    );
}

#[test]
fn history_uses_only_successful_compactions_and_scrubs_sensitive_parts() {
    let pool = database(&["ses_a", "ses_b"]);
    let connection = pool.get().expect("connection");
    put_message(&connection, "ses_a", "msg_old", "user", 1, json!({}));
    put_part(
        &connection,
        "ses_a",
        "msg_old",
        "prt_old",
        1,
        json!({"type": "text", "text": "old release evidence"}),
    );
    put_message(&connection, "ses_a", "msg_tail", "user", 2, json!({}));
    put_part(
        &connection,
        "ses_a",
        "msg_tail",
        "prt_tail",
        2,
        json!({"type": "text", "text": "current work"}),
    );
    put_message(
        &connection,
        "ses_a",
        "msg_marker",
        "assistant",
        3,
        json!({}),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_marker",
        "prt_marker",
        3,
        json!({"type": "compaction", "tail_start_id": "msg_tail"}),
    );
    put_message(
        &connection,
        "ses_a",
        "msg_summary",
        "assistant",
        4,
        json!({
            "parentID": "msg_marker",
            "mode": "compaction",
            "summary": true,
            "finish": "stop",
        }),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_summary",
        "prt_summary",
        4,
        json!({"type": "text", "text": "summary"}),
    );
    put_message(
        &connection,
        "ses_a",
        "msg_after",
        "assistant",
        5,
        json!({"mode": "build"}),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_after",
        "prt_after_text",
        5,
        json!({"type": "text", "text": "done"}),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_after",
        "prt_after_synthetic",
        6,
        json!({
            "type": "text",
            "text": "internal prompt body",
            "synthetic": true,
        }),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_after",
        "prt_after_reasoning",
        7,
        json!({
            "type": "reasoning",
            "text": "private chain",
            "metadata": {"encryptedContent": "cipher"},
        }),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_after",
        "prt_after_file",
        8,
        json!({
            "type": "file",
            "mime": "image/png",
            "url": "data:image/png;base64,AAAA",
        }),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_after",
        "prt_after_tool",
        9,
        json!({
            "type": "tool",
            "tool": "probe",
            "state": {
                "status": "completed",
                "input": {"secret": "request"},
                "output": {
                    "answer": "visible",
                    "encryptedContent": "cipher",
                    "reasoningContent": "hidden derivative",
                    "image": "data:image/png;base64,AAAA",
                },
            },
        }),
    );
    // A failed compaction marker must not create another window.
    put_message(
        &connection,
        "ses_a",
        "msg_failed_marker",
        "assistant",
        10,
        json!({}),
    );
    put_part(
        &connection,
        "ses_a",
        "msg_failed_marker",
        "prt_failed_marker",
        10,
        json!({"type": "compaction", "tail_start_id": "msg_after"}),
    );
    put_message(
        &connection,
        "ses_a",
        "msg_failed_summary",
        "assistant",
        11,
        json!({
            "parentID": "msg_failed_marker",
            "mode": "compaction",
            "summary": true,
            "finish": "error",
            "error": {"message": "failed"},
        }),
    );
    drop(connection);

    let provider = SqliteContinuityProvider::open(pool, false).expect("provider");
    let windows = provider.list_windows("ses_a", None, None).expect("windows");
    assert_eq!(windows["windows"].as_array().expect("array").len(), 2);

    let old =
        HistoryProvider::search_contents(&provider, "ses_a", "old release evidence", None, None)
            .expect("search");
    assert_eq!(old["matches"].as_array().expect("matches").len(), 1);
    let item_id = old["matches"][0]["item_id"].as_str().expect("item id");
    assert!(
        provider.read_item("ses_b", item_id).is_err(),
        "an opaque item id cannot cross sessions"
    );

    let current_window = windows["windows"][0]["window_id"]
        .as_str()
        .expect("current window");
    let items = provider
        .list_items("ses_a", current_window, None, Some(50))
        .expect("items");
    let rendered = items.to_string();
    assert!(rendered.contains("done"));
    assert!(rendered.contains("visible"));
    assert!(!rendered.contains("private chain"));
    assert!(!rendered.contains("internal prompt body"));
    assert!(!rendered.contains("cipher"));
    assert!(!rendered.contains("hidden derivative"));
    assert!(!rendered.contains("AAAA"));
    assert!(!rendered.contains("\"input\""));
    assert!(!rendered.contains("msg_marker"));
    assert!(!rendered.contains("msg_failed_marker"));
    assert!(rendered.contains("binary_attachment"));
    assert!(rendered.contains("reasoning"));
}

#[test]
fn notes_compaction_history_and_plan_survive_one_restart() {
    let directory = tempfile::tempdir().expect("database directory");
    let location = zuno_paths::DbLocation::File(directory.path().join("continuity-restart.db"));
    let pool = database_at(location.clone(), &["ses_a"]);
    let provider = SqliteContinuityProvider::open(Arc::clone(&pool), true).expect("provider");
    provider
        .write_file(
            note_scope("ses_a", "build", "call-note"),
            "recovery/evidence.md",
            "durable note evidence",
            0,
        )
        .expect("write note");
    zuno_tools::WorkStateStore::new(Arc::clone(&pool))
        .update_plan(
            "ses_a",
            zuno_tools::PlanUpdateParams {
                expected_revision: None,
                goal_id: None,
                title: "Recover context".to_owned(),
                steps: vec![zuno_tools::PlanStep {
                    id: "recover".to_owned(),
                    title: "Recover durable context".to_owned(),
                    status: zuno_tools::PlanStepStatus::InProgress,
                }],
            },
        )
        .expect("persist plan");
    {
        let connection = pool.get().expect("connection");
        put_message(&connection, "ses_a", "msg_old", "user", 1, json!({}));
        put_part(
            &connection,
            "ses_a",
            "msg_old",
            "prt_old",
            1,
            json!({"type": "text", "text": "pre-compaction release evidence"}),
        );
        put_message(&connection, "ses_a", "msg_tail", "user", 2, json!({}));
        put_part(
            &connection,
            "ses_a",
            "msg_tail",
            "prt_tail",
            2,
            json!({"type": "text", "text": "retained tail"}),
        );
        put_message(&connection, "ses_a", "msg_marker", "user", 3, json!({}));
        put_part(
            &connection,
            "ses_a",
            "msg_marker",
            "prt_marker",
            3,
            json!({"type": "compaction", "tail_start_id": "msg_tail"}),
        );
        put_message(
            &connection,
            "ses_a",
            "msg_summary",
            "assistant",
            4,
            json!({
                "parentID": "msg_marker",
                "mode": "compaction",
                "summary": true,
                "finish": "stop",
            }),
        );
        put_part(
            &connection,
            "ses_a",
            "msg_summary",
            "prt_summary",
            4,
            json!({"type": "text", "text": "summary"}),
        );
    }
    drop(provider);
    drop(pool);

    let reopened_pool = Arc::new(Pool::open(&location).expect("reopen pool"));
    let reopened =
        SqliteContinuityProvider::open(Arc::clone(&reopened_pool), true).expect("reopen provider");
    let replayed = reopened
        .write_file(
            note_scope("ses_a", "build", "call-note"),
            "recovery/evidence.md",
            "durable note evidence",
            0,
        )
        .expect("replay persisted mutation after restart");
    assert_eq!(replayed["revision"], 1);
    assert_eq!(replayed["replayed"], true);
    assert_eq!(
        reopened
            .read_file(
                note_scope("ses_a", "build", "call-read"),
                "recovery/evidence.md",
            )
            .expect("read note after restart")["content"],
        "durable note evidence"
    );
    let history = HistoryProvider::search_contents(
        &reopened,
        "ses_a",
        "pre-compaction release evidence",
        None,
        None,
    )
    .expect("recover old history after restart");
    assert_eq!(history["matches"].as_array().expect("matches").len(), 1);
    let plan = zuno_tools::WorkStateStore::new(reopened_pool)
        .plan("ses_a")
        .expect("read plan after restart")
        .expect("durable plan");
    assert_eq!(plan.steps[0].id, "recover");
    assert_eq!(plan.steps[0].status, zuno_tools::PlanStepStatus::InProgress);
}

#[tokio::test]
async fn profile_overlay_publishes_tools_service_and_inherited_host_planning() {
    let pool = database(&["ses_a"]);
    let parent = HarnessRuntime::new("parent");
    parent
        .activate_profile(default_profile())
        .await
        .expect("default profile");
    let child = parent.child("session");
    let base = child
        .service::<ToolContributions>()
        .expect("base contributions");
    let overlay = profile_overlay(
        &base,
        pool,
        ContinuitySettings {
            history: true,
            notes: true,
        },
    )
    .expect("overlay");
    child.activate_profile(overlay).await.expect("activate");
    let contributions = child
        .service::<ToolContributions>()
        .expect("overlay contributions");
    let ids = contributions
        .tools()
        .iter()
        .map(|tool| tool.id())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["history", "notes"]);
    assert!(child.service::<HostPlanningCapability>().is_some());
    parent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn notes_policy_is_resolved_from_each_action_and_invalid_calls_fail_closed() {
    let pool = database(&["ses_a"]);
    let parent = HarnessRuntime::new("parent");
    parent
        .activate_profile(default_profile())
        .await
        .expect("default profile");
    let child = parent.child("session");
    let base = child
        .service::<ToolContributions>()
        .expect("base contributions");
    child
        .activate_profile(
            profile_overlay(
                &base,
                pool,
                ContinuitySettings {
                    history: false,
                    notes: true,
                },
            )
            .expect("overlay"),
        )
        .await
        .expect("activate");
    let contributions = child.service::<ToolContributions>().expect("contributions");
    let notes = contributions
        .tools()
        .iter()
        .find(|tool| tool.id() == "notes")
        .expect("notes");

    let read = json!({"action": "read_file", "name": "a.md"});
    assert_eq!(notes.replay_policy_for(&read), ToolReplayPolicy::Safe);
    assert_eq!(
        notes.concurrency_policy_for(&read),
        ToolConcurrencyPolicy::ParallelSafe
    );
    assert_eq!(notes.effect(&read), ToolEffect::ReadOnly);

    for arguments in [
        json!({"action": "write_file", "name": "a.md", "content": "x", "expected_revision": 0}),
        json!({"action": "unknown"}),
        json!({}),
    ] {
        assert_eq!(notes.replay_policy_for(&arguments), ToolReplayPolicy::Never);
        assert_eq!(
            notes.concurrency_policy_for(&arguments),
            ToolConcurrencyPolicy::Exclusive
        );
        assert_eq!(notes.effect(&arguments), ToolEffect::SideEffecting);
    }
    parent.shutdown().await.expect("shutdown");
}
