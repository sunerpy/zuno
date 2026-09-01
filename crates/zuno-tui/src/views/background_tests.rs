use super::*;
use crate::views::dialog::DialogStep;
use crate::views::testkit::{action, press};
use crossterm::event::KeyCode;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use zuno_pty::{
    BackgroundExecutionInput, BackgroundExecutionPurpose, BackgroundExecutionRetention,
};
use zuno_sandbox::{
    NetworkAccess, PrepareRequest, PreparedCommand, SandboxCapabilities, SandboxMode, SandboxPolicy,
};

fn prepared(directory: &Path, command: &str) -> PreparedCommand {
    #[cfg(unix)]
    let (program, arguments) = (
        OsString::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from(command)],
    );
    #[cfg(windows)]
    let (program, arguments) = (
        std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe")),
        vec![
            OsString::from("/D"),
            OsString::from("/Q"),
            OsString::from("/C"),
            OsString::from(command),
        ],
    );
    let request = PrepareRequest {
        program: program.clone(),
        arguments: arguments.clone(),
        cwd: directory.to_owned(),
        environment: std::env::vars_os().collect::<BTreeMap<_, _>>(),
        policy: SandboxPolicy::new(
            directory,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
        )
        .expect("test sandbox policy"),
    };
    PreparedCommand::from_backend(
        request,
        program.clone(),
        arguments,
        &SandboxCapabilities {
            backend: "test_direct".to_owned(),
            executable: Some(program.into()),
            read_only: true,
            workspace_write: true,
            danger_full_access: false,
            network_isolation: true,
        },
        vec![directory.to_owned()],
        Vec::new(),
    )
}

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
    #[cfg(unix)]
    let command = "printf ready; sleep 30";
    #[cfg(windows)]
    let command = "echo ready & ping -n 31 127.0.0.1 >NUL";
    let info = service
        .start(BackgroundExecutionInput {
            prepared: prepared(directory.path(), command),
            session_id: "session-a".to_owned(),
            title: "preview server".to_owned(),
            command: command.to_owned(),
            purpose: BackgroundExecutionPurpose::Command,
            hard_ceiling: Duration::from_secs(60),
            retention: BackgroundExecutionRetention::Durable,
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
