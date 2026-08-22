use super::*;
use crate::views::dialog::DialogStep;
use crate::views::testkit::{action, press};
use crossterm::event::KeyCode;
use std::collections::BTreeMap;
use std::time::Duration;
use zuno_pty::BackgroundExecutionInput;

fn running() -> (
    tempfile::TempDir,
    Arc<BackgroundExecutionService>,
    BackgroundExecutionInfo,
) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(
        BackgroundExecutionService::open(directory.path().join("background"))
            .expect("background service"),
    );
    let info = service
        .start(BackgroundExecutionInput {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "printf ready; sleep 30".into()],
            cwd: directory.path().to_owned(),
            environment: std::env::vars_os().collect::<BTreeMap<_, _>>(),
            session_id: "session-a".to_owned(),
            title: "preview server".to_owned(),
            command: "printf ready; sleep 30".to_owned(),
            hard_ceiling: Duration::from_secs(60),
        })
        .expect("background command starts");
    (directory, service, info)
}

#[test]
fn empty_background_view_says_the_session_has_no_terminals() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service =
        Arc::new(BackgroundExecutionService::open(directory.path()).expect("background service"));
    let mut view = BackgroundView::new(ViewContext::defaults(), service, "session-a");
    let rendered = view
        .lines(80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains(EMPTY), "{rendered}");
}

#[tokio::test]
async fn x_twice_requests_one_stop_and_keeps_the_background_list_open() {
    let (_directory, service, info) = running();
    let mut view = BackgroundView::new(ViewContext::defaults(), Arc::clone(&service), "session-a");
    assert_eq!(
        view.handle_action(action("background_cancel"), &press(KeyCode::Null)),
        DialogStep::Redraw
    );
    assert!(
        view.lines(100)
            .iter()
            .any(|line| line.to_string().contains("press x again"))
    );
    assert_eq!(
        view.handle_action(action("background_cancel"), &press(KeyCode::Null)),
        DialogStep::Emitted(DialogOutcome::BackgroundCancel {
            execution_id: info.id.to_string(),
        })
    );
    service.cancel(&info.id).expect("cleanup cancellation");
    let _ = service
        .wait(&info.id, Some(Duration::from_secs(2)))
        .await
        .expect("cleanup wait");
}
