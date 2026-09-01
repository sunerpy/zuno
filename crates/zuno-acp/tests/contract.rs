use serde_json::json;
use zuno_acp::{
    AttemptBufferedTurnEventProjector, IMPLEMENTED_METHODS, TurnEventProjector, turn_event_update,
};
use zuno_engine::r#loop::{ToolBlockKind, ToolDiff, ToolInterruption, TurnEvent};
use zuno_llm::event::{PromptAccounting, StreamEvent};
use zuno_tool::{
    FileDiff, QuestionResultPresentation, QuestionResultStatus, ToolResultPresentation,
    ToolUiIntent,
};

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

    let command_output = turn_event_update(&TurnEvent::SessionCommandOutput {
        command: zuno_engine::session_command::SessionCommand::Goal,
        content: "{\n  \"objective\": \"ship ACP commands\"\n}".to_owned(),
    })
    .expect("native command output is client-visible");
    assert_eq!(command_output["sessionUpdate"], "agent_message_chunk");
    assert_eq!(
        command_output["content"]["text"],
        "{\n  \"objective\": \"ship ACP commands\"\n}"
    );

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
    assert_eq!(started["sessionUpdate"], "tool_call_update");
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
fn operational_status_is_not_projected_as_agent_thought() {
    let reasoning = turn_event_update(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ReasoningDelta("real reasoning".to_owned()),
    })
    .expect("real reasoning remains client-visible");
    assert_eq!(reasoning["sessionUpdate"], "agent_thought_chunk");
    assert_eq!(reasoning["content"]["text"], "real reasoning");

    let title = turn_event_update(&TurnEvent::SessionTitleUpdated {
        title: "优化 FAQ 中 Shell 沙箱说明".to_owned(),
    })
    .expect("a generated title is a typed session metadata update");
    assert_eq!(title["sessionUpdate"], "session_info_update");
    assert_eq!(title["title"], "优化 FAQ 中 Shell 沙箱说明");

    assert!(
        turn_event_update(&TurnEvent::Provider {
            step: 0,
            event: StreamEvent::StatusDetail {
                detail: "history compacted before this turn".to_owned(),
            },
        })
        .is_none(),
        "operational status has no ACP v1 thought semantics"
    );
    assert!(
        turn_event_update(&TurnEvent::Provider {
            step: 1,
            event: StreamEvent::Error {
                message: "provider retrying".to_owned(),
                retry_after: None,
            },
        })
        .is_none(),
        "provider status errors must not masquerade as model reasoning"
    );
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
fn shell_tool_projects_a_copyable_command_and_separate_interpreter_identity() {
    let mut projector = TurnEventProjector::new();
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ToolUseStart {
            id: "call-shell".to_owned(),
            name: "shell".to_owned(),
        },
    });
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ToolInputDelta {
            id: "call-shell".to_owned(),
            delta: r#"{"command":"git diff --check"}"#.to_owned(),
        },
    });

    let pending = projector
        .project(&TurnEvent::ToolCallStarted {
            step: 1,
            call_id: "call-shell".to_owned(),
            display_name: "zsh".to_owned(),
            name: "shell".to_owned(),
            ui_intent: ToolUiIntent::Generic,
        })
        .expect("the pending shell call is projected");
    assert_eq!(pending["title"], "git diff --check");
    assert_eq!(pending["rawInput"]["command"], "git diff --check");
    assert_eq!(pending["_meta"]["zuno"]["interpreter"], "zsh");

    let running = projector
        .project(&TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: "call-shell".to_owned(),
            display_name: "zsh".to_owned(),
            name: "shell".to_owned(),
            ui_intent: ToolUiIntent::Generic,
        })
        .expect("the running shell call is projected");
    assert_eq!(running["title"], "git diff --check");
    assert_eq!(running["_meta"]["zuno"]["interpreter"], "zsh");

    let completed = projector
        .project(&TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: "call-shell".to_owned(),
            display_name: "zsh".to_owned(),
            name: "shell".to_owned(),
            title: "zsh git diff --check".to_owned(),
            output: "(no output)".to_owned(),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        })
        .expect("the completed shell call is projected");
    assert_eq!(completed["title"], "git diff --check");
    assert_eq!(completed["_meta"]["zuno"]["interpreter"], "zsh");
}

#[test]
fn live_question_completion_uses_the_typed_authoritative_answer() {
    let mut projector = TurnEventProjector::new();
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ToolUseStart {
            id: "call-question".to_owned(),
            name: "question".to_owned(),
        },
    });
    let _ = projector.project(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::ToolInputDelta {
            id: "call-question".to_owned(),
            delta: serde_json::to_string(&json!({
                "questions": [{
                    "header": "Database",
                    "question": "Which database?",
                    "options": [
                        {"label": "Postgres", "description": "Relational"},
                        {"label": "SQLite", "description": "Embedded"}
                    ]
                }]
            }))
            .expect("question input"),
        },
    });
    let _ = projector.project(&TurnEvent::ToolCallStarted {
        step: 1,
        call_id: "call-question".to_owned(),
        display_name: "Question".to_owned(),
        name: "question".to_owned(),
        ui_intent: ToolUiIntent::Generic,
    });
    assert!(
        projector
            .project(&TurnEvent::ToolResultPresented {
                step: 1,
                call_id: "call-question".to_owned(),
                presentation: ToolResultPresentation::Question(QuestionResultPresentation::new(
                    QuestionResultStatus::Answered,
                    Some(vec![vec!["SQLite".to_owned()]]),
                    1,
                    12,
                ),),
            })
            .is_none()
    );

    let completed = projector
        .project(&TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: "call-question".to_owned(),
            display_name: "Question".to_owned(),
            name: "question".to_owned(),
            title: "Answered · 1 question · <1s".to_owned(),
            output: concat!(
                "User has answered your questions: ",
                "\"Which database?\"=\"SQLite\". ",
                "You can now continue with the user's answers in mind."
            )
            .to_owned(),
            diff: None,
            written_paths: Vec::new(),
            is_error: false,
        })
        .expect("completed question projection");

    assert_eq!(completed["title"], "Answered · 1 question · <1s");
    assert_eq!(completed["_meta"]["zuno"]["question"]["status"], "answered");
    let content = completed["content"]
        .as_array()
        .expect("question completion content");
    assert_eq!(content.len(), 1);
    let card = content[0]["content"]["text"]
        .as_str()
        .expect("question card");
    assert!(card.contains("Which database?"), "{card}");
    assert!(card.contains("Selected: SQLite"), "{card}");
    assert_eq!(completed["_meta"]["zuno"]["question"]["questionCount"], 1);
    assert_eq!(completed["_meta"]["zuno"]["question"]["elapsedMs"], 12);
    assert!(
        completed["rawOutput"]
            .as_str()
            .is_some_and(|text| text.contains("\"Which database?\"=\"SQLite\""))
    );
}

#[test]
fn answered_question_remains_in_progress_until_the_continuation_is_checkpointed() {
    let mut projector = AttemptBufferedTurnEventProjector::new();
    let question_input = serde_json::to_string(&json!({
        "questions": [{
            "header": "Database",
            "question": "Which database?",
            "options": [
                {"label": "Postgres", "description": "Relational"},
                {"label": "SQLite", "description": "Embedded"}
            ]
        }]
    }))
    .expect("question input");

    assert!(
        projector
            .project(&TurnEvent::ProviderRequestStarted {
                step: 1,
                message_count: 1,
                estimated_prompt_tokens: 12,
            })
            .is_empty()
    );
    for event in [
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseStart {
                id: "call-question".to_owned(),
                name: "question".to_owned(),
            },
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolInputDelta {
                id: "call-question".to_owned(),
                delta: question_input,
            },
        },
        TurnEvent::ToolCallStarted {
            step: 1,
            call_id: "call-question".to_owned(),
            display_name: "Question".to_owned(),
            name: "question".to_owned(),
            ui_intent: ToolUiIntent::Generic,
        },
    ] {
        assert!(projector.project(&event).is_empty());
    }
    let admitted = projector.project(&TurnEvent::AssistantCheckpointed {
        step: 1,
        message_id: "msg-question".to_owned(),
        interrupted: false,
    });
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0]["status"], "pending");

    let running = projector.project(&TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: "call-question".to_owned(),
        display_name: "Question".to_owned(),
        name: "question".to_owned(),
        ui_intent: ToolUiIntent::Generic,
    });
    assert_eq!(running.len(), 1);
    assert_eq!(running[0]["status"], "in_progress");

    assert!(
        projector
            .project(&TurnEvent::ToolResultPresented {
                step: 1,
                call_id: "call-question".to_owned(),
                presentation: ToolResultPresentation::Question(QuestionResultPresentation::new(
                    QuestionResultStatus::Answered,
                    Some(vec![vec!["SQLite".to_owned()]]),
                    1,
                    292_976,
                ),),
            })
            .is_empty()
    );
    let continuing = projector.project(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: "call-question".to_owned(),
        display_name: "Question".to_owned(),
        name: "question".to_owned(),
        title: "Answered · 1 question · 4m 52s".to_owned(),
        output: concat!(
            "User has answered your questions: ",
            "\"Which database?\"=\"SQLite\". ",
            "You can now continue with the user's answers in mind."
        )
        .to_owned(),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    });
    assert_eq!(continuing.len(), 1);
    assert_eq!(continuing[0]["status"], "in_progress");
    assert_eq!(
        continuing[0]["_meta"]["zuno"]["question"]["status"],
        "answered"
    );
    assert_eq!(
        continuing[0]["_meta"]["zuno"]["question"]["continuationPending"],
        true
    );
    assert!(
        continuing[0]["content"][0]["content"]["text"]
            .as_str()
            .is_some_and(|card| card.contains("Selected: SQLite"))
    );

    assert!(
        projector
            .project(&TurnEvent::ProviderRequestStarted {
                step: 2,
                message_count: 3,
                estimated_prompt_tokens: 48,
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::TextDelta("discarded".to_owned()),
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::TextDelta("Configured SQLite".to_owned()),
            })
            .is_empty()
    );

    let committed = projector.project(&TurnEvent::AssistantCheckpointed {
        step: 2,
        message_id: "msg-continuation".to_owned(),
        interrupted: false,
    });
    assert_eq!(committed.len(), 2);
    assert_eq!(committed[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(committed[0]["status"], "completed");
    assert_eq!(
        committed[0]["_meta"]["zuno"]["question"]["continuationPending"],
        false
    );
    assert_eq!(committed[1]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(committed[1]["content"]["text"], "Configured SQLite");
    assert!(
        committed
            .iter()
            .all(|update| update["content"]["text"] != "discarded")
    );
}

#[test]
fn event_stream_close_settles_a_deferred_question_without_partial_output() {
    let mut projector = AttemptBufferedTurnEventProjector::new();
    assert!(
        projector
            .project(&TurnEvent::ToolResultPresented {
                step: 1,
                call_id: "call-question".to_owned(),
                presentation: ToolResultPresentation::Question(QuestionResultPresentation::new(
                    QuestionResultStatus::Answered,
                    Some(vec![vec!["SQLite".to_owned()]]),
                    1,
                    5,
                ),),
            })
            .is_empty()
    );
    let continuing = projector.project(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: "call-question".to_owned(),
        display_name: "Question".to_owned(),
        name: "question".to_owned(),
        title: "Answered · 1 question · <1s".to_owned(),
        output: "accepted".to_owned(),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    });
    assert_eq!(continuing[0]["status"], "in_progress");

    assert!(
        projector
            .project(&TurnEvent::ProviderRequestStarted {
                step: 2,
                message_count: 3,
                estimated_prompt_tokens: 48,
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 2,
                event: StreamEvent::TextDelta("must not escape".to_owned()),
            })
            .is_empty()
    );

    let settled = projector.finish();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0]["status"], "completed");
    assert_eq!(settled[0]["rawOutput"], "accepted");
    assert!(
        settled
            .iter()
            .all(|update| update["content"]["text"] != "must not escape")
    );
}

#[test]
fn a_non_answered_question_completion_is_not_extended() {
    let mut projector = AttemptBufferedTurnEventProjector::new();
    assert!(
        projector
            .project(&TurnEvent::ToolResultPresented {
                step: 1,
                call_id: "call-question".to_owned(),
                presentation: ToolResultPresentation::Question(QuestionResultPresentation::new(
                    QuestionResultStatus::Cancelled,
                    None,
                    1,
                    5,
                ),),
            })
            .is_empty()
    );
    let completed = projector.project(&TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: "call-question".to_owned(),
        display_name: "Question".to_owned(),
        name: "question".to_owned(),
        title: "Cancelled · 1 question · <1s".to_owned(),
        output: "The user cancelled this question request.".to_owned(),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    });
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["status"], "completed");
    assert!(
        completed[0]["_meta"]["zuno"]["question"]
            .get("continuationPending")
            .is_none()
    );
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
fn cancelled_tools_use_stable_acp_status_with_typed_zuno_metadata() {
    let mut projector = TurnEventProjector::new();
    let cancelled = projector
        .project(&TurnEvent::ToolDispatchInterrupted {
            step: 1,
            call_id: "call-cancelled".to_owned(),
            display_name: "Delegate".to_owned(),
            name: "task".to_owned(),
            title: "Inspect repository".to_owned(),
            output: "child supervisor settled".to_owned(),
            interruption: ToolInterruption::Cooperative,
        })
        .expect("cancelled tool is client-visible");

    assert_eq!(cancelled["status"], "failed");
    assert_eq!(cancelled["_meta"]["zuno"]["outcome"], "cancelled");
    assert_eq!(
        cancelled["_meta"]["zuno"]["interruptionMode"],
        "cooperative"
    );
    assert_eq!(cancelled["_meta"]["zuno"]["uncertain"], false);
    assert_eq!(
        cancelled["content"][0]["content"]["text"],
        "child supervisor settled"
    );
}

#[test]
fn turn_interruption_projects_typed_provenance_without_a_failure_card() {
    let update = turn_event_update(&TurnEvent::TurnInterrupted {
        assistant_message_id: Some("msg-1".to_owned()),
        steps: 1,
        request: Some(zuno_engine::interrupt::HardInterruptRequest::new(
            zuno_engine::interrupt::HardInterruptSource::Acp,
            zuno_engine::interrupt::HardInterruptReason::UserCancel,
        )),
    })
    .expect("turn interruption is client-visible");

    assert_eq!(update["sessionUpdate"], "agent_message_chunk");
    assert_eq!(update["_meta"]["zuno"]["kind"], "turn_interrupted");
    assert_eq!(update["_meta"]["zuno"]["source"], "acp");
    assert_eq!(update["_meta"]["zuno"]["reason"], "user_cancel");
}

#[test]
fn completed_tools_project_native_file_diffs_locations_and_json_output() {
    let mut projector = TurnEventProjector::new();
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let source = workspace.path().join("src");
    let changed = source.join("lib.rs");
    let created = source.join("new.rs");
    let deleted = source.join("deleted.rs");
    let changed_path = zuno_paths::wire_path(&changed);
    let created_path = zuno_paths::wire_path(&created);
    let deleted_path = zuno_paths::wire_path(&deleted);
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
                    FileDiff::new(&changed, Some("old\n".to_owned()), "new\n".to_owned())
                        .expect("changed absolute file"),
                    FileDiff::new(&created, None, "created\n".to_owned())
                        .expect("created absolute file"),
                    FileDiff::new(&deleted, Some("removed\n".to_owned()), String::new())
                        .expect("deleted absolute file"),
                ],
            ),
            written_paths: vec![
                changed_path.clone(),
                created_path.clone(),
                changed_path.clone(),
            ],
            is_error: false,
        })
        .expect("completed dispatches are client-visible");
    assert_eq!(completed["rawOutput"], json!({ "ok": true, "changed": 2 }));
    assert_eq!(
        completed["locations"],
        json!([
            { "path": changed_path },
            { "path": created_path },
            { "path": deleted_path },
        ])
    );
    assert_eq!(
        completed["content"][1],
        json!({
            "type": "diff",
            "path": changed_path,
            "oldText": "old\n",
            "newText": "new\n",
        })
    );
    assert_eq!(
        completed["content"][2],
        json!({
            "type": "diff",
            "path": created_path,
            "oldText": null,
            "newText": "created\n",
        })
    );
    assert_eq!(completed["content"][3]["path"], deleted_path);
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

#[test]
fn attempt_buffering_discards_failed_partial_output_before_acp_commit() {
    let mut projector = AttemptBufferedTurnEventProjector::with_context_size(200_000);
    assert!(
        projector
            .project(&TurnEvent::ProviderRequestStarted {
                step: 1,
                message_count: 1,
                estimated_prompt_tokens: 12,
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::TextDelta("discarded".to_owned()),
            })
            .is_empty(),
        "attempt output must remain provisional"
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::ToolUseStart {
                    id: "discarded-call".to_owned(),
                    name: "read".to_owned(),
                },
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::ToolCallStarted {
                step: 1,
                call_id: "discarded-call".to_owned(),
                display_name: "Read".to_owned(),
                name: "read".to_owned(),
                ui_intent: ToolUiIntent::Generic,
            })
            .is_empty(),
        "a failed attempt must not create a visible ACP tool row"
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::ReasoningDelta("kept thought".to_owned()),
            })
            .is_empty()
    );
    assert!(
        projector
            .project(&TurnEvent::Provider {
                step: 1,
                event: StreamEvent::TextDelta("kept answer".to_owned()),
            })
            .is_empty()
    );

    let committed = projector.project(&TurnEvent::AssistantCheckpointed {
        step: 1,
        message_id: "msg-1".to_owned(),
        interrupted: false,
    });
    assert_eq!(committed.len(), 2);
    assert_eq!(committed[0]["sessionUpdate"], "agent_thought_chunk");
    assert_eq!(committed[0]["content"]["text"], "kept thought");
    assert_eq!(committed[1]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(committed[1]["content"]["text"], "kept answer");
    assert!(
        committed
            .iter()
            .all(|update| update["content"]["text"] != "discarded")
    );
    assert!(
        committed
            .iter()
            .all(|update| update["toolCallId"] != "discarded-call")
    );
}
