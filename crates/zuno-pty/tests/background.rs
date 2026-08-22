#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use zuno_pty::{
    BUFFER_LIMIT, BackgroundExecutionId, BackgroundExecutionInput, BackgroundExecutionService,
    BackgroundExecutionStatus, ReplayCursor,
};

fn input(
    directory: &Path,
    command: impl Into<String>,
    hard_ceiling: Duration,
) -> BackgroundExecutionInput {
    let command = command.into();
    BackgroundExecutionInput {
        program: OsString::from("/bin/sh"),
        arguments: vec![OsString::from("-c"), OsString::from(command.clone())],
        cwd: directory.to_owned(),
        environment: std::env::vars_os().collect::<BTreeMap<_, _>>(),
        session_id: "ses_background".to_owned(),
        title: command.clone(),
        command,
        hard_ceiling,
    }
}

#[tokio::test]
async fn one_service_owns_start_wait_and_bounded_output() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let info = service
        .start(input(
            directory.path(),
            "printf first; sleep 0.03; printf second",
            Duration::from_secs(2),
        ))
        .expect("command starts");

    let settled = service
        .wait(&info.id, None)
        .await
        .expect("wait succeeds")
        .info;
    assert_eq!(settled.status, BackgroundExecutionStatus::Completed);
    assert_eq!(settled.exit_code, Some(0));
    let output = service
        .output(&info.id, ReplayCursor::Full)
        .expect("output replay");
    assert_eq!(output.bytes, b"firstsecond");
    assert_eq!(
        service.complete_output(&info.id).expect("complete output"),
        b"firstsecond"
    );
}

#[tokio::test]
async fn cancellation_terminates_the_complete_process_group() {
    let directory = tempfile::tempdir().expect("workspace");
    let parent_file = directory.path().join("parent.pid");
    let child_file = directory.path().join("child.pid");
    let command = format!(
        "printf '%s' \"$$\" > '{}'; sleep 30 & printf '%s' \"$!\" > '{}'; wait",
        parent_file.display(),
        child_file.display()
    );
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let info = service
        .start(input(directory.path(), command, Duration::from_secs(30)))
        .expect("command starts");
    wait_for_file(&parent_file).await;
    wait_for_file(&child_file).await;
    let parent = read_pid(&parent_file);
    let child = read_pid(&child_file);

    assert!(service.cancel(&info.id).expect("cancel request"));
    assert!(!service.cancel(&info.id).expect("duplicate cancel request"));
    let settled = service
        .wait(&info.id, None)
        .await
        .expect("cancel settles")
        .info;

    assert_eq!(settled.status, BackgroundExecutionStatus::Cancelled);
    wait_for_process_exit(parent).await;
    wait_for_process_exit(child).await;
}

#[tokio::test]
async fn hard_ceiling_is_terminal_and_never_replays_the_command() {
    let directory = tempfile::tempdir().expect("workspace");
    let marker = directory.path().join("started");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let info = service
        .start(input(
            directory.path(),
            format!("touch '{}'; sleep 30", marker.display()),
            Duration::from_millis(50),
        ))
        .expect("command starts");

    let settled = service
        .wait(&info.id, None)
        .await
        .expect("hard ceiling settles")
        .info;

    assert!(marker.exists());
    assert_eq!(settled.status, BackgroundExecutionStatus::Failed);
    assert!(settled.timed_out);
    assert_eq!(
        service.list().len(),
        1,
        "the timed-out command must not be started again"
    );
}

#[tokio::test]
async fn live_output_is_bounded_while_the_complete_file_is_retained() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let total = BUFFER_LIMIT + 4_096;
    let info = service
        .start(input(
            directory.path(),
            format!("head -c {total} /dev/zero | tr '\\0' x"),
            Duration::from_secs(5),
        ))
        .expect("command starts");
    service
        .wait(&info.id, None)
        .await
        .expect("large command settles");

    let output = service
        .output(&info.id, ReplayCursor::Full)
        .expect("bounded output");
    assert!(output.bytes.len() <= BUFFER_LIMIT);
    assert_eq!(output.total_written, total as u64);
    assert_eq!(output.discarded, 4_096);
    assert_eq!(
        std::fs::metadata(output.output_file)
            .expect("complete output file")
            .len(),
        total as u64
    );
}

#[test]
fn persisted_running_state_reconciles_to_uncertain_without_replay() {
    let directory = tempfile::tempdir().expect("workspace");
    let id =
        BackgroundExecutionId::parse("bg_0123456789abcdef0123456789abcdef").expect("fixture id");
    let output_file = directory.path().join(format!("{id}.output"));
    let status_file = directory.path().join(format!("{id}.status.json"));
    std::fs::write(&output_file, b"partial").expect("fixture output");
    std::fs::write(
        &status_file,
        serde_json::to_vec_pretty(&serde_json::json!({
            "format": 1,
            "info": {
                "id": id.as_str(),
                "sessionId": "ses_background",
                "title": "fixture",
                "command": "fixture",
                "cwd": directory.path(),
                "status": "running",
                "pid": 999999,
                "exitCode": null,
                "timedOut": false,
                "timeCreated": 1,
                "timeUpdated": 1,
                "timeCompleted": null,
                "error": null,
                "outputFile": "/must/not/be/trusted",
                "statusFile": "/must/not/be/trusted"
            }
        }))
        .expect("fixture JSON"),
    )
    .expect("fixture status");

    let service = BackgroundExecutionService::open(directory.path()).expect("reconciled service");
    let info = service.get(&id).expect("recovered execution");

    assert_eq!(info.status, BackgroundExecutionStatus::Uncertain);
    assert_eq!(info.output_file, output_file);
    assert_eq!(info.status_file, status_file);
    assert!(
        info.error
            .as_deref()
            .is_some_and(|value| value.contains("not replayed"))
    );
    assert_eq!(
        service
            .output(&id, ReplayCursor::Full)
            .expect("recovered output")
            .bytes,
        b"partial"
    );
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !path.exists() || std::fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} was not populated", path.display()));
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("pid file")
        .parse()
        .expect("numeric pid")
}

async fn wait_for_process_exit(pid: u32) {
    let process = std::path::PathBuf::from(format!("/proc/{pid}"));
    tokio::time::timeout(Duration::from_secs(2), async {
        while process.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} survived cancellation"));
}
