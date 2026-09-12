use std::path::Path;

use serde_json::{Value, json};
use zuno_acp::{
    AttemptBufferedTurnEventProjector, ReplayPolicy, TurnEventProjector, durable_updates,
    turn_event_update,
};
use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};
use zuno_engine::r#loop::{ToolBlockKind, ToolDiff, ToolInterruption, TurnEvent};
use zuno_llm::event::StreamEvent;
use zuno_tool::{
    FileDiff, MutationConflictPresentation, ToolResultPresentation, ToolUiIntent,
    UncertainMutationPresentation,
};

const CALL: &str = "file-call";

fn call_started(name: &str) -> TurnEvent {
    TurnEvent::ToolCallStarted {
        step: 1,
        call_id: CALL.to_owned(),
        display_name: name.to_owned(),
        name: name.to_owned(),
        ui_intent: ToolUiIntent::Generic,
    }
}

fn input_delta(delta: &str) -> TurnEvent {
    TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ToolInputDelta {
            id: CALL.to_owned(),
            delta: delta.to_owned(),
        },
    }
}

fn dispatch_started(name: &str) -> TurnEvent {
    TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: CALL.to_owned(),
        display_name: name.to_owned(),
        name: name.to_owned(),
        ui_intent: ToolUiIntent::Generic,
    }
}

fn completion(name: &str, is_error: bool) -> TurnEvent {
    TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: CALL.to_owned(),
        display_name: name.to_owned(),
        name: name.to_owned(),
        title: "File operation".to_owned(),
        output: if is_error {
            "File operation failed; inspect the current file."
        } else {
            "File operation completed."
        }
        .to_owned(),
        diff: None,
        written_paths: Vec::new(),
        is_error,
    }
}

fn write_input(path: &str) -> Value {
    json!({"filePath": path, "content": "new contents\n"})
}

fn prime(name: &str, input: &Value) -> (TurnEventProjector, Value) {
    let mut projector = TurnEventProjector::new();
    let _ = projector.project(&call_started(name));
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ToolUseStart {
            id: CALL.to_owned(),
            name: name.to_owned(),
        },
    });
    let update = projector
        .project(&input_delta(&input.to_string()))
        .expect("a visible call receives its input");
    (projector, update)
}

fn locations(update: &Value) -> Vec<&str> {
    update
        .get("locations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|location| location["path"].as_str().expect("string location"))
        .collect()
}

fn assert_zed_standard_update(update: &Value) {
    assert!(matches!(
        update["sessionUpdate"].as_str(),
        Some("tool_call" | "tool_call_update")
    ));
    #[cfg(feature = "zed-schema-contract")]
    {
        // Filename visibility must survive a client that knows only ACP v1.
        let mut standard = update.clone();
        standard.as_object_mut().expect("update").remove("_meta");
        let decoded: agent_client_protocol_schema::v1::SessionUpdate =
            serde_json::from_value(standard).expect("Zed's pinned schema accepts the update");
        let encoded = serde_json::to_value(decoded).expect("round-trip ACP update");
        for field in ["title", "locations"] {
            if let Some(value) = update.get(field) {
                if field == "locations" && value.as_array().is_some_and(Vec::is_empty) {
                    // ToolCall defaults an omitted locations array to empty and
                    // omits that default when serializing it again.
                    assert!(locations(&encoded).is_empty());
                    continue;
                }
                assert_eq!(&encoded[field], value, "standard ACP {field} is retained");
            }
        }
    }
}

fn assert_file_card(update: &Value, filename: &str, path: &str) {
    let title = update["title"].as_str().unwrap_or_default();
    assert!(
        title.contains(filename),
        "missing filename {filename:?} in ACP title: {title:?}"
    );
    assert_eq!(
        locations(update),
        [path],
        "standard ACP locations: {update}"
    );
    assert_zed_standard_update(update);
}

#[test]
fn native_write_and_edit_publish_paths_after_complete_input_and_keep_them_at_dispatch() {
    let root = tempfile::tempdir().expect("fixture root");
    let path = zuno_paths::wire_path(&root.path().join("你好 file.rs"));
    for (name, mut input) in [
        ("write", write_input(&path)),
        (
            "edit",
            json!({
                "filePath": path,
                "edits": [{"oldString": "old", "newString": "new"}],
            }),
        ),
    ] {
        input["intent"] = json!("Update this file");
        input["accept_large_output"] = json!(false);
        let mut projector = TurnEventProjector::new();
        let pending = projector
            .project(&call_started(name))
            .expect("pending call");
        assert_eq!(pending["title"], "Editing files");
        assert!(locations(&pending).is_empty());
        assert_zed_standard_update(&pending);
        let _ = projector.project(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseStart {
                id: CALL.to_owned(),
                name: name.to_owned(),
            },
        });
        let encoded = input.to_string();
        let (prefix, suffix) = encoded.split_at(encoded.len() - 1);
        let partial = projector
            .project(&input_delta(prefix))
            .expect("raw input progress");
        assert!(partial["rawInput"].is_string());
        assert!(partial.get("title").is_none());
        assert!(locations(&partial).is_empty());
        let complete = projector
            .project(&input_delta(suffix))
            .expect("complete input progress");
        assert_eq!(complete["rawInput"], input);
        assert_file_card(&complete, "你好 file.rs", &path);
        let running = projector
            .project(&dispatch_started(name))
            .expect("dispatch start");
        assert_eq!(running["status"], "in_progress");
        assert_file_card(&running, "你好 file.rs", &path);
        let completed = projector
            .project(&completion(name, false))
            .expect("completion");
        assert_eq!(completed["status"], "completed");
        assert_file_card(&completed, "你好 file.rs", &path);
    }
}

#[test]
fn complete_input_before_the_call_is_visible_is_used_by_the_pending_card() {
    let path = "/workspace/early file.txt";
    let mut projector = TurnEventProjector::new();
    assert!(
        projector
            .project(&input_delta(&write_input(path).to_string()))
            .is_none()
    );
    let pending = projector.project(&call_started("write")).expect("pending");
    assert_file_card(&pending, "early file.txt", path);
}

#[test]
fn patch_paths_include_add_delete_and_move_without_treating_hunks_as_paths() {
    let paths = [
        "/workspace/new file.txt",
        "/workspace/deleted.rs",
        "/workspace/old 文件.rs",
        "/workspace/moved 文件.rs",
    ];
    let patch = format!(
        concat!(
            "*** Begin Patch\n*** Add File: {}\n+hello\n",
            "+*** Add File: /not-a-target/from-content\n",
            "*** Delete File: {}\n*** Update File: {}\n*** Move to: {}\n",
            "@@ literal header /not-a-target/from-header\n-old\n+new\n",
            " *** Delete File: /not-a-target/from-context\n",
            "+*** Move to: /not-a-target/from-addition\n*** End Patch\n",
        ),
        paths[0], paths[1], paths[2], paths[3]
    );
    let (mut projector, update) = prime("apply_patch", &json!({"patchText": patch}));
    assert_eq!(locations(&update), paths);
    assert!(
        update["title"]
            .as_str()
            .is_some_and(|title| title.contains("new file.txt") && !title.contains("not-a-target"))
    );
    assert_zed_standard_update(&update);
    let running = projector
        .project(&dispatch_started("apply_patch"))
        .expect("running patch");
    assert_eq!(locations(&running), paths);
    assert_zed_standard_update(&running);
}

#[test]
fn invalid_or_incomplete_input_never_creates_actionable_paths() {
    for (name, input) in [
        ("write", json!({"filePath": "/workspace/not-yet-valid"})),
        ("write", json!({"filePath": 9, "content": "text"})),
        (
            "edit",
            json!({"filePath": "/workspace/not-valid", "edits": "wrong type"}),
        ),
        (
            "write",
            json!({"filePath": "/workspace/rejected", "content": "text", "unknown": true}),
        ),
        (
            "apply_patch",
            json!({"patchText": "*** Begin Patch\n*** Add File: /workspace/incomplete\n+new\n"}),
        ),
        (
            "apply_patch",
            json!({"patchText": "*** Begin Patch\n*** Add File: /workspace/invalid\nnot-added\n*** End Patch"}),
        ),
        (
            "apply_patch",
            json!({"patchText": "*** Begin Patch\n*** Update File: /workspace/invalid\n@@\n*** Move to: /not-a-target\n*** End Patch"}),
        ),
        (
            "apply_patch",
            json!({"patchText": "*** Begin Patch\n*** Delete File: /workspace/invalid\n+not-valid\n*** End Patch"}),
        ),
        ("write", write_input("")),
        ("write", write_input("/workspace/nul\0file")),
    ] {
        let (_, update) = prime(name, &input);
        assert!(locations(&update).is_empty(), "{name}: {input}: {update}");
        assert!(update.get("title").is_none(), "{name}: {input}: {update}");
    }
}

#[test]
fn relative_patch_targets_are_named_without_guessing_the_session_working_directory() {
    let (_, update) = prime(
        "apply_patch",
        &json!({
            "patchText": "*** Begin Patch\n*** Add File: src/relative file.rs\n+new\n*** End Patch",
        }),
    );
    assert!(
        update["title"]
            .as_str()
            .is_some_and(|title| title.contains("relative file.rs"))
    );
    assert!(locations(&update).is_empty());
    assert_zed_standard_update(&update);
}

#[test]
fn windows_unicode_and_space_paths_are_portable_wire_locations() {
    for (native, wire) in [
        (r"C:\repo\目录\space file.rs", "C:/repo/目录/space file.rs"),
        (
            r"\\server\share\目录\space file.rs",
            "//server/share/目录/space file.rs",
        ),
        (
            r"\\?\C:\repo\目录\space file.rs",
            "C:/repo/目录/space file.rs",
        ),
        (
            r"\\?\UNC\server\share\目录\space file.rs",
            "//server/share/目录/space file.rs",
        ),
    ] {
        let (_, update) = prime("write", &write_input(native));
        assert_file_card(&update, "space file.rs", wire);
        assert_eq!(update["rawInput"]["filePath"], native);
    }
}

#[test]
fn long_unicode_filenames_and_many_targets_have_bounded_titles_and_complete_locations() {
    let path = format!("/workspace/{}.rs", "很长的文件名".repeat(80));
    let (_, update) = prime("write", &write_input(&path));
    let title = update["title"].as_str().expect("filename title");
    assert!(title.chars().count() <= 160, "{title}");
    assert!(title.ends_with(".rs"), "{title}");
    assert!(title.contains('…'), "{title}");
    assert_eq!(locations(&update), [path.as_str()]);
    assert_zed_standard_update(&update);

    let paths = (0..50)
        .map(|index| format!("/workspace/file {index}.txt"))
        .collect::<Vec<_>>();
    let patch = format!(
        "*** Begin Patch\n{}*** End Patch",
        paths
            .iter()
            .map(|path| format!("*** Add File: {path}\n+text\n"))
            .collect::<String>()
    );
    let (_, update) = prime("apply_patch", &json!({"patchText": patch}));
    let title = update["title"].as_str().expect("multi-file title");
    assert!(title.chars().count() <= 160, "{title}");
    assert!(
        title.contains("file 0.txt") && title.contains("more"),
        "{title}"
    );
    assert_eq!(locations(&update), paths);
}

#[test]
fn terminal_native_paths_override_input_aliases_and_keep_exact_diff_images() {
    let root = tempfile::tempdir().expect("fixture root");
    let actual = root.path().join("resolved 文件.rs");
    let deleted = root.path().join("deleted file.rs");
    let actual_wire = zuno_paths::wire_path(&actual);
    let deleted_wire = zuno_paths::wire_path(&deleted);
    let (mut projector, _) = prime("write", &write_input("/workspace/input-alias.rs"));
    let completed = projector
        .project(&TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: CALL.to_owned(),
            display_name: "Write".to_owned(),
            name: "write".to_owned(),
            title: "File operation".to_owned(),
            output: "File operation completed.".to_owned(),
            is_error: false,
            diff: ToolDiff::new(
                None,
                vec![
                    FileDiff::new(&actual, Some("old\n".to_owned()), "new\n".to_owned())
                        .expect("native file diff"),
                    FileDiff::new(&deleted, Some("deleted\n".to_owned()), String::new())
                        .expect("native delete diff"),
                ],
            ),
            written_paths: vec![actual_wire.clone(), actual_wire.clone()],
        })
        .expect("completion");
    let title = completed["title"].as_str().expect("completion title");
    assert!(
        title.contains("resolved 文件.rs") && title.contains("deleted file.rs"),
        "{title}"
    );
    assert!(!title.contains("input-alias"));
    assert_eq!(
        locations(&completed),
        [actual_wire.as_str(), deleted_wire.as_str()]
    );
    assert_eq!(completed["content"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        completed["content"][0],
        json!({"type": "diff", "path": actual_wire, "oldText": "old\n", "newText": "new\n"})
    );
    assert_eq!(completed["content"][1]["newText"], "");
    assert_zed_standard_update(&completed);
}

#[test]
fn failed_cancelled_blocked_and_provider_result_updates_keep_target_identity() {
    let path = "/workspace/failed file.rs";
    for interruption in [ToolInterruption::Cooperative, ToolInterruption::Forced] {
        let (mut projector, _) = prime("write", &write_input(path));
        let update = projector
            .project(&TurnEvent::ToolDispatchInterrupted {
                step: 1,
                call_id: CALL.to_owned(),
                display_name: "Write".to_owned(),
                name: "write".to_owned(),
                title: "Interrupted".to_owned(),
                output: "The call was interrupted.".to_owned(),
                interruption,
                uncertain: true,
            })
            .expect("interrupted call");
        assert_file_card(&update, "failed file.rs", path);
        assert_eq!(update["status"], "failed");
        assert_eq!(update["_meta"]["zuno"]["outcome"], "uncertain");
        assert_eq!(update["_meta"]["zuno"]["cancelled"], true);
        assert_eq!(update["_meta"]["zuno"]["forced"], interruption.is_forced());
        assert_eq!(
            update["content"][0]["content"]["text"],
            "The call was interrupted."
        );
    }
    let (mut projector, _) = prime("write", &write_input(path));
    let blocked = projector
        .project(&TurnEvent::ToolDispatchBlocked {
            step: 1,
            call_id: CALL.to_owned(),
            kind: ToolBlockKind::Denied,
        })
        .expect("blocked call");
    assert_file_card(&blocked, "failed file.rs", path);
    assert_eq!(blocked["rawOutput"]["blocked"], true);
    assert_eq!(blocked["status"], "failed");
    // The engine emits completion after the block notice. That second update must
    // not overwrite the named card with a generic title.
    let failed = projector
        .project(&completion("write", true))
        .expect("failed completion after block");
    assert_file_card(&failed, "failed file.rs", path);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["content"].as_array().map(Vec::len), Some(1));

    let (mut projector, _) = prime("write", &write_input(path));
    let result = projector
        .project(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolResult {
                tool_use_id: CALL.to_owned(),
                content: "Provider tool result".to_owned(),
                is_error: true,
            },
        })
        .expect("provider result");
    assert_file_card(&result, "failed file.rs", path);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["rawOutput"], "Provider tool result");
}

#[test]
fn uncertain_and_conflict_presentations_supply_paths_without_claiming_other_writes() {
    let actual = "/workspace/partially applied.rs";
    let (mut projector, _) = prime("write", &write_input("/workspace/unobserved.rs"));
    let _ = projector.project(&TurnEvent::ToolResultPresented {
        step: 1,
        call_id: CALL.to_owned(),
        presentation: ToolResultPresentation::UncertainMutation(
            UncertainMutationPresentation::new(vec![actual.to_owned()]),
        ),
    });
    let update = projector
        .project(&completion("write", true))
        .expect("uncertain");
    assert_file_card(&update, "partially applied.rs", actual);
    assert_eq!(update["_meta"]["zuno"]["appliedPaths"], json!([actual]));
    assert_eq!(update["_meta"]["zuno"]["outcome"], "uncertain");
    assert_eq!(update["content"].as_array().map(Vec::len), Some(1));

    let path = "/workspace/conflicting file.rs";
    let mut projector = TurnEventProjector::new();
    let _ = projector.project(&TurnEvent::ToolResultPresented {
        step: 1,
        call_id: CALL.to_owned(),
        presentation: ToolResultPresentation::MutationConflict(
            MutationConflictPresentation::from_conflict(&zuno_error::ToolMutationConflict {
                kind: zuno_error::ToolMutationConflictKind::ContextMismatch,
                resource: path.to_owned(),
                operation_digest: "operation".to_owned(),
                observed_digest: Some("observed".to_owned()),
                hunk_index: Some(1),
                hunk_header: Some("real hunk".to_owned()),
            }),
        ),
    });
    let update = projector
        .project(&completion("apply_patch", true))
        .expect("conflict");
    assert_file_card(&update, "conflicting file.rs", path);
    assert_eq!(
        update["_meta"]["zuno"]["mutationConflict"]["requiredAction"],
        "reread_and_revise"
    );
    assert_eq!(update["status"], "failed");
}

fn history(name: &str, state: Value) -> Vec<MessageWithParts> {
    vec![MessageWithParts {
        info: MessageRecord::from_json(json!({
            "id": "message",
            "sessionID": "session",
            "role": "assistant",
            "time": {"created": 1},
        }))
        .expect("message fixture"),
        parts: vec![
            PartRecord::from_json(
                json!({
                    "id": "part",
                    "messageID": "message",
                    "sessionID": "session",
                    "type": "tool",
                    "callID": CALL,
                    "tool": name,
                    "displayName": name,
                    "state": state,
                }),
                1,
            )
            .expect("part fixture"),
        ],
    }]
}

#[test]
fn history_pending_running_success_error_and_cancellation_retain_file_identity() {
    let root = tempfile::tempdir().expect("fixture root");
    let path = root.path().join("history 文件.rs");
    std::fs::write(&path, "old\n").expect("fixture file");
    let wire = zuno_paths::wire_path(&path);
    for status in ["pending", "running", "completed", "error"] {
        let state = json!({
            "status": status,
            "input": write_input(&wire),
            "title": "File operation",
            "output": "Historical output",
        });
        let replay = durable_updates(
            &history("write", state),
            &ReplayPolicy::for_workspace(root.path()),
            0,
        );
        for update in &replay.updates {
            assert_file_card(update, "history 文件.rs", &wire);
        }
        if status == "error" {
            assert_eq!(replay.updates[1]["status"], "failed");
        }
    }
    let replay = durable_updates(
        &history(
            "write",
            json!({
                "status": "error",
                "input": write_input(&wire),
                "error": "Interrupted",
                "metadata": {"interruption": {"mode": "cooperative", "uncertain": true}},
            }),
        ),
        &ReplayPolicy::for_workspace(root.path()),
        0,
    );
    assert_file_card(&replay.updates[1], "history 文件.rs", &wire);
    assert_eq!(replay.updates[1]["_meta"]["zuno"]["outcome"], "uncertain");
    assert_eq!(replay.updates[1]["_meta"]["zuno"]["forced"], false);
}

#[test]
fn history_uses_complete_durable_input_when_raw_json_was_incomplete() {
    let root = tempfile::tempdir().expect("fixture root");
    let path = root.path().join("durable.rs");
    std::fs::write(&path, "old\n").expect("fixture file");
    let wire = zuno_paths::wire_path(&path);
    let replay = durable_updates(
        &history(
            "write",
            json!({
                "status": "error",
                "raw": "{\"filePath\":",
                "input": write_input(&wire),
                "error": "Stopped",
            }),
        ),
        &ReplayPolicy::for_workspace(root.path()),
        0,
    );
    for update in &replay.updates {
        assert_file_card(update, "durable.rs", &wire);
    }
}

#[test]
fn history_preserves_non_file_input_shapes_and_complete_raw_input_precedence() {
    let root = tempfile::tempdir().expect("fixture root");
    for input in [json!("literal argument"), json!(["argument"]), json!(17)] {
        let replay = durable_updates(
            &history("plugin", json!({"status": "pending", "input": input})),
            &ReplayPolicy::for_workspace(root.path()),
            0,
        );
        assert_eq!(replay.updates[0]["rawInput"], input);
    }
    let replay = durable_updates(
        &history(
            "shell",
            json!({
                "status": "running",
                "raw": r#"{"command":"submitted command"}"#,
                "input": {"command": "other representation"},
            }),
        ),
        &ReplayPolicy::for_workspace(root.path()),
        0,
    );
    assert_eq!(
        replay.updates[0]["rawInput"]["command"],
        "submitted command"
    );
    assert_eq!(replay.updates[0]["title"], "submitted command");
}

#[test]
fn replay_names_deleted_and_moved_files_but_filters_unopenable_locations_and_diffs() {
    let root = tempfile::tempdir().expect("fixture root");
    let destination = root.path().join("new file.rs");
    std::fs::write(&destination, "contents\n").expect("move destination");
    let destination = zuno_paths::wire_path(&destination);
    let source = zuno_paths::wire_path(&root.path().join("old file.rs"));
    let deleted = zuno_paths::wire_path(&root.path().join("deleted file.rs"));
    let replay = durable_updates(
        &history(
            "apply_patch",
            json!({
                "status": "completed",
                "output": "Moved and deleted files",
                "metadata": {
                    "writtenPaths": [destination],
                    "fileDiffs": [
                        {"path": source, "oldText": "contents\n", "newText": ""},
                        {"path": destination, "oldText": null, "newText": "contents\n"},
                        {"path": deleted, "oldText": "gone\n", "newText": ""},
                    ],
                },
            }),
        ),
        &ReplayPolicy::for_workspace(root.path()),
        0,
    );
    let completed = &replay.updates[1];
    let title = completed["title"].as_str().expect("historical title");
    for filename in ["new file.rs", "old file.rs", "deleted file.rs"] {
        assert!(title.contains(filename), "{title}");
    }
    assert_eq!(locations(completed), [destination.as_str()]);
    assert_eq!(completed["content"].as_array().map(Vec::len), Some(1));
    assert_eq!(completed["content"][0]["path"], destination);
    assert_zed_standard_update(completed);
}

#[test]
fn replay_input_paths_do_not_bypass_workspace_and_existence_filters() {
    let root = tempfile::tempdir().expect("fixture root");
    let outside = tempfile::tempdir().expect("outside fixture root");
    let external = outside.path().join("external.rs");
    std::fs::write(&external, "outside\n").expect("outside fixture");
    for path in [external, root.path().join("missing.rs")] {
        let wire = zuno_paths::wire_path(&path);
        let filename = path.file_name().expect("filename").to_str().expect("UTF-8");
        let replay = durable_updates(
            &history(
                "write",
                json!({"status": "error", "input": write_input(&wire), "error": "failed"}),
            ),
            &ReplayPolicy::for_workspace(root.path()),
            0,
        );
        for update in replay.updates {
            assert!(
                update["title"]
                    .as_str()
                    .is_some_and(|title| title.contains(filename)),
                "{update}"
            );
            assert!(locations(&update).is_empty(), "{update}");
            assert_zed_standard_update(&update);
        }
    }
}

#[test]
#[cfg(unix)]
fn replay_input_symlink_escape_is_not_an_actionable_location() {
    let root = tempfile::tempdir().expect("fixture root");
    let outside = tempfile::tempdir().expect("outside root");
    let external = outside.path().join("external.rs");
    std::fs::write(&external, "outside\n").expect("outside file");
    let link = root.path().join("linked.rs");
    std::os::unix::fs::symlink(external, &link).expect("escaping symlink fixture");
    let replay = durable_updates(
        &history(
            "write",
            json!({
                "status": "pending",
                "input": write_input(&zuno_paths::wire_path(&link)),
            }),
        ),
        &ReplayPolicy::for_workspace(root.path()),
        0,
    );
    assert!(locations(&replay.updates[0]).is_empty());
}

#[test]
fn buffered_provider_retries_discard_failed_attempt_filenames() {
    let mut projector = AttemptBufferedTurnEventProjector::new();
    let _ = projector.project(&TurnEvent::ProviderRequestStarted {
        step: 1,
        message_count: 1,
        estimated_prompt_tokens: 12,
    });
    assert!(projector.project(&call_started("write")).is_empty());
    assert!(
        projector
            .project(&input_delta(
                &write_input("/workspace/discarded.rs").to_string()
            ))
            .is_empty()
    );
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
    });
    assert!(projector.project(&call_started("write")).is_empty());
    assert!(
        projector
            .project(&input_delta(&write_input("/workspace/kept.rs").to_string()))
            .is_empty()
    );
    let committed = projector.project(&TurnEvent::AssistantCheckpointed {
        step: 1,
        message_id: "message".to_owned(),
        interrupted: false,
    });
    assert_eq!(committed.len(), 2);
    assert!(
        !serde_json::to_string(&committed)
            .expect("wire updates")
            .contains("discarded.rs")
    );
    assert_file_card(&committed[1], "kept.rs", "/workspace/kept.rs");
}

#[test]
fn task_and_shell_arguments_do_not_become_file_mutation_locations() {
    let (_, shell) = prime(
        "shell",
        &json!({"command": "printf hello", "filePath": "/not-a-target", "cwd": "/workspace"}),
    );
    assert_eq!(shell["title"], "printf hello");
    assert!(locations(&shell).is_empty());
    let (_, task) = prime(
        "task",
        &json!({
            "agent": "explore",
            "objective": "Inspect files",
            "scope": {"include": ["/workspace/file.rs"]},
        }),
    );
    assert!(locations(&task).is_empty());
    assert!(
        task["title"]
            .as_str()
            .is_some_and(|title| title.starts_with("Delegate"))
    );
}

#[test]
fn blocked_non_file_tools_keep_their_existing_completion_content() {
    for (name, input) in [
        (
            "task",
            json!({"agent": "explore", "objective": "Inspect files"}),
        ),
        (
            "question",
            json!({"questions": [{"header": "Choice", "question": "Proceed?"}]}),
        ),
    ] {
        let (mut projector, _) = prime(name, &input);
        let _ = projector.project(&TurnEvent::ToolDispatchBlocked {
            step: 1,
            call_id: CALL.to_owned(),
            kind: ToolBlockKind::Denied,
        });
        let completed = projector
            .project(&completion(name, true))
            .expect("failed completion");
        assert_eq!(completed["content"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            completed["content"][0]["content"]["text"],
            "File operation failed; inspect the current file."
        );
    }
}

#[test]
fn standalone_delete_and_move_results_use_typed_paths() {
    let root = tempfile::tempdir().expect("fixture root");
    let path = root.path().join("native file.rs");
    let wire = zuno_paths::wire_path(&path);
    for name in ["delete", "move"] {
        let update = turn_event_update(&TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: CALL.to_owned(),
            display_name: name.to_owned(),
            name: name.to_owned(),
            title: "Operation complete".to_owned(),
            output: "Completed".to_owned(),
            diff: ToolDiff::new(
                None,
                vec![
                    FileDiff::new(Path::new(&path), Some("old\n".to_owned()), String::new())
                        .expect("native deletion"),
                ],
            ),
            written_paths: Vec::new(),
            is_error: false,
        })
        .expect("native path update");
        assert_file_card(&update, "native file.rs", &wire);
        assert_eq!(update["kind"], name);
    }
}
