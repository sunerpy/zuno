//! What the turn driver and the render loop must be able to trust about each other.

use super::*;

use oc_tui::app::render_offscreen;
use oc_tui::views::dialog::ObservedBase;
use oc_tui::views::message::TranscriptView;
use std::time::Duration;

fn broker() -> (Arc<PermissionBroker>, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = oc_tui::app::terminal_event_channel();
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
    oc_tui::keybind::definition("dialog.select.submit")
        .unwrap_or_else(|| panic!("`dialog.select.submit` is not in the binding table"))
}

fn press() -> oc_tui::crossterm::event::KeyEvent {
    oc_tui::crossterm::event::KeyEvent {
        code: oc_tui::crossterm::event::KeyCode::Enter,
        modifiers: oc_tui::crossterm::event::KeyModifiers::NONE,
        kind: oc_tui::crossterm::event::KeyEventKind::Press,
        state: oc_tui::crossterm::event::KeyEventState::NONE,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ask_becomes_a_prompt_and_the_decision_answers_it() {
    let (broker, mut wake) = broker();
    broker.bind_session("ses_bridge");
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask("bash", PermissionAsk::new("bash", "git status"))
                .await
        })
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
        oc_tui::keybind::definition("dialog.select.next").expect("the action exists"),
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
