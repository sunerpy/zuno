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
        .output(&info.id, ReplayCursor::Full, None)
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
            // This is a terminal-state and replay test, not a process-start
            // benchmark. Leave enough wall-clock headroom for a saturated
            // hosted runner to schedule the child before its ceiling.
            Duration::from_secs(3),
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
        .output(&info.id, ReplayCursor::Full, None)
        .expect("bounded output");
    assert!(output.bytes.len() <= BUFFER_LIMIT);
    assert_eq!(output.total_written, total as u64);
    assert_eq!(output.discarded, 4_096);
    assert!(
        !output.from_disk,
        "the retained tail is served from memory, not by re-reading the file"
    );
    assert_eq!(
        std::fs::metadata(output.output_file)
            .expect("complete output file")
            .len(),
        total as u64
    );
}

#[tokio::test]
async fn a_cursor_older_than_the_retained_ring_is_served_from_the_persisted_file() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let total = BUFFER_LIMIT + 4_096;
    let info = service
        .start(input(
            directory.path(),
            format!("printf 'first line\\n'; head -c {total} /dev/zero | tr '\\0' x"),
            Duration::from_secs(10),
        ))
        .expect("command starts");
    service
        .wait(&info.id, None)
        .await
        .expect("large command settles");

    let retained = service
        .output(&info.id, ReplayCursor::Full, None)
        .expect("retained tail");
    assert!(
        retained.discarded > 0,
        "the ring has to have dropped its prefix for this to mean anything"
    );
    assert!(
        !String::from_utf8_lossy(&retained.bytes).contains("first line"),
        "the opening line must be gone from memory"
    );

    // The discarded prefix is on disk in full. Clamping a cursor forward made it
    // unreachable through this service and left `tail` on a file the only way to see it.
    let recovered = service
        .output(&info.id, ReplayCursor::From(0), Some(11))
        .expect("prefix from the persisted file");
    assert_eq!(recovered.bytes, b"first line\n");
    assert_eq!(
        recovered.cursor, 11,
        "the next window starts where this one ended"
    );
    assert!(recovered.from_disk);
    assert_eq!(recovered.total_written, total as u64 + 11);
    assert!(
        recovered.retained_from > recovered.cursor,
        "the window is behind what the ring retains, which is why it came from the file"
    );

    let next = service
        .output(&info.id, ReplayCursor::From(recovered.cursor), Some(4))
        .expect("the window after the prefix");
    assert_eq!(next.bytes, b"xxxx");
    assert!(next.from_disk);
}

#[tokio::test]
async fn a_window_limit_bounds_one_replay_without_moving_the_cursor_space() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let info = service
        .start(input(
            directory.path(),
            "printf '0123456789'".to_owned(),
            Duration::from_secs(5),
        ))
        .expect("command starts");
    service.wait(&info.id, None).await.expect("settles");

    let first = service
        .output(&info.id, ReplayCursor::From(0), Some(4))
        .expect("first window");
    assert_eq!(first.bytes, b"0123");
    assert_eq!(first.cursor, 4);
    assert!(!first.from_disk, "these bytes are still retained");
    assert_eq!(first.total_written, 10);

    let second = service
        .output(&info.id, ReplayCursor::From(first.cursor), Some(4))
        .expect("second window");
    assert_eq!(second.bytes, b"4567");
    assert_eq!(second.cursor, 8);
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
            .output(&id, ReplayCursor::Full, None)
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
