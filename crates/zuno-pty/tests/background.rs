#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use zuno_pty::{
    BUFFER_LIMIT, BackgroundExecutionId, BackgroundExecutionInput, BackgroundExecutionPurpose,
    BackgroundExecutionRetention, BackgroundExecutionService, BackgroundExecutionStatus,
    MAX_RETAINED_TERMINAL_EXECUTIONS, ReplayCursor,
};
use zuno_sandbox::{
    NetworkAccess, PrepareRequest, PreparedCommand, SandboxCapabilities, SandboxMode, SandboxPolicy,
};

fn prepared(directory: &Path, command: &str) -> PreparedCommand {
    let arguments = vec![OsString::from("-c"), OsString::from(command)];
    let request = PrepareRequest {
        program: OsString::from("/bin/sh"),
        arguments: arguments.clone(),
        cwd: directory.to_owned(),
        environment: std::env::vars_os().collect::<BTreeMap<_, _>>(),
        policy: SandboxPolicy::new(
            directory,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
        )
        .expect("test policy"),
    };
    PreparedCommand::from_backend(
        request,
        OsString::from("/bin/sh"),
        arguments,
        &SandboxCapabilities {
            backend: "test_direct".to_owned(),
            executable: Some(Path::new("/bin/sh").to_owned()),
            read_only: true,
            workspace_write: true,
            danger_full_access: false,
            network_isolation: true,
        },
        vec![directory.to_owned()],
        Vec::new(),
    )
}

fn input(
    directory: &Path,
    command: impl Into<String>,
    hard_ceiling: Duration,
) -> BackgroundExecutionInput {
    let command = command.into();
    BackgroundExecutionInput {
        prepared: prepared(directory, &command),
        session_id: "ses_background".to_owned(),
        title: command.clone(),
        command,
        purpose: BackgroundExecutionPurpose::Command,
        hard_ceiling,
        retention: BackgroundExecutionRetention::Durable,
    }
}

#[tokio::test]
async fn dropping_a_foreground_lease_cancels_an_already_spawned_process_tree() {
    let directory = tempfile::tempdir().expect("workspace");
    let pid_file = directory.path().join("leased.pid");
    let command = format!("printf '%s' \"$$\" > '{}'; sleep 30", pid_file.display());
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let (info, lease) = service
        .start_leased(input(directory.path(), command, Duration::from_secs(30)))
        .expect("command starts");
    wait_for_file(&pid_file).await;
    let pid = read_pid(&pid_file);

    drop(lease);
    let settled = service
        .wait(&info.id, None)
        .await
        .expect("lease cancellation settles")
        .info;

    assert_eq!(settled.status, BackgroundExecutionStatus::Cancelled);
    wait_for_process_exit(pid).await;
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
    assert_eq!(settled.authority.schema_version, 3);
    assert_eq!(
        settled.authority.requested_mode(),
        SandboxMode::WorkspaceWrite
    );
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
    assert_eq!(settled.authority.schema_version, 3);
    assert_eq!(
        settled.authority.requested_network(),
        NetworkAccess::Allowed
    );
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

#[tokio::test]
async fn terminal_retention_removes_the_oldest_state_and_both_files() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let mut oldest = None;

    for index in 0..=MAX_RETAINED_TERMINAL_EXECUTIONS {
        let info = service
            .start(input(
                directory.path(),
                format!("printf task-{index}"),
                Duration::from_secs(2),
            ))
            .expect("command starts");
        if oldest.is_none() {
            oldest = Some(info.clone());
        }
        service.wait(&info.id, None).await.expect("command settles");
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        while service.list().len() > MAX_RETAINED_TERMINAL_EXECUTIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retention pruning");

    let oldest = oldest.expect("oldest command");
    assert_eq!(service.list().len(), MAX_RETAINED_TERMINAL_EXECUTIONS);
    assert!(service.get(&oldest.id).is_err());
    assert!(!oldest.output_file.exists());
    assert!(!oldest.status_file.exists());
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("background directory")
            .count(),
        MAX_RETAINED_TERMINAL_EXECUTIONS * 2
    );
}

#[tokio::test]
async fn remote_observer_purpose_survives_terminal_persistence_and_reopen() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let mut observer = input(directory.path(), "printf observed", Duration::from_secs(2));
    observer.purpose = BackgroundExecutionPurpose::RemoteObserver;
    let started = service.start(observer).expect("observer starts");
    let settled = service
        .wait(&started.id, None)
        .await
        .expect("observer settles")
        .info;
    assert_eq!(settled.purpose, BackgroundExecutionPurpose::RemoteObserver);
    drop(service);

    let reopened =
        BackgroundExecutionService::open(directory.path()).expect("background service reopens");
    let restored = reopened.get(&started.id).expect("observer is restored");
    assert_eq!(restored.purpose, BackgroundExecutionPurpose::RemoteObserver);
    assert!(restored.purpose.requires_authoritative_refresh());
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
            "format": 3,
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
                "statusFile": "/must/not/be/trusted",
                "authority": prepared(directory.path(), "fixture").authority()
            }
        }))
        .expect("fixture JSON"),
    )
    .expect("fixture status");

    let service = BackgroundExecutionService::open(directory.path()).expect("reconciled service");
    let info = service.get(&id).expect("recovered execution");

    assert_eq!(info.status, BackgroundExecutionStatus::Uncertain);
    assert_eq!(
        info.purpose,
        BackgroundExecutionPurpose::Command,
        "older format-3 rows without purpose must retain ordinary command semantics"
    );
    assert_eq!(info.authority.schema_version, 3);
    assert_eq!(info.authority.requested_mode(), SandboxMode::WorkspaceWrite);
    assert_eq!(info.output_file, output_file);
    assert_eq!(info.status_file, status_file);
    assert!(
        info.error
            .as_deref()
            .is_some_and(|value| value.contains("not replayed"))
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(&status_file).expect("rewritten status")
        )
        .expect("rewritten JSON")["format"],
        3
    );
    assert_eq!(
        service
            .output(&id, ReplayCursor::Full)
            .expect("recovered output")
            .bytes,
        b"partial"
    );
}

#[test]
fn persisted_v2_authority_recovers_with_requested_equal_to_effective() {
    let directory = tempfile::tempdir().expect("workspace");
    let id =
        BackgroundExecutionId::parse("bg_1123456789abcdef0123456789abcdef").expect("fixture id");
    let output_file = directory.path().join(format!("{id}.output"));
    let status_file = directory.path().join(format!("{id}.status.json"));
    std::fs::write(&output_file, b"legacy").expect("fixture output");
    let prepared = prepared(directory.path(), "fixture");
    let mut authority = serde_json::to_value(prepared.authority()).expect("authority JSON");
    let object = authority.as_object_mut().expect("authority object");
    object.insert("schemaVersion".to_owned(), serde_json::json!(2));
    object.remove("requestedMode");
    object.remove("requestedNetwork");
    object.remove("resolutionKind");
    object.remove("fallbackReason");
    std::fs::write(
        &status_file,
        serde_json::to_vec_pretty(&serde_json::json!({
            "format": 3,
            "info": {
                "id": id.as_str(),
                "sessionId": "ses_background",
                "title": "fixture",
                "command": "fixture",
                "cwd": directory.path(),
                "status": "completed",
                "pid": null,
                "exitCode": 0,
                "timedOut": false,
                "timeCreated": 1,
                "timeUpdated": 1,
                "timeCompleted": 1,
                "error": null,
                "outputFile": "/must/not/be/trusted",
                "statusFile": "/must/not/be/trusted",
                "authority": authority
            }
        }))
        .expect("fixture JSON"),
    )
    .expect("fixture status");

    let service = BackgroundExecutionService::open(directory.path()).expect("reconciled service");
    let info = service.get(&id).expect("recovered execution");

    assert_eq!(info.authority.schema_version, 2);
    assert_eq!(info.authority.requested_mode(), info.authority.mode);
    assert_eq!(info.authority.requested_network(), info.authority.network);
    assert_eq!(
        info.authority.resolution_kind,
        zuno_sandbox::SandboxResolutionKind::Legacy
    );
    assert_eq!(info.purpose, BackgroundExecutionPurpose::Command);
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
