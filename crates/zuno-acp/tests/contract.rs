use zuno_acp::{IMPLEMENTED_METHODS, turn_event_update};
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::StreamEvent;
use zuno_tool::ToolUiIntent;

#[test]
fn adapter_exposes_the_thirteen_methods_implemented_upstream() {
    assert_eq!(
        IMPLEMENTED_METHODS,
        [
            "initialize",
            "authenticate",
            "session/new",
            "session/load",
            "session/list",
            "session/resume",
            "session/close",
            "session/fork",
            "session/set_config_option",
            "session/set_mode",
            "session/set_model",
            "session/prompt",
            "session/cancel",
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
