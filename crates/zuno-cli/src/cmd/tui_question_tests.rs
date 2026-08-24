use super::*;

use crate::cmd::tui_permission::{PermissionBridge, PermissionBroker};
use std::time::Duration;
use zuno_tools::question::{QuestionOption, QuestionOutcome, QuestionPrompt as ToolQuestionPrompt};
use zuno_tui::app::{AppEvent, Component, render_offscreen};
use zuno_tui::keybind::{ActionComponent, Definition};
use zuno_tui::views::dialog::ObservedBase;
use zuno_tui::views::message::TranscriptView;

fn broker() -> (Arc<QuestionBroker>, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = zuno_tui::app::terminal_event_channel();
    (Arc::new(QuestionBroker::new(sender)), receiver)
}

fn bridge(broker: &Arc<QuestionBroker>) -> PermissionBridge {
    let context = ViewContext::defaults();
    let host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context.clone()))),
    );
    let (permission_wake, _permission_events) = zuno_tui::app::terminal_event_channel();
    let permission = Arc::new(PermissionBroker::new(permission_wake));
    PermissionBridge::new(context.clone(), permission, host)
        .with_question(QuestionBridge::new(context, Arc::clone(broker)))
}

fn request(question: &str, header: &str) -> QuestionRequest {
    ToolQuestionPrompt::new(
        question,
        header,
        vec![
            QuestionOption::new("First", "the first choice"),
            QuestionOption::new("Second", "the second choice"),
        ],
    )
    .into_request()
}

fn resize() -> AppEvent {
    AppEvent::Terminal(TerminalEvent::Resize {
        width: 80,
        height: 24,
    })
}

fn action(name: &str) -> &'static Definition {
    zuno_tui::keybind::definition(name)
        .unwrap_or_else(|| panic!("`{name}` is not in the binding table"))
}

fn key(code: zuno_tui::crossterm::event::KeyCode) -> zuno_tui::crossterm::event::KeyEvent {
    zuno_tui::crossterm::event::KeyEvent {
        code,
        modifiers: zuno_tui::crossterm::event::KeyModifiers::NONE,
        kind: zuno_tui::crossterm::event::KeyEventKind::Press,
        state: zuno_tui::crossterm::event::KeyEventState::NONE,
    }
}

fn rendered_text(component: &mut impl Component) -> String {
    let rendered = render_offscreen(component, 80, 24).expect("infallible");
    (0..rendered.area.height)
        .map(|y| {
            (0..rendered.area.width)
                .map(|x| rendered[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_for_wake(wake: &mut mpsc::Receiver<TerminalEvent>) {
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), wake.recv()).await,
            Ok(Some(TerminalEvent::Wake))
        ),
        "the broker did not nudge the event loop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_queue_applies_backpressure_at_its_capacity() {
    let (broker, mut wake) = broker();
    let mut asking = Vec::new();
    for index in 0..QUESTION_CHANNEL_CAPACITY {
        let broker = Arc::clone(&broker);
        asking.push(tokio::spawn(async move {
            broker
                .ask(
                    "ses_capacity",
                    &[request(&format!("queued question {index}"), "Queue")],
                    None,
                )
                .await
        }));
        wait_for_wake(&mut wake).await;
    }

    let overflow = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask(
                    "ses_capacity",
                    &[request("overflow question", "Queue")],
                    None,
                )
                .await
        })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(50), wake.recv())
            .await
            .is_err(),
        "an over-capacity question entered the queue instead of waiting"
    );

    drop(broker.next_request().expect("the bounded queue was full"));
    wait_for_wake(&mut wake).await;

    overflow.abort();
    for task in asking {
        task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_round_trip_returns_multi_question_answers_in_order() {
    let (broker, mut wake) = broker();
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask(
                    "ses_question",
                    &[
                        request("Choose for the first question", "One"),
                        request("Choose for the second question", "Two"),
                    ],
                    Some(("msg_1", "call_1")),
                )
                .await
        })
    };
    wait_for_wake(&mut wake).await;

    let mut bridge = bridge(&broker);
    assert!(bridge.handle_event(&resize()).redraw);
    let frame = rendered_text(&mut bridge);
    assert!(
        frame.contains("Question 1/2 (2 unanswered) · One")
            && frame.contains("Choose for the first question"),
        "the parked request did not become the first dialog:\n{frame}"
    );

    bridge.handle_action(
        action("dialog.select.submit"),
        &key(zuno_tui::crossterm::event::KeyCode::Enter),
    );
    bridge.handle_action(
        action("dialog.select.next"),
        &key(zuno_tui::crossterm::event::KeyCode::Down),
    );
    bridge.handle_action(
        action("dialog.select.submit"),
        &key(zuno_tui::crossterm::event::KeyCode::Enter),
    );

    let answers = tokio::time::timeout(Duration::from_millis(250), asking)
        .await
        .expect("the completed dialog must unblock the question tool")
        .expect("the asking task")
        .expect("the question was answered");
    assert_eq!(
        answers,
        QuestionOutcome::Answered(vec![
            vec![String::from("First")],
            vec![String::from("Second")],
        ])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_escape_cancels_the_tool_and_never_hangs() {
    let (broker, mut wake) = broker();
    let asking = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .ask("ses_skip", &[request("Skip this question?", "Skip")], None)
                .await
        })
    };
    wait_for_wake(&mut wake).await;

    let mut bridge = bridge(&broker);
    bridge.handle_event(&resize());
    bridge.handle_action(
        action("session_interrupt"),
        &key(zuno_tui::crossterm::event::KeyCode::Esc),
    );

    let outcome = tokio::time::timeout(Duration::from_millis(250), asking)
        .await
        .expect("cancelling must not leave the turn parked forever")
        .expect("the asking task");
    assert_eq!(
        outcome.expect("the cancellation is an authoritative outcome"),
        QuestionOutcome::Cancelled,
        "escape returned a fabricated answer instead of cancelling"
    );
}
