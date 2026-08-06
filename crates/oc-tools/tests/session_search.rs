use oc_db::message::{MessageRecord, MessageStore, PartRecord};
use oc_db::{Connection, migration, open};
use oc_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, erase};
use oc_tools::session_search::SessionSearchTool;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

fn context() -> ToolContext {
    ToolContext::new(
        "ses_current",
        "msg_current",
        "call_search",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn database() -> (tempfile::TempDir, Connection) {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let path = directory.path().join("opencode.db");
    let mut connection = open::open_at(&path).expect("open database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-search', '/workspace', 1, 1, '[]')",
            [],
        )
        .expect("seed project");
    (directory, connection)
}

fn add_session(
    connection: &Connection,
    id: &str,
    title: &str,
    parent_id: Option<&str>,
    updated: i64,
) {
    connection
        .execute(
            "INSERT INTO session \
               (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) \
             VALUES (?1, 'project-search', ?2, ?1, '/workspace', ?3, '1', ?4, ?4)",
            rusqlite::params![id, parent_id, title, updated],
        )
        .expect("seed session");
}

fn add_message(
    connection: &Connection,
    session_id: &str,
    message_id: &str,
    role: &str,
    text: &str,
    created: i64,
) {
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": session_id,
        "role": role,
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "test", "modelID": "test" },
    }))
    .expect("valid message");
    let part = PartRecord::from_json(
        json!({
            "id": format!("prt_{message_id}"),
            "sessionID": session_id,
            "messageID": message_id,
            "type": "text",
            "text": text,
        }),
        created,
    )
    .expect("valid part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("write message");
    store.put_part_at(&part, created).expect("write part");
}

fn tool(path: &Path) -> Arc<dyn Tool> {
    erase(SessionSearchTool::new(path))
}

fn payload(output: &oc_tool::ToolOutput) -> Value {
    serde_json::from_str(&output.output).expect("tool output is JSON")
}

#[tokio::test]
async fn session_search_browse_returns_recent_root_sessions_with_previews() {
    let (directory, connection) = database();
    add_session(&connection, "ses_old", "Old work", None, 10);
    add_session(&connection, "ses_new", "New work", None, 30);
    add_session(
        &connection,
        "ses_child",
        "Delegated work",
        Some("ses_new"),
        40,
    );
    add_message(&connection, "ses_old", "msg_old", "user", "old preview", 11);
    add_message(&connection, "ses_new", "msg_new", "user", "new preview", 31);
    drop(connection);

    let output = tool(&directory.path().join("opencode.db"))
        .execute(json!({}), context())
        .await
        .expect("browse succeeds");
    let body = payload(&output);

    assert_eq!(body["mode"], "browse");
    assert_eq!(body["results"].as_array().expect("results").len(), 2);
    assert_eq!(body["results"][0]["session_id"], "ses_new");
    assert_eq!(body["results"][0]["preview"], "new preview");
    assert_eq!(body["results"][1]["session_id"], "ses_old");
}

#[tokio::test]
async fn session_search_discovery_returns_snippet_window_and_three_message_bookends() {
    let (directory, connection) = database();
    add_session(&connection, "ses_match", "Socket diagnosis", None, 100);
    for index in 0..12 {
        let text = if index == 6 {
            "the orbital socket handshake failed"
        } else {
            "ordinary context"
        };
        add_message(
            &connection,
            "ses_match",
            &format!("msg_{index:02}"),
            if index % 2 == 0 { "user" } else { "assistant" },
            text,
            100 + index,
        );
    }
    drop(connection);

    let output = tool(&directory.path().join("opencode.db"))
        .execute(json!({ "query": "orbital socket", "limit": 3 }), context())
        .await
        .expect("discovery succeeds");
    let body = payload(&output);
    let result = &body["results"][0];

    assert_eq!(body["mode"], "discovery");
    assert_eq!(result["session_id"], "ses_match");
    assert_eq!(result["match_message_id"], "msg_06");
    assert!(
        result["snippet"]
            .as_str()
            .expect("snippet")
            .contains("orbital")
    );
    assert_eq!(result["messages"].as_array().expect("window").len(), 11);
    assert_eq!(result["messages"][5]["anchor"], true);
    assert_eq!(result["bookend_start"].as_array().expect("start").len(), 3);
    assert_eq!(result["bookend_start"][0]["id"], "msg_00");
    assert_eq!(result["bookend_end"].as_array().expect("end").len(), 3);
    assert_eq!(result["bookend_end"][2]["id"], "msg_11");
    assert_eq!(result["messages_before"], 6);
    assert_eq!(result["messages_after"], 5);
}

#[tokio::test]
async fn session_search_discovery_demotes_children_without_excluding_them() {
    let (directory, connection) = database();
    add_session(&connection, "ses_root", "Interactive", None, 10);
    add_message(
        &connection,
        "ses_root",
        "msg_root",
        "user",
        "recallblindness",
        10,
    );
    for index in 0..4 {
        let session_id = format!("ses_child_{index}");
        add_session(
            &connection,
            &session_id,
            "Delegated",
            Some("ses_root"),
            100 + index,
        );
        add_message(
            &connection,
            &session_id,
            &format!("msg_child_{index}"),
            "assistant",
            "recallblindness recallblindness recallblindness",
            100 + index,
        );
    }
    drop(connection);

    let output = tool(&directory.path().join("opencode.db"))
        .execute(json!({ "query": "recallblindness", "limit": 2 }), context())
        .await
        .expect("discovery succeeds");
    let body = payload(&output);

    assert_eq!(body["results"][0]["session_id"], "ses_root");
    assert!(
        body["results"][1]["session_id"]
            .as_str()
            .expect("child id")
            .starts_with("ses_child_")
    );
}

#[tokio::test]
async fn session_search_scroll_centers_on_the_anchor_without_fts_or_bookends() {
    let (directory, connection) = database();
    add_session(&connection, "ses_scroll", "Scrollable", None, 10);
    for index in 0..9 {
        add_message(
            &connection,
            "ses_scroll",
            &format!("msg_scroll_{index:02}"),
            if index % 2 == 0 { "user" } else { "assistant" },
            &format!("message {index}"),
            10 + index,
        );
    }
    drop(connection);

    let output = tool(&directory.path().join("opencode.db"))
        .execute(
            json!({
                "session_id": "ses_scroll",
                "around_message_id": "msg_scroll_04",
                "window": 2
            }),
            context(),
        )
        .await
        .expect("scroll succeeds");
    let body = payload(&output);

    assert_eq!(body["mode"], "scroll");
    assert_eq!(body["messages"].as_array().expect("window").len(), 5);
    assert_eq!(body["messages"][0]["id"], "msg_scroll_02");
    assert_eq!(body["messages"][2]["anchor"], true);
    assert_eq!(body["messages_before"], 4);
    assert_eq!(body["messages_after"], 4);
    assert!(body.get("bookend_start").is_none());
    assert!(body.get("bookend_end").is_none());
}

#[tokio::test]
async fn session_search_invalid_mode_combinations_are_model_correctable() {
    let (directory, connection) = database();
    drop(connection);
    let search = tool(&directory.path().join("opencode.db"));

    for arguments in [
        json!({ "around_message_id": "msg_missing" }),
        json!({ "session_id": "ses_missing" }),
        json!({ "query": "needle", "session_id": "ses_missing", "around_message_id": "msg_missing" }),
        json!({ "window": 3 }),
    ] {
        let error = search
            .execute(arguments, context())
            .await
            .expect_err("invalid mode is rejected");
        assert!(matches!(error, oc_error::ToolError::InvalidArgs { .. }));
        assert!(error.is_model_correctable());
    }
}

#[test]
fn session_search_schema_has_no_provider_or_model_parameters() {
    let definition = tool(Path::new("/tmp/opencode.db")).definition();
    let properties = definition.parameters["properties"]
        .as_object()
        .expect("properties");

    assert!(!properties.contains_key("provider"));
    assert!(!properties.contains_key("model"));
    assert!(!properties.contains_key("summarize"));
}
