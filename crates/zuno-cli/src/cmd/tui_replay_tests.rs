use super::*;
use serde_json::json;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Connection, migration, open};

const SESSION_ID: &str = "ses_replay";

/// A database holding one project and one session, with the schema applied.
fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-replay', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-replay', 'replay', '/workspace', 'replay', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn put_message(connection: &Connection, id: &str, role: &str, created: i64, extra: Value) {
    let mut payload = json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": role,
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    });
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            payload[key] = value;
        }
    }
    let record = MessageRecord::from_json(payload).expect("valid message");
    MessageStore::new(connection)
        .put_message_at(&record, created)
        .expect("persist message");
}

fn put_part(connection: &Connection, message_id: &str, part_id: &str, created: i64, body: Value) {
    let mut payload = json!({
        "id": part_id,
        "sessionID": SESSION_ID,
        "messageID": message_id,
    });
    if let Value::Object(body) = body {
        for (key, value) in body {
            payload[key] = value;
        }
    }
    let record = PartRecord::from_json(payload, created).expect("valid part");
    MessageStore::new(connection)
        .put_part_at(&record, created)
        .expect("persist part");
}

fn put_text_turn(connection: &Connection, index: i64, prompt: &str, reply: &str) {
    let user = format!("msg_user_{index}");
    let assistant = format!("msg_assistant_{index}");
    let created = 100 + index * 10;
    put_message(connection, &user, "user", created, Value::Null);
    put_part(
        connection,
        &user,
        &format!("prt_{user}"),
        created,
        json!({ "type": "text", "text": prompt }),
    );
    put_message(
        connection,
        &assistant,
        "assistant",
        created + 1,
        Value::Null,
    );
    put_part(
        connection,
        &assistant,
        &format!("prt_{assistant}"),
        created + 1,
        json!({ "type": "text", "text": reply }),
    );
}

/// The history the resume path reads, through the same function the next request calls.
fn history(connection: &Connection) -> Vec<MessageWithParts> {
    zuno_engine::r#loop::hydrate_retained_history(connection, SESSION_ID)
        .expect("hydrate retained history")
}

fn texts(message: &Message) -> Vec<&str> {
    message.parts.iter().filter_map(MessagePart::text).collect()
}

fn notice_text(message: &Message) -> &str {
    match &message.parts[0] {
        MessagePart::Notice { text, .. } => text,
        other => panic!("expected a notice: {other:?}"),
    }
}

#[test]
fn a_resumed_session_replays_its_user_and_assistant_turns() {
    let connection = seeded();
    put_text_turn(
        &connection,
        0,
        "what does the guard do",
        "it clamps the width",
    );

    let replay = project(history(&connection));

    assert_eq!(replay.messages.len(), 2, "{:?}", replay.messages);
    assert_eq!(replay.messages[0].role, Role::User);
    assert_eq!(texts(&replay.messages[0]), vec!["what does the guard do"]);
    assert_eq!(replay.messages[1].role, Role::Assistant);
    assert_eq!(texts(&replay.messages[1]), vec!["it clamps the width"]);
}

#[test]
fn a_replayed_message_keeps_its_stored_id() {
    let connection = seeded();
    put_text_turn(&connection, 0, "prompt", "reply");

    let replay = project(history(&connection));

    assert_eq!(
        replay
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>(),
        vec![
            Some(String::from("msg_user_0")),
            Some(String::from("msg_assistant_0")),
        ],
    );
}

#[test]
fn a_completed_tool_call_replays_with_its_arguments_output_title_and_diff() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_tool",
        100,
        json!({
            "type": "tool",
            "callID": "call_1",
            "tool": "edit",
            "state": {
                "status": "completed",
                "input": { "filePath": "src/lib.rs" },
                "title": "Edit lib.rs",
                "output": "the file was rewritten",
                "metadata": { "diff": "--- a\n+++ b\n" }
            }
        }),
    );

    let replay = project(history(&connection));

    let [
        MessagePart::Tool {
            call_id,
            name,
            arguments,
            title,
            status,
            output,
            diff,
            ..
        },
    ] = replay.messages[0].parts.as_slice()
    else {
        panic!("expected one tool part: {:?}", replay.messages[0].parts);
    };
    assert_eq!(call_id, "call_1");
    assert_eq!(name, "edit");
    assert_eq!(arguments, r#"{"filePath":"src/lib.rs"}"#);
    assert_eq!(title.as_deref(), Some("Edit lib.rs"));
    assert_eq!(*status, ToolStatus::Completed);
    assert_eq!(output.as_deref(), Some("the file was rewritten"));
    assert_eq!(diff.as_deref(), Some("--- a\n+++ b\n"));
}

#[test]
fn a_failed_tool_call_replays_its_error_as_the_output_it_showed_live() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_tool",
        100,
        json!({
            "type": "tool",
            "callID": "call_1",
            "tool": "bash",
            "state": {
                "status": "error",
                "input": {},
                "error": "permission denied by the user"
            }
        }),
    );

    let replay = project(history(&connection));

    let [MessagePart::Tool { status, output, .. }] = replay.messages[0].parts.as_slice() else {
        panic!("expected one tool part: {:?}", replay.messages[0].parts);
    };
    assert_eq!(*status, ToolStatus::Error);
    assert_eq!(output.as_deref(), Some("permission denied by the user"));
}

#[test]
fn a_tool_call_that_never_resolved_replays_as_pending_rather_than_vanishing() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_tool",
        100,
        json!({
            "type": "tool",
            "callID": "call_1",
            "tool": "bash",
            "state": { "status": "pending", "input": { "command": "sleep 600" } }
        }),
    );

    let replay = project(history(&connection));

    let [MessagePart::Tool { status, name, .. }] = replay.messages[0].parts.as_slice() else {
        panic!("expected one tool part: {:?}", replay.messages[0].parts);
    };
    assert_eq!(
        *status,
        ToolStatus::Pending,
        "an unresolved call is what a user resuming after an interruption is looking for",
    );
    assert_eq!(name, "bash");
}

#[test]
fn a_tool_part_with_no_call_id_is_dropped_exactly_as_the_provider_projection_drops_it() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_text",
        100,
        json!({ "type": "text", "text": "before the broken call" }),
    );
    put_part(
        &connection,
        "msg_a",
        "prt_tool",
        101,
        json!({ "type": "tool", "tool": "bash", "state": { "status": "completed" } }),
    );

    let replay = project(history(&connection));

    assert_eq!(texts(&replay.messages[0]), vec!["before the broken call"]);
    assert!(
        !replay.messages[0]
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Tool { .. })),
        "a call with no id cannot be matched to a result: {:?}",
        replay.messages[0].parts,
    );
}

#[test]
fn a_replayed_reasoning_block_carries_its_duration_and_is_never_still_streaming() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_reasoning",
        100,
        json!({
            "type": "reasoning",
            "text": "checking the guard",
            "time": { "start": 1_000, "end": 3_500 }
        }),
    );

    let replay = project(history(&connection));

    let [
        MessagePart::Reasoning {
            text,
            duration_secs,
            streaming,
        },
    ] = replay.messages[0].parts.as_slice()
    else {
        panic!(
            "expected one reasoning part: {:?}",
            replay.messages[0].parts
        );
    };
    assert_eq!(text, "checking the guard");
    assert_eq!(*duration_secs, Some(2.5));
    assert!(
        !*streaming,
        "a spinner that resumes and never stops is worse than no spinner",
    );
}

#[test]
fn an_empty_text_part_is_not_replayed_as_a_blank_row() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_text",
        100,
        json!({ "type": "text", "text": "" }),
    );

    let replay = project(history(&connection));

    assert!(
        replay.messages.is_empty(),
        "a turn abandoned before its first delta has nothing to show: {:?}",
        replay.messages,
    );
}

#[test]
fn bookkeeping_parts_have_no_on_screen_form_and_are_dropped_whole() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    for (index, body) in [
        json!({ "type": "step-start" }),
        json!({ "type": "step-finish" }),
        json!({ "type": "snapshot", "snapshot": "abc123" }),
        json!({ "type": "agent", "name": "build" }),
        json!({ "type": "patch", "hash": "abc", "files": [] }),
        json!({ "type": "retry", "attempt": 2, "max": 3 }),
    ]
    .into_iter()
    .enumerate()
    {
        put_part(
            &connection,
            "msg_a",
            &format!("prt_{index}"),
            100 + i64::try_from(index).expect("small index"),
            body,
        );
    }

    let replay = project(history(&connection));

    assert!(
        replay.messages.is_empty(),
        "a message with only bookkeeping parts must be skipped, not shown as an empty \
         frame: {:?}",
        replay.messages,
    );
}

#[test]
fn an_interrupted_turn_reports_its_failure_beside_the_partial_reply() {
    let connection = seeded();
    put_message(
        &connection,
        "msg_a",
        "assistant",
        100,
        json!({
            "error": {
                "name": "MessageAbortedError",
                "data": { "message": "aborted by the user" }
            }
        }),
    );
    put_part(
        &connection,
        "msg_a",
        "prt_text",
        100,
        json!({ "type": "text", "text": "I was part way through" }),
    );

    let replay = project(history(&connection));

    let parts = &replay.messages[0].parts;
    assert_eq!(texts(&replay.messages[0]), vec!["I was part way through"]);
    let Some(MessagePart::Notice { text, level }) = parts.last() else {
        panic!("expected a trailing failure notice: {parts:?}");
    };
    assert!(
        text.contains("aborted by the user"),
        "the notice must name why the reply stops: {text}",
    );
    assert_eq!(*level, ToastLevel::Error);
}

#[test]
fn a_shapeless_stored_error_produces_no_empty_notice_row() {
    let connection = seeded();
    put_message(
        &connection,
        "msg_a",
        "assistant",
        100,
        json!({ "error": { "unexpected": true } }),
    );
    put_part(
        &connection,
        "msg_a",
        "prt_text",
        100,
        json!({ "type": "text", "text": "a complete reply" }),
    );

    let replay = project(history(&connection));

    assert_eq!(
        replay.messages[0].parts.len(),
        1,
        "{:?}",
        replay.messages[0].parts,
    );
}

#[test]
fn a_data_url_attachment_is_labelled_by_its_mime_rather_than_its_bytes() {
    let connection = seeded();
    put_message(&connection, "msg_a", "user", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_file",
        100,
        json!({
            "type": "file",
            "mime": "image/png",
            "data": "AAAA",
            "url": "data:image/png;base64,AAAA"
        }),
    );

    let replay = project(history(&connection));

    let [MessagePart::Attachment { filename, mime }] = replay.messages[0].parts.as_slice() else {
        panic!("expected one attachment: {:?}", replay.messages[0].parts);
    };
    assert_eq!(filename, "image/png");
    assert_eq!(mime.as_deref(), Some("image/png"));
    assert!(
        !filename.contains("base64"),
        "a base64 blob is not a filename: {filename}",
    );
}

#[test]
fn a_named_attachment_keeps_its_filename() {
    let connection = seeded();
    put_message(&connection, "msg_a", "assistant", 100, Value::Null);
    put_part(
        &connection,
        "msg_a",
        "prt_file",
        100,
        json!({
            "type": "file",
            "mime": "image/webp",
            "filename": "diagram.webp",
            "url": "/tmp/generated/diagram.webp"
        }),
    );

    let replay = project(history(&connection));

    let [MessagePart::Attachment { filename, .. }] = replay.messages[0].parts.as_slice() else {
        panic!("expected one attachment: {:?}", replay.messages[0].parts);
    };
    assert_eq!(filename, "diagram.webp");
}

#[test]
fn a_session_inside_the_cap_reports_no_omission() {
    let connection = seeded();
    for index in 0..3 {
        put_text_turn(&connection, index, "prompt", "reply");
    }

    let replay = project(history(&connection));

    assert_eq!(replay.messages.len(), 6);
    assert_eq!(replay.omitted, 0);
    assert!(replay.omission_notice().is_none());
}

#[test]
fn the_cap_keeps_the_newest_turns_and_reports_how_many_it_omitted() {
    let connection = seeded();
    let turns = RESUME_MESSAGE_CAP / 2 + 4;
    for index in 0..turns {
        put_text_turn(
            &connection,
            i64::try_from(index).expect("small index"),
            &format!("prompt {index}"),
            &format!("reply {index}"),
        );
    }

    let replay = project(history(&connection));

    assert_eq!(replay.messages.len(), RESUME_MESSAGE_CAP);
    assert_eq!(replay.omitted, turns * 2 - RESUME_MESSAGE_CAP);
    assert_eq!(
        replay
            .messages
            .last()
            .and_then(|message| message.id.clone()),
        Some(format!("msg_assistant_{}", turns - 1)),
        "the cap must drop the oldest turns, not the newest",
    );
    let notice = replay
        .omission_notice()
        .expect("an omission must be visible");
    let text = notice_text(&notice);
    assert!(
        text.contains("earlier turns not shown") && text.contains(&replay.omitted.to_string()),
        "the notice must say how many turns are missing: {text}",
    );
}

#[test]
fn the_screen_and_the_model_are_replayed_from_the_same_stored_messages_in_the_same_order() {
    let connection = seeded();
    for index in 0..3 {
        put_text_turn(
            &connection,
            index,
            &format!("prompt {index}"),
            &format!("reply {index}"),
        );
    }

    let stored = history(&connection);
    let screen = project(stored.clone())
        .messages
        .into_iter()
        .filter_map(|message| message.id)
        .collect::<Vec<_>>();
    let mut model = zuno_engine::r#loop::project_history_owned_with_ids("system", stored)
        .into_iter()
        .filter_map(|projected| projected.message_id)
        .collect::<Vec<_>>();
    model.dedup();

    assert_eq!(
        screen, model,
        "the transcript and the request must be built from the same stored messages, or the \
         user is reading a different conversation from the one the model was given",
    );
}

#[test]
fn a_compacted_session_replays_only_the_tail_the_model_will_receive() {
    let connection = seeded();
    put_text_turn(
        &connection,
        0,
        "the forgotten prompt",
        "the forgotten reply",
    );
    put_message(&connection, "msg_marker", "assistant", 200, Value::Null);
    put_part(
        &connection,
        "msg_marker",
        "prt_marker",
        200,
        json!({ "type": "compaction", "tail_start_id": "msg_user_9" }),
    );
    put_message(
        &connection,
        "msg_summary",
        "assistant",
        201,
        json!({ "parentID": "msg_marker" }),
    );
    put_part(
        &connection,
        "msg_summary",
        "prt_summary",
        201,
        json!({ "type": "text", "text": "a summary of what came before" }),
    );
    put_text_turn(&connection, 9, "the retained prompt", "the retained reply");

    let replay = project(history(&connection));

    let rendered = replay
        .messages
        .iter()
        .flat_map(texts)
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        rendered.contains("the retained prompt"),
        "the retained tail must be on screen: {rendered}",
    );
    assert!(
        !rendered.contains("the forgotten prompt"),
        "showing a compacted head the model has already forgotten is the same mismatch \
         pointed the other way: {rendered}",
    );
}

/// Corrupt one stored part and return the notice a resume would show.
///
/// `data` is not valid JSON here, so SQLite refuses the read itself rather than handing
/// a bad blob up to be decoded — the failure arrives as [`zuno_error::DbError::Query`]
/// with `malformed JSON` in its cause chain. Both corruption modes are exercised,
/// because they surface as different variants and only one of them was obvious.
fn notice_for_corrupt_part(data: &str) -> Message {
    let connection = seeded();
    put_text_turn(&connection, 0, "prompt", "reply");
    connection
        .execute(
            "UPDATE part SET data = ?1 WHERE id = 'prt_msg_user_0'",
            [data],
        )
        .expect("corrupt one stored part");

    let error = zuno_engine::r#loop::hydrate_retained_history(&connection, SESSION_ID)
        .expect_err("a corrupt part must surface as an error rather than as silence");
    failure_notice(SESSION_ID, &error)
}

#[test]
fn a_part_holding_malformed_json_yields_a_notice_naming_the_session_and_the_reason() {
    let notice = notice_for_corrupt_part("{not json");

    let MessagePart::Notice { text, level } = &notice.parts[0] else {
        panic!("expected a notice: {:?}", notice.parts);
    };
    assert_eq!(*level, ToastLevel::Error);
    assert_eq!(notice.role, Role::System);
    assert!(text.contains(SESSION_ID), "{text}");
    assert!(
        text.contains("malformed JSON"),
        "the notice must carry the real reason from the cause chain, not just that \
         something failed: {text}",
    );
    assert!(
        text.contains("the model still has it"),
        "the user has to know the model is not equally blank: {text}",
    );
}

#[test]
fn a_part_holding_an_unknown_kind_yields_a_notice_rather_than_an_unexplained_blank_screen() {
    let notice = notice_for_corrupt_part(r#"{"type":"nonsense"}"#);

    let MessagePart::Notice { text, level } = &notice.parts[0] else {
        panic!("expected a notice: {:?}", notice.parts);
    };
    assert_eq!(*level, ToastLevel::Error);
    assert!(text.contains(SESSION_ID), "{text}");
    assert!(
        text.contains("could not be decoded"),
        "an unknown part kind decodes to nothing and must say so: {text}",
    );
}

/// The resume is wired into the TUI's composition root, before the startup notices.
///
/// # Why a source scan and not a behavioural assertion
///
/// Every test above calls [`project`] directly, so all of them stay green on a build in
/// which `tui.rs` never calls it — which is exactly how the original defect survived. The
/// two are strictly complementary: delete the call site and only this test fails; make
/// `project` return nothing and only the others do. This project's own record shows a
/// four-subsystem feature shipped with zero production callers, so the call site is
/// asserted rather than assumed.
///
/// The *order* is asserted because it is a correctness property with no syntactic
/// expression. [`zuno_tui::views::message::Transcript::replay`] is a no-op on a non-empty
/// transcript, so a notice pushed before it would silently discard the whole history and
/// leave `replayed()` reading zero — a resumed prompt would then be offered a revert whose
/// checkpoint no longer exists.
#[test]
fn the_resume_is_wired_into_the_tui_before_its_startup_notices() {
    let tui = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/tui.rs"),
    )
    .expect("tui.rs is readable");

    let replay_at = tui.find("match host.resumed_history()").expect(
        "`tui.rs` no longer replays a resumed session, so `-s` reopens a screen the \
                 model's own history contradicts",
    );
    let theme_notice_at = tui
        .find("for diagnostic in theme_diagnostics")
        .expect("the theme-diagnostic call site moved; this test's anchors need updating");
    let prompt_at = tui
        .find("screen.submit_prompt(prompt)")
        .expect("the command-line prompt call site moved; this test's anchors need updating");

    assert!(
        replay_at < theme_notice_at,
        "the replay must precede the startup notices, or `Transcript::replay` sees a \
         non-empty transcript and discards the whole session"
    );
    assert!(
        replay_at < prompt_at,
        "the replay must precede a command-line prompt, or `-s <id> \"…\"` loses its history"
    );
    assert_eq!(
        tui.matches("match host.resumed_history()").count(),
        1,
        "one replay site only: a second would re-read the session per launch for nothing"
    );
    assert!(
        tui.contains("failure_notice("),
        "`tui.rs` no longer reports an unreadable history, so a corrupt session would open \
         blank with no explanation — the original defect wearing a different hat"
    );
}

/// The resume reads the *retained* history, which is the one the model will be given.
///
/// Asserted at the call site because `TurnHost` cannot be built in a unit test — it needs
/// a resolved plan, a credential and an assembled dispatcher — while the choice it encodes
/// is the single most consequential one here. Swapping in
/// [`zuno_db::message::MessageStore::hydrate_session`] compiles, returns the same type, and
/// passes every behavioural test above, because those call the hydration directly. It would
/// silently put a compacted head on screen that the model has already forgotten, which is
/// the same screen-versus-model mismatch this whole change removes, pointed the other way.
#[test]
fn the_resume_reads_the_same_retained_history_the_next_request_will_send() {
    let turn = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/turn.rs"),
    )
    .expect("turn.rs is readable");

    let body = turn
        .split_once("pub(crate) fn resumed_history(")
        .map(|(_, rest)| rest.split_once("\n    }").expect("a closed body").0)
        .expect(
            "`TurnHost::resumed_history` is gone, so the TUI has no way to read a resumed \
             session's history at all",
        );

    assert!(
        body.contains("hydrate_retained_history"),
        "the resume must read the retained history, or the screen shows turns the model no \
         longer has: {body}"
    );
    assert!(
        !body.contains("hydrate_session"),
        "reading the full session would put a compacted head on screen that the request \
         will not carry: {body}"
    );
}
