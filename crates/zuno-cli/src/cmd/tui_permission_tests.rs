//! What the turn driver and the render loop must be able to trust about each other.

use super::*;

use serde_json::{Value, json};
use std::time::Duration;
use zuno_engine::dispatch::ToolRegistryDispatcher;
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{DispatchRequest, ToolCall, ToolDispatcher};
use zuno_llm::cache::McpToolStatus;
use zuno_tool::{Tool, ToolContext, ToolOutput};
use zuno_tui::app::render_offscreen;
use zuno_tui::keybind::{KeyDispatcher, Keymap};
use zuno_tui::views::dialog::ObservedBase;
use zuno_tui::views::message::TranscriptView;
use zuno_tui::views::session::scopes;

fn broker() -> (Arc<PermissionBroker>, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = zuno_tui::app::terminal_event_channel();
    (Arc::new(PermissionBroker::new(sender)), receiver)
}

fn bridge(broker: &Arc<PermissionBroker>) -> PermissionBridge {
    let context = ViewContext::defaults();
    let host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context.clone()))),
    );
    PermissionBridge::new(context, Arc::clone(broker), host)
}

fn resize() -> AppEvent {
    AppEvent::Terminal(TerminalEvent::Resize {
        width: 80,
        height: 24,
    })
}

fn submit() -> &'static Definition {
    zuno_tui::keybind::definition("dialog.select.submit")
        .unwrap_or_else(|| panic!("`dialog.select.submit` is not in the binding table"))
}

fn press() -> zuno_tui::crossterm::event::KeyEvent {
    zuno_tui::crossterm::event::KeyEvent {
        code: zuno_tui::crossterm::event::KeyCode::Enter,
        modifiers: zuno_tui::crossterm::event::KeyModifiers::NONE,
        kind: zuno_tui::crossterm::event::KeyEventKind::Press,
        state: zuno_tui::crossterm::event::KeyEventState::NONE,
    }
}

fn key(
    code: zuno_tui::crossterm::event::KeyCode,
    modifiers: zuno_tui::crossterm::event::KeyModifiers,
) -> zuno_tui::crossterm::event::KeyEvent {
    zuno_tui::crossterm::event::KeyEvent {
        code,
        modifiers,
        kind: zuno_tui::crossterm::event::KeyEventKind::Press,
        state: zuno_tui::crossterm::event::KeyEventState::NONE,
    }
}

struct DispatchBash;

#[async_trait]
impl Tool for DispatchBash {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Exercise the production permission dispatch path."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("bash", args["command"].to_string()))
    }
}

struct DispatchEdit;

#[async_trait]
impl Tool for DispatchEdit {
    fn id(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Exercise edit through the production permission dispatch path."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filePath": { "type": "string" },
                "oldString": { "type": "string" },
                "newString": { "type": "string" }
            },
            "required": ["filePath", "oldString", "newString"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("edit", args["filePath"].to_string()))
    }
}

fn rendered_text(component: &mut impl Component, width: u16, height: u16) -> String {
    let rendered = render_offscreen(component, width, height).expect("infallible");
    (0..rendered.area.height)
        .map(|y| {
            (0..rendered.area.width)
                .map(|x| rendered[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn answer_through_production_keys(
    keys: Vec<zuno_tui::crossterm::event::KeyEvent>,
) -> (Arc<PermissionBroker>, Result<(), ToolError>) {
    let (broker, mut wake) = broker();
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask("bash", PermissionAsk::new("bash", "ls")).await })
    };
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), wake.recv()).await,
        Ok(Some(TerminalEvent::Wake))
    ));

    let mut bridge = bridge(&broker);
    bridge.handle_event(&resize());
    let mut dispatcher = KeyDispatcher::new(
        Keymap::defaults().expect("the shipped keymap builds"),
        scopes(),
        Box::new(bridge),
    );
    for key in keys {
        let result = dispatcher.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            zuno_tui::crossterm::event::Event::Key(key),
        )));
        assert!(
            result.redraw,
            "a permission prompt key was consumed without updating the dialog"
        );
    }
    let answer = tokio::time::timeout(Duration::from_millis(250), asking)
        .await
        .expect("the permission key sequence must unblock the waiting turn")
        .expect("the asking task");
    (broker, answer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ask_becomes_a_prompt_and_the_decision_answers_it() {
    let (broker, mut wake) = broker();
    broker.bind_session("ses_bridge");
    let mut ask = PermissionAsk::new("bash", "git status");
    ask.metadata.insert(
        String::from("arguments"),
        serde_json::json!({"command": "git status"}),
    );
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask("bash", ask).await })
    };

    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), wake.recv()).await,
            Ok(Some(TerminalEvent::Wake))
        ),
        "the broker did not nudge the render loop, so an idle TUI would never ask"
    );

    let mut bridge = bridge(&broker);
    let opened = bridge.handle_event(&resize());
    assert!(
        opened.redraw,
        "the bridge did not open a prompt for the parked request"
    );
    let rendered = render_offscreen(&mut bridge, 60, 12).expect("infallible");
    let joined = (0..rendered.area.height)
        .map(|y| {
            (0..rendered.area.width)
                .map(|x| rendered[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("Permission required"),
        "the permission prompt is not on screen:\n{joined}"
    );
    assert!(
        joined.contains("$ git status"),
        "the decoded command in `metadata.arguments` is not shown:\n{joined}"
    );

    // `Allow once` is the highlighted option, so submitting resolves to it.
    bridge.handle_action(submit(), &press());
    let answer = tokio::time::timeout(Duration::from_secs(5), asking)
        .await
        .expect("the ask must be answered once the user decides")
        .expect("the asking task");
    assert!(
        answer.is_ok(),
        "an `Allow once` decision did not authorize the call: {answer:?}"
    );

    let mut webfetch = PermissionAsk::new("webfetch", "https://example.com/docs");
    webfetch.metadata.insert(
        String::from("arguments"),
        serde_json::json!({"url": "https://example.com/docs"}),
    );
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask("webfetch", webfetch).await })
    };
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), wake.recv()).await,
        Ok(Some(TerminalEvent::Wake))
    ));
    bridge.handle_event(&resize());
    let rendered = render_offscreen(&mut bridge, 70, 12).expect("infallible");
    let joined = (0..rendered.area.height)
        .map(|y| {
            (0..rendered.area.width)
                .map(|x| rendered[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("WebFetch https://example.com/docs"),
        "the decoded URL in `metadata.arguments` is not shown:\n{joined}"
    );
    bridge.handle_action(submit(), &press());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), asking)
            .await
            .expect("the URL ask must be answered")
            .expect("the asking task")
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_dispatch_arguments_reach_the_rendered_permission_dialog() {
    let (broker, mut wake) = broker();
    broker.bind_session("ses_dispatch_bridge");
    let dispatcher = Arc::new(ToolRegistryDispatcher::new(
        vec![Arc::new(DispatchBash)],
        Vec::new(),
        Arc::clone(&broker) as Arc<dyn PermissionAsker>,
        InterruptSignal::new(),
        McpToolStatus::Ready,
    ));
    let available_tools = dispatcher.available_tools().definitions.into();
    let mut dispatching = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move {
            dispatcher
                .dispatch(DispatchRequest {
                    call: ToolCall {
                        id: "call_dispatch_bridge".to_owned(),
                        name: "bash".to_owned(),
                        input: json!({
                            "command": "printf seam-20",
                            "intent": "prove permission argument plumbing"
                        }),
                        raw_input: r#"{"command":"printf seam-20","intent":"prove permission argument plumbing"}"#.to_owned(),
                        input_error: None,
                        thought_signature: None,
                    },
                    session_id: "ses_dispatch_bridge".to_owned(),
                    message_id: "msg_dispatch_bridge".to_owned(),
                    agent: "build".to_owned(),
                    available_tools,
                    interrupt: InterruptSignal::new(),
                })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            event = wake.recv() => assert!(matches!(event, Some(TerminalEvent::Wake))),
            result = &mut dispatching => {
                let result = result.expect("the dispatch task");
                panic!("dispatch completed before asking permission: {}", result.output.output);
            }
        }
    })
    .await
    .expect("production dispatch never reached the permission broker");

    let mut bridge = bridge(&broker);
    bridge.handle_event(&resize());
    let rendered = render_offscreen(&mut bridge, 70, 12).expect("infallible");
    let joined = (0..rendered.area.height)
        .map(|y| {
            (0..rendered.area.width)
                .map(|x| rendered[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("$ printf seam-20"),
        "the production dispatcher did not carry its bash arguments to the dialog:\n{joined}"
    );

    bridge.handle_action(submit(), &press());
    let result = tokio::time::timeout(Duration::from_secs(5), dispatching)
        .await
        .expect("the rendered permission must be answerable")
        .expect("the dispatch task");
    assert!(!result.is_error, "{}", result.output.output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_edit_dispatch_renders_path_and_diff_in_collapsed_and_fullscreen() {
    let (broker, mut wake) = broker();
    broker.bind_session("ses_edit_dispatch_bridge");
    let dispatcher = Arc::new(ToolRegistryDispatcher::new(
        vec![Arc::new(DispatchEdit)],
        Vec::new(),
        Arc::clone(&broker) as Arc<dyn PermissionAsker>,
        InterruptSignal::new(),
        McpToolStatus::Ready,
    ));
    let available_tools = dispatcher.available_tools().definitions.into();
    let mut dispatching = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move {
            dispatcher
                .dispatch(DispatchRequest {
                    call: ToolCall {
                        id: "call_edit_dispatch_bridge".to_owned(),
                        name: "edit".to_owned(),
                        input: json!({
                            "filePath": "src/production.rs",
                            "oldString": "PRODUCTION_BEFORE",
                            "newString": "PRODUCTION_AFTER",
                            "intent": "prove edit permission rendering"
                        }),
                        raw_input: r#"{"filePath":"src/production.rs","oldString":"PRODUCTION_BEFORE","newString":"PRODUCTION_AFTER","intent":"prove edit permission rendering"}"#.to_owned(),
                        input_error: None,
                        thought_signature: None,
                    },
                    session_id: "ses_edit_dispatch_bridge".to_owned(),
                    message_id: "msg_edit_dispatch_bridge".to_owned(),
                    agent: "build".to_owned(),
                    available_tools,
                    interrupt: InterruptSignal::new(),
                })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            event = wake.recv() => assert!(matches!(event, Some(TerminalEvent::Wake))),
            result = &mut dispatching => {
                let result = result.expect("the edit dispatch task");
                panic!("edit dispatch completed before asking permission: {}", result.output.output);
            }
        }
    })
    .await
    .expect("production edit dispatch never reached the permission broker");

    let mut bridge = bridge(&broker);
    bridge.handle_event(&resize());
    let collapsed = rendered_text(&mut bridge, 80, 20);
    assert!(
        collapsed.contains("Edit src/production.rs"),
        "production edit arguments did not reach the subject:\n{collapsed}"
    );
    assert!(
        collapsed.contains("PRODUCTION_BEFORE") && collapsed.contains("PRODUCTION_AFTER"),
        "production edit arguments did not become a visible diff:\n{collapsed}"
    );

    bridge.handle_action(
        zuno_tui::keybind::definition("permission.prompt.fullscreen")
            .expect("the fullscreen action exists"),
        &key(
            zuno_tui::crossterm::event::KeyCode::Char('f'),
            zuno_tui::crossterm::event::KeyModifiers::CONTROL,
        ),
    );
    let fullscreen = rendered_text(&mut bridge, 80, 30);
    assert!(
        fullscreen.contains("Edit src/production.rs"),
        "fullscreen dropped the production edit subject:\n{fullscreen}"
    );
    assert!(
        fullscreen.contains("PRODUCTION_BEFORE") && fullscreen.contains("PRODUCTION_AFTER"),
        "fullscreen dropped the production edit diff:\n{fullscreen}"
    );

    bridge.handle_action(submit(), &press());
    let result = tokio::time::timeout(Duration::from_secs(5), dispatching)
        .await
        .expect("the rendered edit permission must be answerable")
        .expect("the edit dispatch task");
    assert!(!result.is_error, "{}", result.output.output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_keys_answer_every_permission_choice() {
    use zuno_tui::crossterm::event::{KeyCode, KeyModifiers};

    let (once_broker, once) = answer_through_production_keys(vec![
        key(KeyCode::Char('f'), KeyModifiers::CONTROL),
        key(KeyCode::Enter, KeyModifiers::NONE),
    ])
    .await;
    assert!(
        once.is_ok(),
        "Ctrl+F then Enter did not allow once: {once:?}"
    );
    assert!(once_broker.next_request().is_none());

    let (always_broker, always) = answer_through_production_keys(vec![
        key(KeyCode::Down, KeyModifiers::NONE),
        key(KeyCode::Enter, KeyModifiers::NONE),
        key(KeyCode::Enter, KeyModifiers::NONE),
    ])
    .await;
    assert!(
        always.is_ok(),
        "Down and Enter did not allow always: {always:?}"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(250),
            always_broker.ask("bash", PermissionAsk::new("bash", "ls")),
        )
        .await
        .expect("the always grant must answer a matching ask immediately")
        .is_ok()
    );

    let (_reject_broker, reject) =
        answer_through_production_keys(vec![key(KeyCode::Esc, KeyModifiers::NONE)]).await;
    assert!(
        matches!(reject, Err(ToolError::Denied { .. })),
        "Escape did not reject the permission request: {reject:?}"
    );

    let (_reject_broker, reject) = answer_through_production_keys(vec![
        key(KeyCode::Down, KeyModifiers::NONE),
        key(KeyCode::Down, KeyModifiers::NONE),
        key(KeyCode::Enter, KeyModifiers::NONE),
    ])
    .await;
    assert!(
        matches!(reject, Err(ToolError::Denied { .. })),
        "selecting Reject did not deny the permission request: {reject:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tui_that_goes_away_denies_an_outstanding_ask() {
    // Fail closed. A tool call whose authorization can no longer be obtained must
    // not run, and the only way to say so is the error the dispatcher already
    // understands.
    let (broker, _wake) = broker();
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask("bash", PermissionAsk::new("bash", "rm -rf /"))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Dropping every parked answer is what the render loop's exit does to them.
    locked(&broker.parked).pending.clear();

    let answer = tokio::time::timeout(Duration::from_secs(5), asking)
        .await
        .expect("a dropped answer must not hang the turn")
        .expect("the asking task");
    assert!(
        matches!(answer, Err(ToolError::Denied { .. })),
        "an unanswerable ask was not denied: {answer:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_answers_the_next_matching_ask_without_prompting() {
    let (broker, _wake) = broker();
    let mut bridge = bridge(&broker);
    let first = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.ask("bash", PermissionAsk::new("bash", "ls")).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    bridge.handle_event(&resize());
    // `Allow always` is one step down, and it escalates before it resolves.
    bridge.handle_action(
        zuno_tui::keybind::definition("dialog.select.next").expect("the action exists"),
        &press(),
    );
    bridge.handle_action(submit(), &press());
    bridge.handle_action(submit(), &press());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("the first ask resolves")
            .expect("the asking task")
            .is_ok(),
        "`Allow always` did not authorize the call it was answering"
    );

    let repeated = tokio::time::timeout(
        Duration::from_secs(5),
        broker.ask("bash", PermissionAsk::new("bash", "ls")),
    )
    .await
    .expect("a standing grant must answer without a prompt");
    assert!(repeated.is_ok(), "the standing grant was not honoured");
    assert_eq!(
        broker.next_request(),
        None,
        "a standing grant still parked a request, so the user would be asked twice"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_approval_never_parks_anything() {
    assert!(
        AutoApproval
            .ask("bash", PermissionAsk::new("bash", "anything"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_wake_reaches_the_component_below_the_bridge() {
    // The regression this guards: the bridge used to absorb `Wake` on the grounds that it
    // "carries no state of its own". That was true while the permission broker was the
    // only producer of one. The language-server probe is a second producer — it queues a
    // report and then nudges — and a wake stopped here meant the report was never drained.
    // Since a completed turn is the last event the loop sees, "the next event will pick it
    // up" is not a fallback but never.
    //
    // The assertion is the real path: bridge -> dialog host -> screen -> drain ->
    // transcript. The host is in the chain rather than skipped because it is the other
    // component that has absorbed an event it did not recognise.
    let (broker, _wake) = broker();
    let context = ViewContext::defaults();
    let (shutdown, _held) = tokio::sync::mpsc::channel(4);
    let (reports, report_receiver) = tokio::sync::mpsc::channel(4);
    let screen = zuno_tui::views::session::SessionScreen::new(context.clone(), shutdown)
        .with_diagnostics_source(report_receiver);
    let host = zuno_tui::views::dialog::DialogHost::new(context.clone(), Box::new(screen));
    let mut bridge = PermissionBridge::new(context, Arc::clone(&broker), host);

    reports
        .try_send(zuno_tui::views::lsp::Report::checked(
            "src/lib.rs",
            "rust",
            vec![zuno_tui::views::lsp::Diagnostic {
                severity: zuno_tui::views::lsp::Severity::Error,
                line: 2,
                column: 9,
                source: Some(String::from("rustc")),
                message: String::from("mismatched types"),
            }],
        ))
        .expect("the inlet accepts a report");

    let result = bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    assert!(
        result.redraw,
        "a wake carrying a queued report produced no frame"
    );
    let buffer = zuno_tui::app::render_offscreen(&mut bridge, 120, 24).expect("a frame");
    let joined = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("mismatched types"),
        "the report never reached the transcript, so the wake was absorbed:\n{joined}"
    );
}

/// The spinner and the prompt are mutually exclusive, end to end through the real host.
///
/// `SessionScreen` sits under `DialogHost` and cannot see the stack, so this asserts the
/// whole path: a parked ask opens a prompt, the host reports the active modal down to the
/// screen while it draws, and the transcript drops `working`. Asserting
/// `Transcript::set_awaiting_permission` directly would pass with the notification
/// unwired, which is the state that shipped.
///
/// A turn message is streamed first on purpose. With an empty transcript the welcome
/// surface owns that area and the prompt overlays the status strip, so a frame assertion
/// would be checking rows the user never sees — the strip's own wording is asserted in
/// `zuno-tui`'s unit tests instead.
#[tokio::test]
async fn cmd_tui_permission_prompt_replaces_the_working_spinner() {
    let (broker, mut wake) = broker();
    let context = ViewContext::defaults();
    let (shutdown, _held) = tokio::sync::mpsc::channel(4);
    let screen = zuno_tui::views::session::SessionScreen::new(context.clone(), shutdown);
    let host = zuno_tui::views::dialog::DialogHost::new(context.clone(), Box::new(screen));
    let mut bridge = PermissionBridge::new(context, Arc::clone(&broker), host);

    for event in [
        zuno_engine::r#loop::TurnEvent::TurnStarted {
            session_id: String::from("s"),
        },
        zuno_engine::r#loop::TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: String::from("m"),
        },
        zuno_engine::r#loop::TurnEvent::Provider {
            step: 1,
            event: zuno_llm::event::StreamEvent::TextDelta(String::from("on it")),
        },
    ] {
        bridge.handle_event(&AppEvent::Engine(event));
    }
    let busy = frame(&mut bridge);
    assert!(
        busy.contains("working"),
        "a running turn with nothing outstanding still spins:\n{busy}"
    );

    let asked = tokio::spawn({
        let broker = Arc::clone(&broker);
        async move {
            broker
                .ask(
                    "bash",
                    PermissionAsk {
                        permission: String::from("bash"),
                        patterns: vec![String::from("rm -rf /")],
                        metadata: serde_json::Map::new(),
                        always: vec![String::from("*")],
                    },
                )
                .await
        }
    });
    // The broker nudges the loop once the request is parked, which is the same
    // synchronisation the production loop uses — waiting on it rather than on a clock
    // means this test cannot pass by being slow.
    let nudge = tokio::time::timeout(Duration::from_secs(5), wake.recv())
        .await
        .expect("the broker nudges the loop when it parks a request");
    assert!(matches!(nudge, Some(TerminalEvent::Wake)));

    bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    let waiting = frame(&mut bridge);
    assert!(
        waiting.contains("Permission required"),
        "the prompt never opened, so this proves nothing about the spinner:\n{waiting}"
    );
    assert!(
        !waiting.contains("working"),
        "the transcript still claimed to be working while asking the user to decide:\n{waiting}"
    );
    assert!(
        waiting.contains("waiting for your approval"),
        "nothing said who the turn is blocked on:\n{waiting}"
    );

    asked.abort();
}

fn frame(bridge: &mut PermissionBridge) -> String {
    let buffer = render_offscreen(bridge, 120, 30).expect("the offscreen backend is infallible");
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
