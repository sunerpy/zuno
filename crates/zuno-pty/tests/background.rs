#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use zuno_pty::{
    BUFFER_LIMIT, BackgroundExecutionError, BackgroundExecutionId, BackgroundExecutionInput,
    BackgroundExecutionPurpose, BackgroundExecutionRetention, BackgroundExecutionService,
    BackgroundExecutionStatus, MAX_RETAINED_TERMINAL_EXECUTIONS, ReplayCursor,
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

/// An ephemeral command's capture file is created after its `<id>.lock` and has to be
/// removed before it, because a capture with no claim beside it is what a build from
/// before the claim protocol leaves and `reclaim_orphan` deliberately never sweeps one.
/// This pins the resting state a peer can observe once the caller has the bytes: neither
/// file left behind, the claim included.
#[tokio::test]
async fn consuming_a_foreground_result_leaves_neither_its_capture_nor_its_claim() {
    let directory = tempfile::tempdir().expect("workspace");
    let service = BackgroundExecutionService::open(directory.path()).expect("background service");
    let mut launch = input(directory.path(), "printf done", Duration::from_secs(30));
    launch.retention = BackgroundExecutionRetention::Ephemeral;
    let started = service.start(launch).expect("command starts");
    let capture = started.output_file.clone();
    let claim = directory
        .path()
        .join(format!("{}.lock", started.id.as_str()));
    assert!(claim.exists(), "the claim precedes the capture it covers");
    service
        .wait(&started.id, None)
        .await
        .expect("the command settles");

    let output = service
        .finish_foreground(&started.id)
        .expect("the caller consumes the terminal result");

    assert_eq!(output, b"done");
    assert!(
        !capture.exists(),
        "the capture file is consumed and removed"
    );
    assert!(
        !claim.exists(),
        "and its claim goes with it: a leaked claim file would make every later id \
         collision look owned, and a claim released before the capture would leave the \
         capture in the one state nothing sweeps"
    );
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

/// A bounded read of "everything still retained" is the newest window, not the oldest.
///
/// This is the request a caller makes when it names no cursor at all. Serving it from the
/// head returned an execution's opening bytes forever: every poll of a running command
/// came back identical, and the summary the caller was waiting for was one call per window
/// away.
#[tokio::test]
async fn a_bounded_retained_read_returns_the_newest_window_of_an_execution() {
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

    let window = service
        .output(&info.id, ReplayCursor::Full, Some(4))
        .expect("newest window");

    assert_eq!(window.bytes, b"6789");
    assert_eq!(
        window.cursor, window.total_written,
        "a tail window leaves nothing newer to page toward"
    );
    assert!(
        !window.from_disk,
        "the newest bytes are always still retained"
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

/// A row in the shape Zuno 0.6.6 wrote it - format 3, no `claimed` marker, no `<id>.lock` -
/// whose command is over. It is reconciled exactly as the released build reconciled it,
/// because the pid it recorded is one this process spawned and reaped, so its absence is
/// provable rather than assumed from the missing lock file.
#[test]
fn persisted_running_state_reconciles_to_uncertain_without_replay() {
    let directory = tempfile::tempdir().expect("workspace");
    let id =
        BackgroundExecutionId::parse("bg_0123456789abcdef0123456789abcdef").expect("fixture id");
    let output_file = directory.path().join(format!("{id}.output"));
    let status_file = directory.path().join(format!("{id}.status.json"));
    let reaped = reaped_pid();
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
                "pid": reaped,
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

#[tokio::test]
async fn a_second_service_leaves_a_live_process_execution_to_its_owner() {
    let directory = tempfile::tempdir().expect("workspace");
    let pid_file = directory.path().join("live.pid");
    let command = format!("printf '%s' \"$$\" > '{}'; sleep 30", pid_file.display());
    let owner = BackgroundExecutionService::open(directory.path()).expect("owning service");
    let started = owner
        .start(input(directory.path(), command, Duration::from_secs(30)))
        .expect("command starts");
    wait_for_file(&pid_file).await;

    // What a concurrent `zuno run` or `zuno serve` in the same worktree does.
    let peer = BackgroundExecutionService::open(directory.path()).expect("peer service");
    let observed = peer
        .get(&started.id)
        .expect("the running row stays visible");

    assert_eq!(observed.status, BackgroundExecutionStatus::Running);
    assert_eq!(observed.pid, started.pid);
    assert!(started.output_file.exists(), "live output must survive");
    assert!(started.status_file.exists(), "live state must survive");
    assert!(matches!(
        peer.cancel(&started.id),
        Err(BackgroundExecutionError::Foreign(_))
    ));
    assert!(matches!(
        peer.wait(&started.id, None).await,
        Err(BackgroundExecutionError::Foreign(_))
    ));

    assert!(owner.cancel(&started.id).expect("the owner still cancels"));
    let settled = owner
        .wait(&started.id, None)
        .await
        .expect("owner cancellation settles")
        .info;
    assert_eq!(settled.status, BackgroundExecutionStatus::Cancelled);
    wait_for_process_exit(read_pid(&pid_file)).await;

    // The peer's row is a snapshot of someone else's execution, and nothing in this
    // process will ever move it. Without a refresh it reports `running` with a dead pid
    // and refuses every wait for as long as this process lives.
    let converged = peer.get(&started.id).expect("the peer's row converges");
    assert_eq!(converged.status, BackgroundExecutionStatus::Cancelled);
    assert_eq!(converged.pid, None);
    let waited = peer
        .wait(&started.id, None)
        .await
        .expect("a settled row is no longer refused");
    assert_eq!(waited.info.status, BackgroundExecutionStatus::Cancelled);
    assert!(
        !peer
            .cancel(&started.id)
            .expect("terminal cancel is a no-op")
    );
}

#[tokio::test]
async fn a_peer_sweep_reclaims_a_dead_capture_and_keeps_a_live_one() {
    let directory = tempfile::tempdir().expect("workspace");
    let owner = BackgroundExecutionService::open(directory.path()).expect("owning service");
    let mut foreground = input(directory.path(), "sleep 30", Duration::from_secs(30));
    foreground.retention = BackgroundExecutionRetention::Ephemeral;
    let live = owner.start(foreground).expect("foreground command starts");
    // What this build leaves behind when it is killed before it can clean up: the claim
    // file it created before the capture (`claim_capture`) survives the process, and the
    // OS releases the lock on it. That pair - a capture beside a claim nobody holds - is
    // what proves the writer is gone. A capture with no claim file beside it proves
    // nothing and is left alone; `an_orphaned_capture_with_no_claim_file_beside_it_is_not_swept`
    // pins that half.
    let dead = directory
        .path()
        .join("bg_8123456789abcdef0123456789abcdef.output");
    std::fs::write(&dead, b"lost").expect("dead capture");
    let dead_claim = directory
        .path()
        .join("bg_8123456789abcdef0123456789abcdef.lock");
    std::fs::write(&dead_claim, b"").expect("its released claim");

    let _peer = BackgroundExecutionService::open(directory.path()).expect("peer service");

    assert!(
        live.output_file.exists(),
        "a live foreground capture has no state row and must survive a peer's sweep"
    );
    assert!(!dead.exists(), "a capture no process owns is reclaimed");
    assert!(!dead_claim.exists(), "and so is the claim that proved it");
    let _cancelled = owner.cancel(&live.id);
}

#[tokio::test]
async fn a_state_path_that_is_not_utf8_reports_an_error_instead_of_aborting_the_turn() {
    use std::os::unix::ffi::OsStrExt as _;

    let directory = tempfile::tempdir().expect("workspace");
    let root = directory
        .path()
        .join(std::ffi::OsStr::from_bytes(b"background-\xff"));
    std::fs::create_dir(&root).expect("non-utf8 root");
    let service = BackgroundExecutionService::open(&root).expect("background service");

    let error = service
        .start(input(directory.path(), "printf hi", Duration::from_secs(2)))
        .expect_err("a state file that cannot be encoded is an error, not an abort");

    assert!(
        matches!(error, BackgroundExecutionError::State { .. }),
        "{error}"
    );
    assert_eq!(
        std::fs::read_dir(&root).expect("state root").count(),
        0,
        "a failed start leaves no state, capture, or claim behind"
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

/// The reviewer's input for the released-format gate: a real `sleep 30` recorded in a row
/// that is byte-for-byte what Zuno 0.6.6 writes. The marker `claimed` is optional, so the
/// released build - which has no claim protocol and creates no `<id>.lock` at all - writes
/// exactly the row a reader could mistake for "the owner is gone".
///
/// Settling it reports a command that is running as `uncertain` / "was not replayed", and
/// under retention pressure takes its `.status.json` and `.output` with it while the owner
/// is still appending to an unlinked inode. Every background command a released Zuno
/// started and is still running across an upgrade is this row.
#[tokio::test]
async fn a_released_format_row_is_left_alone_while_the_command_it_names_still_runs() {
    let directory = tempfile::tempdir().expect("workspace");
    let pid_file = directory.path().join("released.pid");
    let command = format!(
        "printf '%s' \"$$\" > '{}'; printf ready; sleep 30",
        pid_file.display()
    );
    let owner = BackgroundExecutionService::open(directory.path()).expect("owning service");
    let started = owner
        .start(input(directory.path(), command, Duration::from_secs(30)))
        .expect("command starts");
    wait_for_file(&pid_file).await;
    // The capture is written by the owner's pump task, so wait for the bytes rather than
    // for the process: a peer must be able to read a released row's output, and an empty
    // file would pass that assertion for the wrong reason.
    wait_for_file(&started.output_file).await;
    let live_pid = started.pid.expect("a spawned command records its pid");

    // Downgrade the row this build just wrote to the released format. `git show
    // df4d7490:crates/zuno-pty/src/background.rs` serializes `PersistedExecution { format,
    // info }` with an identical `BackgroundExecutionInfo`, so the released row is this row
    // without the marker - and the released build never creates the lock file.
    let mut row = read_row(&started.status_file);
    assert_eq!(
        row["claimed"],
        serde_json::json!(true),
        "this build records the claim it holds"
    );
    assert_eq!(row["info"]["pid"], serde_json::json!(live_pid));
    let object = row.as_object_mut().expect("row object");
    assert!(object.remove("claimed").is_some());
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["format", "info"],
        "the released row is this row minus the marker: another top-level key means the \
         released shape has to be re-derived before this fixture means anything"
    );
    std::fs::write(
        &started.status_file,
        serde_json::to_vec_pretty(&row).expect("released row"),
    )
    .expect("released row is written");
    let lock_file = directory.path().join(format!("{}.lock", started.id));
    assert!(lock_file.exists(), "this build took a claim");
    std::fs::remove_file(&lock_file).expect("the released build creates no claim file");
    // Enough terminal history that a settled row would be pruned on sight.
    for index in 0..=MAX_RETAINED_TERMINAL_EXECUTIONS {
        seed_completed_row(directory.path(), &row, index);
    }

    // What an upgraded `zuno run` or `zuno serve` opening the same worktree does.
    let peer = BackgroundExecutionService::open(directory.path()).expect("peer service");
    let observed = peer
        .get(&started.id)
        .expect("the released row stays visible");

    assert_eq!(observed.status, BackgroundExecutionStatus::Running);
    assert_eq!(observed.pid, Some(live_pid));
    assert!(observed.error.is_none(), "{:?}", observed.error);
    assert!(
        started.output_file.exists(),
        "a live command's capture must survive a peer that cannot prove its owner is gone"
    );
    assert!(started.status_file.exists(), "and so must its row");
    let on_disk = read_row(&started.status_file);
    assert_eq!(
        on_disk["info"]["status"], "running",
        "nothing proves this command is over, so its row must not be rewritten"
    );
    assert!(
        on_disk.get("claimed").is_none(),
        "a peer that owns nothing here may not stamp a claim marker either"
    );
    assert_eq!(
        peer.output(&started.id, ReplayCursor::Full, None)
            .expect("the released row's capture is readable")
            .bytes,
        b"ready",
        "a released row still reads through the new build"
    );
    assert!(matches!(
        peer.cancel(&started.id),
        Err(BackgroundExecutionError::Foreign(_))
    ));
    assert_eq!(
        owner
            .get(&started.id)
            .expect("the owner still owns it")
            .status,
        BackgroundExecutionStatus::Running,
        "the command really was running for the whole check"
    );

    // And it still converges: once the owner publishes an outcome, the peer adopts it.
    assert!(owner.cancel(&started.id).expect("the owner cancels"));
    let settled = owner
        .wait(&started.id, None)
        .await
        .expect("owner cancellation settles")
        .info;
    assert_eq!(settled.status, BackgroundExecutionStatus::Cancelled);
    wait_for_process_exit(read_pid(&pid_file)).await;
    let converged = peer.get(&started.id).expect("the peer's row converges");
    assert_eq!(converged.status, BackgroundExecutionStatus::Cancelled);
}

fn read_row(status_file: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(status_file).expect("row bytes")).expect("row JSON")
}

/// One retained terminal row, derived from a real one so its `authority` and paths are real.
fn seed_completed_row(directory: &Path, template: &serde_json::Value, index: usize) {
    let id = BackgroundExecutionId::parse(format!("bg_{index:032x}")).expect("fixture id");
    let mut row = template.clone();
    row["claimed"] = serde_json::json!(true);
    row["info"]["id"] = serde_json::json!(id.as_str());
    row["info"]["status"] = serde_json::json!("completed");
    row["info"]["pid"] = serde_json::Value::Null;
    row["info"]["exitCode"] = serde_json::json!(0);
    row["info"]["timeCreated"] = serde_json::json!(index);
    row["info"]["timeUpdated"] = serde_json::json!(index);
    row["info"]["timeCompleted"] = serde_json::json!(index);
    std::fs::write(directory.join(format!("{id}.output")), b"old").expect("fixture output");
    std::fs::write(
        directory.join(format!("{id}.status.json")),
        serde_json::to_vec_pretty(&row).expect("fixture row"),
    )
    .expect("fixture status");
}

/// A pid no process can be using: spawned, waited for, and therefore reaped.
fn reaped_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("a shell runs");
    let pid = child.id();
    assert!(child.wait().expect("the child is reaped").success());
    pid
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
