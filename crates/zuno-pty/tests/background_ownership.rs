//! Cross-process ownership of one workspace's background execution store.
//!
//! Every case here drives the real reconciliation path in
//! `BackgroundExecutionService::open` against state files a second Zuno process
//! could have left in the directory, so the rules hold on every platform rather
//! than only where a fixture can spawn a POSIX shell.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use zuno_pty::{
    BackgroundExecutionError, BackgroundExecutionId, BackgroundExecutionService,
    BackgroundExecutionStatus, ReplayCursor,
};
use zuno_sandbox::{
    ExecutionAuthority, NetworkAccess, PrepareRequest, PreparedCommand, SandboxCapabilities,
    SandboxMode, SandboxPolicy,
};

/// The program a fixture records. It is never executed, but a state file that
/// encoded a POSIX path would not describe a Windows execution.
fn fixture_program() -> OsString {
    if cfg!(windows) {
        OsString::from("cmd.exe")
    } else {
        OsString::from("/bin/sh")
    }
}

fn authority(directory: &Path) -> ExecutionAuthority {
    let program = fixture_program();
    let request = PrepareRequest {
        program: program.clone(),
        arguments: Vec::new(),
        cwd: directory.to_owned(),
        environment: BTreeMap::new(),
        policy: SandboxPolicy::new(
            directory,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
        )
        .expect("fixture policy"),
    };
    PreparedCommand::from_backend(
        request,
        program,
        Vec::new(),
        &SandboxCapabilities {
            backend: "test_direct".to_owned(),
            executable: None,
            read_only: true,
            workspace_write: true,
            danger_full_access: false,
            network_isolation: true,
        },
        vec![directory.to_owned()],
        Vec::new(),
    )
    .authority()
    .clone()
}

fn state(directory: &Path, id: &BackgroundExecutionId, status: &str) -> serde_json::Value {
    serde_json::json!({
        "format": 3,
        "info": {
            "id": id.as_str(),
            "sessionId": "ses_ownership",
            "title": "fixture",
            "command": "fixture",
            "cwd": directory,
            "status": status,
            "pid": 4242,
            "exitCode": null,
            "timedOut": false,
            "timeCreated": 1,
            "timeUpdated": 1,
            "timeCompleted": null,
            "error": null,
            "outputFile": "/must/not/be/trusted",
            "statusFile": "/must/not/be/trusted",
            "authority": authority(directory)
        }
    })
}

fn seed(directory: &Path, id: &BackgroundExecutionId, status: &str) {
    std::fs::write(directory.join(format!("{id}.output")), b"partial").expect("fixture output");
    write_row(directory, id, state(directory, id, status));
}

/// Seeds a row that says whether an ownership claim backed it, which is what a build
/// after 0.6.x records and what an unlockable filesystem records as `false`.
fn seed_claimed(directory: &Path, id: &BackgroundExecutionId, status: &str, claimed: bool) {
    std::fs::write(directory.join(format!("{id}.output")), b"partial").expect("fixture output");
    let mut row = state(directory, id, status);
    row["claimed"] = serde_json::json!(claimed);
    write_row(directory, id, row);
}

fn write_row(directory: &Path, id: &BackgroundExecutionId, row: serde_json::Value) {
    std::fs::write(
        directory.join(format!("{id}.status.json")),
        serde_json::to_vec_pretty(&row).expect("fixture JSON"),
    )
    .expect("fixture status");
}

/// The row an owner publishes when its command exits, before it releases its claim.
fn settled_row(
    directory: &Path,
    id: &BackgroundExecutionId,
    status: &str,
    exit_code: i32,
) -> serde_json::Value {
    let mut row = state(directory, id, status);
    row["claimed"] = serde_json::json!(true);
    row["info"]["pid"] = serde_json::Value::Null;
    row["info"]["exitCode"] = serde_json::json!(exit_code);
    row["info"]["timeCompleted"] = serde_json::json!(9);
    row
}

fn persisted(directory: &Path, id: &BackgroundExecutionId) -> serde_json::Value {
    serde_json::from_slice(
        &std::fs::read(directory.join(format!("{id}.status.json"))).expect("status file"),
    )
    .expect("status JSON")
}

/// Holds one execution's claim the way the owning process holds it.
fn hold_claim(directory: &Path, id: &BackgroundExecutionId) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{id}.lock")))
        .expect("claim file");
    file.try_lock().expect("peer claim");
    file
}

fn id(value: &str) -> BackgroundExecutionId {
    BackgroundExecutionId::parse(value).expect("fixture id")
}

#[test]
fn a_running_row_a_live_process_still_owns_is_neither_settled_nor_reclaimed() {
    let directory = tempfile::tempdir().expect("workspace");
    let id = id("bg_2123456789abcdef0123456789abcdef");
    // Claim-backed, which is what an owner that holds `<id>.lock` writes. The claim is then
    // the whole proof in both directions and no pid is probed, which is why this case reads
    // identically on every platform. A row *without* the marker is a different question -
    // see `a_row_from_before_the_claim_protocol_is_not_settled_from_an_unprobeable_pid`.
    seed_claimed(directory.path(), &id, "running", true);
    let claim = hold_claim(directory.path(), &id);

    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
    let observed = service.get(&id).expect("the peer's row stays visible");

    assert_eq!(observed.status, BackgroundExecutionStatus::Running);
    assert_eq!(observed.pid, Some(4242));
    assert!(observed.error.is_none());
    assert!(observed.output_file.exists(), "live output must survive");
    let row = persisted(directory.path(), &id);
    assert_eq!(row["info"]["status"], "running");
    assert_eq!(row["info"]["pid"], 4242);
    assert!(matches!(
        service.cancel(&id),
        Err(BackgroundExecutionError::Foreign(_))
    ));
    drop(service);

    claim.unlock().expect("owner releases its claim");
    drop(claim);
    let reopened = BackgroundExecutionService::open(directory.path()).expect("service reopens");
    let settled = reopened.get(&id).expect("the abandoned row is reconciled");

    assert_eq!(settled.status, BackgroundExecutionStatus::Uncertain);
    assert!(
        settled
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not replayed"))
    );
    assert!(
        !directory.path().join(format!("{id}.lock")).exists(),
        "a reclaimed claim must not linger"
    );
}

#[test]
fn an_uninterpretable_state_file_is_skipped_instead_of_failing_every_session() {
    let directory = tempfile::tempdir().expect("workspace");
    let future = id("bg_3123456789abcdef0123456789abcdef");
    let corrupt = id("bg_4123456789abcdef0123456789abcdef");
    let usable = id("bg_5123456789abcdef0123456789abcdef");
    seed(directory.path(), &usable, "completed");
    seed(directory.path(), &future, "running");
    let mut ahead = state(directory.path(), &future, "running");
    ahead["format"] = serde_json::json!(STATE_FORMAT_AHEAD);
    std::fs::write(
        directory.path().join(format!("{future}.status.json")),
        serde_json::to_vec_pretty(&ahead).expect("fixture JSON"),
    )
    .expect("fixture status");
    seed(directory.path(), &corrupt, "running");
    std::fs::write(
        directory.path().join(format!("{corrupt}.status.json")),
        b"{ this is not state",
    )
    .expect("fixture status");

    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");

    assert_eq!(
        service.get(&usable).expect("usable row").status,
        BackgroundExecutionStatus::Completed
    );
    assert!(matches!(
        service.get(&future),
        Err(BackgroundExecutionError::NotFound(_))
    ));
    assert!(matches!(
        service.get(&corrupt),
        Err(BackgroundExecutionError::NotFound(_))
    ));
    for rejected in [&future, &corrupt] {
        assert!(
            directory
                .path()
                .join(format!("{rejected}.status.json"))
                .exists(),
            "a rejected state file is the only evidence of what happened"
        );
        assert!(
            directory.path().join(format!("{rejected}.output")).exists(),
            "a rejected row's output must not be swept as an orphan"
        );
    }
}

#[test]
fn an_orphaned_capture_file_is_reclaimed_only_when_no_process_owns_it() {
    let directory = tempfile::tempdir().expect("workspace");
    let dead = id("bg_6123456789abcdef0123456789abcdef");
    let live = id("bg_7123456789abcdef0123456789abcdef");
    std::fs::write(directory.path().join(format!("{dead}.output")), b"lost").expect("dead output");
    std::fs::write(directory.path().join(format!("{dead}.lock")), b"").expect("stale claim");
    std::fs::write(
        directory.path().join(format!("{dead}.status.json.tmp")),
        b"partial",
    )
    .expect("stale temporary");
    std::fs::write(
        directory.path().join(format!("{live}.output")),
        b"streaming",
    )
    .expect("live output");
    let claim = hold_claim(directory.path(), &live);

    let _service = BackgroundExecutionService::open(directory.path()).expect("service opens");

    assert!(
        !directory.path().join(format!("{dead}.output")).exists(),
        "an unowned capture file is reclaimed"
    );
    assert!(!directory.path().join(format!("{dead}.lock")).exists());
    assert!(
        !directory
            .path()
            .join(format!("{dead}.status.json.tmp"))
            .exists()
    );
    assert!(
        directory.path().join(format!("{live}.output")).exists(),
        "a live process's capture file must survive a peer's sweep"
    );
    drop(claim);
}

/// One past [`STATE_FORMAT`], which is what a newer Zuno writes into a shared checkout.
const STATE_FORMAT_AHEAD: u32 = 4;

/// The asymmetry the round-1 claim protocol left open: the writer's own claim attempt
/// failed (an unlockable mount, an out-of-descriptors process, a Windows pending delete),
/// so there is no lock file to acquire - and a reader whose attempt *would* succeed must
/// not read that absence as proof the owner exited.
#[test]
fn a_running_row_written_without_an_ownership_claim_is_not_settled_by_a_peer() {
    let directory = tempfile::tempdir().expect("workspace");
    let id = id("bg_8223456789abcdef0123456789abcdef");
    seed_claimed(directory.path(), &id, "running", false);
    assert!(
        !directory.path().join(format!("{id}.lock")).exists(),
        "the writer never managed to create a claim"
    );

    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
    let observed = service.get(&id).expect("the row stays visible");

    assert_eq!(observed.status, BackgroundExecutionStatus::Running);
    assert_eq!(observed.pid, Some(4242));
    assert!(observed.error.is_none());
    assert!(
        observed.output_file.exists(),
        "the capture of a possibly live command must survive"
    );
    let row = persisted(directory.path(), &id);
    assert_eq!(
        row["info"]["status"], "running",
        "nothing proves this command is over, so its row must not be rewritten"
    );
    assert_eq!(row["claimed"], serde_json::json!(false));
    assert!(matches!(
        service.cancel(&id),
        Err(BackgroundExecutionError::Foreign(_))
    ));

    // Refusing to settle must not strand the row: the refusal is now taken before the
    // claim is acquired, so the only read that can converge this row is the one
    // `refresh_foreign` performs on the next call. The owner records its outcome.
    write_row(
        directory.path(),
        &id,
        settled_row(directory.path(), &id, "completed", 0),
    );
    let adopted = service
        .get(&id)
        .expect("an unclaimed row is still re-read, not stranded");

    assert_eq!(
        adopted.status,
        BackgroundExecutionStatus::Completed,
        "a row nothing here may settle must still adopt the outcome its owner wrote"
    );
    assert_eq!(adopted.exit_code, Some(0));
}

/// The released 0.6.6 build writes a `running` row with no `claimed` marker and no
/// `<id>.lock`, so the marker cannot decide such a row and the recorded pid is the only
/// other thing it states about its owner. A row that does not even state that (`pid: null`)
/// is not resolvable from anything this process trusts, so it fails closed: left visible,
/// left readable, and left out of retention rather than settled on a guess. This is the one
/// branch of the released-format gate that behaves identically on Linux, macOS and Windows.
#[test]
fn a_row_from_before_the_claim_protocol_is_not_settled_from_an_unprobeable_pid() {
    let directory = tempfile::tempdir().expect("workspace");
    let id = id("bg_c223456789abcdef0123456789abcdef");
    let mut row = state(directory.path(), &id, "running");
    row["info"]["pid"] = serde_json::Value::Null;
    std::fs::write(directory.path().join(format!("{id}.output")), b"partial").expect("capture");
    write_row(directory.path(), &id, row);
    assert!(
        !directory.path().join(format!("{id}.lock")).exists(),
        "the released build creates no claim file at all"
    );

    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
    let observed = service.get(&id).expect("the row stays visible");

    assert_eq!(observed.status, BackgroundExecutionStatus::Running);
    assert!(observed.error.is_none());
    assert!(
        observed.output_file.exists(),
        "the capture of a possibly live command must survive"
    );
    let persisted_row = persisted(directory.path(), &id);
    assert_eq!(persisted_row["info"]["status"], "running");
    assert!(
        persisted_row.get("claimed").is_none(),
        "a reader that owns nothing here may not stamp a marker on the row either"
    );
    assert!(matches!(
        service.cancel(&id),
        Err(BackgroundExecutionError::Foreign(_))
    ));
}

/// The sibling of the row gate, on the sweep that deletes artifacts instead of rewriting
/// rows. Every capture file this build creates is preceded by its `<id>.lock`, and a
/// process killed before it could clean up leaves both. A capture with no lock beside it is
/// therefore a writer that never took one - a released build's live foreground command, or
/// an unlockable filesystem - and creating a fresh lock, finding nobody holding it and
/// deleting the capture is the same mistake the row gate refuses.
#[test]
fn an_orphaned_capture_with_no_claim_file_beside_it_is_not_swept() {
    let directory = tempfile::tempdir().expect("workspace");
    let released = id("bg_d223456789abcdef0123456789abcdef");
    let ours = id("bg_e223456789abcdef0123456789abcdef");
    std::fs::write(
        directory.path().join(format!("{released}.output")),
        b"streaming",
    )
    .expect("released capture");
    std::fs::write(directory.path().join(format!("{ours}.output")), b"lost")
        .expect("our own leftover");
    std::fs::write(directory.path().join(format!("{ours}.lock")), b"").expect("its free claim");

    let _service = BackgroundExecutionService::open(directory.path()).expect("service opens");

    assert!(
        directory.path().join(format!("{released}.output")).exists(),
        "a capture no claim protocol ever covered must not be deleted on a guess"
    );
    assert!(
        !directory.path().join(format!("{ours}.output")).exists(),
        "a capture whose free claim proves its writer is gone is still reclaimed"
    );
    assert!(!directory.path().join(format!("{ours}.lock")).exists());
}

/// A row another process owns is a snapshot, and a snapshot that never refreshes reports a
/// finished command as `running` with a dead pid for the life of this process.
#[tokio::test]
async fn a_peer_owned_row_converges_when_its_owner_records_an_outcome() {
    let directory = tempfile::tempdir().expect("workspace");
    let id = id("bg_9223456789abcdef0123456789abcdef");
    seed_claimed(directory.path(), &id, "running", true);
    let claim = hold_claim(directory.path(), &id);
    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
    assert_eq!(
        service.get(&id).expect("peer row").status,
        BackgroundExecutionStatus::Running
    );
    assert!(matches!(
        service.wait(&id, None).await,
        Err(BackgroundExecutionError::Foreign(_))
    ));

    // The owner appends to its capture while it runs.
    OpenOptions::new()
        .append(true)
        .open(directory.path().join(format!("{id}.output")))
        .expect("owner capture")
        .write_all(b"more")
        .expect("owner append");
    let replayed = service
        .output(&id, ReplayCursor::Full, None)
        .expect("replay of a peer's live capture");

    assert_eq!(replayed.bytes, b"partialmore");
    assert_eq!(replayed.total_written, 11);

    // Then it exits: `run_execution` publishes the terminal row before releasing the claim.
    write_row(
        directory.path(),
        &id,
        settled_row(directory.path(), &id, "completed", 0),
    );
    let adopted = service.get(&id).expect("peer row");

    assert_eq!(adopted.status, BackgroundExecutionStatus::Completed);
    assert_eq!(adopted.exit_code, Some(0));
    assert_eq!(adopted.pid, None);
    let waited = service
        .wait(&id, None)
        .await
        .expect("a settled peer row is no longer refused");
    assert_eq!(waited.info.status, BackgroundExecutionStatus::Completed);
    assert!(!waited.timed_out);
    assert!(
        !service
            .cancel(&id)
            .expect("a terminal row cancels idempotently"),
        "there is nothing left to cancel"
    );
    assert_eq!(
        service.list().len(),
        1,
        "the adopted row stays listed for this workspace"
    );
    drop(claim);
}

/// The other half of convergence: the owner died without recording anything, which is only
/// observable once its claim is free. Waiting for the next process start to notice loses the
/// `uncertain` settlement for the whole life of this process.
#[test]
fn a_peer_owned_row_is_settled_as_uncertain_as_soon_as_its_claim_is_free() {
    let directory = tempfile::tempdir().expect("workspace");
    let id = id("bg_a223456789abcdef0123456789abcdef");
    seed_claimed(directory.path(), &id, "running", true);
    let claim = hold_claim(directory.path(), &id);
    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
    assert_eq!(
        service.get(&id).expect("peer row").status,
        BackgroundExecutionStatus::Running
    );

    // What the OS does to the lock when the owning process dies: it releases it and leaves
    // the file behind.
    claim.unlock().expect("the dying owner releases its claim");
    drop(claim);
    let settled = service
        .get(&id)
        .expect("the abandoned row is reconciled here");

    assert_eq!(settled.status, BackgroundExecutionStatus::Uncertain);
    assert!(
        settled
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not replayed"))
    );
    assert_eq!(
        persisted(directory.path(), &id)["info"]["status"],
        "uncertain",
        "the reconciled outcome is durable, not only in memory"
    );
    assert!(!directory.path().join(format!("{id}.lock")).exists());
}

/// A row whose owner retired it is gone, not permanently `running`: its files are the only
/// place this process could have learned anything about it.
#[test]
fn a_peer_owned_row_its_owner_retired_stops_being_reported() {
    let directory = tempfile::tempdir().expect("workspace");
    let id = id("bg_b223456789abcdef0123456789abcdef");
    seed_claimed(directory.path(), &id, "running", true);
    let claim = hold_claim(directory.path(), &id);
    let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
    assert_eq!(service.list().len(), 1);

    // The owner settled and its retention pruned the row.
    std::fs::remove_file(directory.path().join(format!("{id}.status.json"))).expect("prune row");
    std::fs::remove_file(directory.path().join(format!("{id}.output"))).expect("prune capture");

    assert!(matches!(
        service.get(&id),
        Err(BackgroundExecutionError::NotFound(_))
    ));
    assert!(service.list().is_empty());
    drop(claim);
}
