use serde_json::json;
use zuno_acp::{IMPLEMENTED_METHODS, TurnEventProjector, turn_event_update};
use zuno_engine::r#loop::{ToolBlockKind, ToolDiff, TurnEvent};
use zuno_llm::event::{PromptAccounting, StreamEvent};
use zuno_tool::{FileDiff, ToolUiIntent};

#[test]
fn adapter_exposes_exactly_the_stable_v1_21_agent_methods() {
    assert_eq!(
        IMPLEMENTED_METHODS,
        [
            "initialize",
            "session/new",
            "session/load",
            "session/set_mode",
            "session/set_config_option",
            "session/prompt",
            "session/cancel",
            "session/list",
            "session/delete",
            "session/resume",
            "session/close",
        ]
    );
}

#[test]
fn engine_stream_events_project_to_protocol_updates() {
    let text = turn_event_update(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::TextDelta("hello".to_owned()),
    })
    .expect("text is client-visible");
    assert_eq!(text["sessionUpdate"], "agent_message_chunk");
    assert_eq!(text["content"]["type"], "text");
    assert_eq!(text["content"]["text"], "hello");

    let pending = turn_event_update(&TurnEvent::ToolCallStarted {
        step: 1,
        call_id: "call-1".to_owned(),
        display_name: "zsh".to_owned(),
        name: "shell".to_owned(),
        ui_intent: ToolUiIntent::Generic,
    })
    .expect("pending tool call is client-visible");
    assert_eq!(pending["sessionUpdate"], "tool_call");
    assert_eq!(pending["toolCallId"], "call-1");
    assert_eq!(pending["title"], "zsh");
    assert_eq!(pending["status"], "pending");
    assert!(
        turn_event_update(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseStart {
                id: "call-1".to_owned(),
                name: "shell".to_owned(),
            },
        })
        .is_none(),
        "the raw provider event must not publish a second wire-name tool row"
    );

    let started = turn_event_update(&TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: "call-1".to_owned(),
        display_name: "write".to_owned(),
        name: "write".to_owned(),
        ui_intent: ToolUiIntent::Generic,
    })
    .expect("tool start is client-visible");
    assert_eq!(started["sessionUpdate"], "tool_call");
    assert_eq!(started["toolCallId"], "call-1");
    assert_eq!(started["status"], "in_progress");

    let completed = turn_event_update(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: "call-1".to_owned(),
        display_name: "write".to_owned(),
        name: "write".to_owned(),
        title: "Wrote file".to_owned(),
        output: "ok".to_owned(),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    })
    .expect("tool completion is client-visible");
    assert_eq!(completed["sessionUpdate"], "tool_call_update");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["content"][0]["content"]["text"], "ok");
}

#[test]
fn stateful_projection_accumulates_tool_input_without_emitting_invalid_json() {
    let mut projector = TurnEventProjector::new();
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::ToolInputDelta {
                    id: "call-1".to_owned(),
                    delta: "{\"path\":".to_owned(),
                },
            })
            .is_none(),
        "raw input must wait until the tool call exists on the client"
    );

    let pending = projector
        .project(&TurnEvent::ToolCallStarted {
            step: 1,
            call_id: "call-1".to_owned(),
            display_name: "Read".to_owned(),
            name: "read".to_owned(),
            ui_intent: ToolUiIntent::Generic,
        })
        .expect("the pending tool call is projected");
    assert_eq!(pending["rawInput"], "{\"path\":");

    let completed_input = projector
        .project(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolInputDelta {
                id: "call-1".to_owned(),
                delta: "\"src/lib.rs\"}".to_owned(),
            },
        })
        .expect("a started tool gets raw input updates");
    assert_eq!(completed_input["sessionUpdate"], "tool_call_update");
    assert_eq!(completed_input["toolCallId"], "call-1");
    assert_eq!(completed_input["rawInput"], json!({ "path": "src/lib.rs" }));
}

#[test]
fn blocked_tools_are_projected_as_failed_with_typed_output() {
    let mut projector = TurnEventProjector::new();
    let blocked = projector
        .project(&TurnEvent::ToolDispatchBlocked {
            step: 1,
            call_id: "call-denied".to_owned(),
            kind: ToolBlockKind::Denied,
        })
        .expect("blocked dispatches are client-visible");
    assert_eq!(blocked["sessionUpdate"], "tool_call_update");
    assert_eq!(blocked["status"], "failed");
    assert_eq!(
        blocked["rawOutput"],
        json!({ "blocked": true, "kind": "denied" })
    );
}

#[test]
fn completed_tools_project_native_file_diffs_locations_and_json_output() {
    let mut projector = TurnEventProjector::new();
    let patch = concat!(
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -1 +1 @@\n",
        "-old\n",
        "+new\n",
    );
    let completed = projector
        .project(&TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: "call-write".to_owned(),
            display_name: "Apply patch".to_owned(),
            name: "apply_patch".to_owned(),
            title: "Updated files".to_owned(),
            output: r#"{"ok":true,"changed":2}"#.to_owned(),
            diff: ToolDiff::new(
                Some(patch.to_owned()),
                vec![
                    FileDiff::new(
                        std::path::Path::new("/workspace/src/lib.rs"),
                        Some("old\n".to_owned()),
                        "new\n".to_owned(),
                    )
                    .expect("changed absolute file"),
                    FileDiff::new(
                        std::path::Path::new("/workspace/src/new.rs"),
                        None,
                        "created\n".to_owned(),
                    )
                    .expect("created absolute file"),
                    FileDiff::new(
                        std::path::Path::new("/workspace/src/deleted.rs"),
                        Some("removed\n".to_owned()),
                        String::new(),
                    )
                    .expect("deleted absolute file"),
                ],
            ),
            written_paths: vec![
                "/workspace/src/lib.rs".to_owned(),
                "/workspace/src/new.rs".to_owned(),
                "/workspace/src/lib.rs".to_owned(),
            ],
            is_error: false,
        })
        .expect("completed dispatches are client-visible");
    assert_eq!(completed["rawOutput"], json!({ "ok": true, "changed": 2 }));
    assert_eq!(
        completed["locations"],
        json!([
            { "path": "/workspace/src/lib.rs" },
            { "path": "/workspace/src/new.rs" },
            { "path": "/workspace/src/deleted.rs" },
        ])
    );
    assert_eq!(
        completed["content"][1],
        json!({
            "type": "diff",
            "path": "/workspace/src/lib.rs",
            "oldText": "old\n",
            "newText": "new\n",
        })
    );
    assert_eq!(
        completed["content"][2],
        json!({
            "type": "diff",
            "path": "/workspace/src/new.rs",
            "oldText": null,
            "newText": "created\n",
        })
    );
    assert_eq!(completed["content"][3]["path"], "/workspace/src/deleted.rs");
    assert_eq!(completed["content"][3]["newText"], "");
}

#[test]
fn unified_diff_is_kept_as_an_extension_fallback_for_untyped_tools() {
    let patch = "@@ -1 +1 @@\n-old\n+new\n";
    let completed = turn_event_update(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: "call-plugin".to_owned(),
        display_name: "Plugin edit".to_owned(),
        name: "plugin_edit".to_owned(),
        title: "Updated".to_owned(),
        output: "ok".to_owned(),
        diff: ToolDiff::new(Some(patch.to_owned()), Vec::new()),
        written_paths: Vec::new(),
        is_error: false,
    })
    .expect("completed dispatch is client-visible");
    assert_eq!(completed["content"][1]["type"], "content");
    assert_eq!(completed["content"][1]["content"]["text"], patch);
    assert_eq!(
        completed["content"][1]["_meta"]["zuno"]["kind"],
        "unified_diff"
    );
}

#[test]
fn usage_updates_require_an_explicit_context_window() {
    let usage = TurnEvent::Provider {
        step: 1,
        event: StreamEvent::TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(25),
            cache_read_input_tokens: Some(40),
            cache_write_input_tokens: Some(10),
            accounting: PromptAccounting::CacheBesideInput,
        },
    };

    assert!(
        TurnEventProjector::new().project(&usage).is_none(),
        "unknown context size must not be represented as a made-up ACP size"
    );
    let update = TurnEventProjector::with_context_size(200_000)
        .project(&usage)
        .expect("known context size enables usage projection");
    assert_eq!(update["sessionUpdate"], "usage_update");
    assert_eq!(update["used"], 175);
    assert_eq!(update["size"], 200_000);
}
