use std::collections::BTreeSet;
use std::fs;

use zuno_db::artifact_gc::ArtifactGcPaths;
use zuno_db::prune::{RemoteUnshare, SharedSession, UnshareError};
use zuno_db::retention::{DAY_MILLIS, Liveness, LivenessProbe, RetentionKey};
use zuno_db::session_prune::{
    ProgressPhase, SessionPruneAction, SessionPruneProgress, SessionPruneProgressSink,
    SessionPruneRequest, SessionPruneScope, execute, to_json_bytes,
};
use zuno_db::{Connection, Pool, migration};
use zuno_paths::DbLocation;
use zuno_snapshot::StoreKey;
use zuno_tool::ToolOutputStore;

const NOW: i64 = 200 * DAY_MILLIS;
const OLD: i64 = 10 * DAY_MILLIS;
const NEW: i64 = 190 * DAY_MILLIS;

struct Reachable;

impl LivenessProbe for Reachable {
    fn probe(&self) -> Liveness {
        Liveness::Reachable {
            active_session_ids: BTreeSet::new(),
        }
    }
}

struct Remote;

impl RemoteUnshare for Remote {
    fn unshare(&self, _session: &SharedSession) -> Result<(), UnshareError> {
        Ok(())
    }
}

#[derive(Default)]
struct Progress(Vec<SessionPruneProgress>);

impl SessionPruneProgressSink for Progress {
    fn emit(&mut self, progress: SessionPruneProgress) {
        self.0.push(progress);
    }
}

struct Fixture {
    pool: Pool,
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
        {
            let mut connection = pool.get().expect("check out connection");
            migration::apply(&mut connection).expect("apply schema");
            seed(&connection);
        }
        let temp = tempfile::tempdir().expect("temporary artifact root");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        write(
            &paths
                .tool_output
                .join("tool_ses_old_00000000000000000000000000000001"),
            b"old output",
        );
        Self { pool, temp }
    }

    fn paths(&self) -> ArtifactGcPaths {
        ArtifactGcPaths::from_data_root(self.temp.path())
    }

    fn connection(&self) -> zuno_db::PooledConnection<'_> {
        self.pool.get().expect("check out connection")
    }
}

fn seed(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_a', '/srv/a', 1, 1, '[]'),
                    ('prj_b', '/srv/b', 1, 1, '[]');",
        )
        .expect("insert projects");
    insert_session(connection, "ses_old", "prj_a", None, OLD, false);
    insert_session(
        connection,
        "ses_child",
        "prj_a",
        Some("ses_old"),
        NEW,
        false,
    );
    insert_session(connection, "ses_shared", "prj_a", None, OLD, true);
    insert_session(connection, "ses_other", "prj_b", None, OLD, false);
    for id in ["ses_old", "ses_child", "ses_shared", "ses_other"] {
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, 1, 1, '{}')",
                rusqlite::params![format!("msg_{id}"), id],
            )
            .expect("insert message");
    }
}

fn insert_session(
    connection: &Connection,
    id: &str,
    project: &str,
    parent: Option<&str>,
    updated: i64,
    shared: bool,
) {
    connection
        .execute(
            "INSERT INTO session
               (id, project_id, parent_id, slug, directory, title, version, share_url,
                cost, tokens_input, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?1, '/srv/a', ?1, 'test', ?4, 1.5, 7, ?5, ?5)",
            rusqlite::params![
                id,
                project,
                parent,
                shared.then_some("https://share.example/session"),
                updated,
            ],
        )
        .expect("insert session");
}

fn write(path: &std::path::Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture path parent")).expect("create parent");
    fs::write(path, bytes).expect("write artifact");
}

fn backdate(path: &std::path::Path, age: std::time::Duration) {
    let when = std::time::SystemTime::now()
        .checked_sub(age)
        .expect("representable old timestamp");
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open fixture")
        .set_modified(when)
        .expect("set fixture mtime");
}

fn request(action: SessionPruneAction) -> SessionPruneRequest {
    SessionPruneRequest {
        older_than_days: 90,
        scope: SessionPruneScope::Project("prj_a".to_owned()),
        key: RetentionKey::Updated,
        action,
        include_shared: false,
        include_recent: false,
        force: false,
        confirm_delete: false,
        now_ms: NOW,
    }
}

/// The one sentence that says the snapshot class was not evaluated.
///
/// Written out here rather than rendered through the crate's own `Display`: this string is
/// a published report value that `docs/session-retention.md` quotes verbatim, so a test
/// that asked the implementation to spell it could not notice the wording drifting away
/// from the documentation.
fn snapshot_skip_warning(database: &std::path::Path) -> String {
    format!(
        "`{}` retains 0 sessions after this operation; snapshot store reclamation is skipped \
         because a shared artifact cannot be attributed to a surviving session and may belong \
         to another channel's database.",
        database.display()
    )
}

fn session_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM session", [], |row| row.get(0))
        .expect("count sessions")
}

#[test]
fn session_prune_preview_is_inert_and_projects_artifact_reclamation() {
    let fixture = Fixture::new();
    let mut connection = fixture.connection();
    let before = session_count(&connection);
    let mut progress = Progress::default();

    let report = execute(
        &mut connection,
        &fixture.paths(),
        &request(SessionPruneAction::Preview),
        &Reachable,
        &Remote,
        &mut progress,
    )
    .expect("preview succeeds");

    assert_eq!(session_count(&connection), before);
    assert_eq!(report.action, SessionPruneAction::Preview);
    assert_eq!(report.selected_session_ids, ["ses_child", "ses_old"]);
    assert_eq!(report.excluded.len(), 1);
    assert_eq!(report.excluded[0].session_id, "ses_shared");
    assert_eq!(report.database.cost, 3.0);
    assert_eq!(report.database.tokens.input, 14);
    assert_eq!(report.changed_sessions, 0);
    assert_eq!(report.artifacts.total_bytes, 10);
    assert!(report.artifacts.items.iter().all(|item| !item.removed));
    assert!(
        fixture
            .paths()
            .tool_output
            .join("tool_ses_old_00000000000000000000000000000001")
            .is_file()
    );
    assert_eq!(
        progress
            .0
            .iter()
            .map(|event| event.phase)
            .collect::<Vec<_>>(),
        [
            ProgressPhase::Selecting,
            ProgressPhase::Selected,
            ProgressPhase::Database,
            ProgressPhase::Artifacts,
            ProgressPhase::Completed,
        ]
    );

    let first = to_json_bytes(&report).expect("serialize preview");
    let second = to_json_bytes(&report).expect("serialize preview again");
    assert_eq!(first, second, "both adapters must receive byte-stable JSON");
}

#[test]
fn session_prune_empty_database_warns_for_preview_and_delete() {
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let database_path = temp.path().join("opencode-local.db");
    let pool = Pool::open(&DbLocation::File(database_path.clone())).expect("open file database");
    let mut connection = pool.get().expect("check out connection");
    migration::apply(&mut connection).expect("apply schema");
    let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
    let warning = snapshot_skip_warning(&database_path);

    for action in [SessionPruneAction::Preview, SessionPruneAction::Delete] {
        let mut empty_request = request(action);
        empty_request.scope = SessionPruneScope::AllProjects;
        empty_request.confirm_delete = true;
        let report = execute(
            &mut connection,
            &paths,
            &empty_request,
            &Reachable,
            &Remote,
            &mut Progress::default(),
        )
        .expect("empty database produces an operator-visible report");

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0], warning);
        assert!(report.selected_session_ids.is_empty());
        assert!(report.artifacts.items.is_empty());
        assert_eq!(report.artifacts.total_bytes, 0);
    }
}

#[test]
fn session_prune_nonempty_database_with_no_selection_has_no_visibility_warning_or_global_gc() {
    let fixture = Fixture::new();
    let mut connection = fixture.connection();
    connection
        .execute(
            "UPDATE session SET time_created = ?1, time_updated = ?1",
            [NEW],
        )
        .expect("make every session recent");
    let paths = fixture.paths();
    let unreferenced = StoreKey::new("prj_orphan", std::path::Path::new("/srv/orphan"));
    write(
        &unreferenced
            .path_in(&paths.snapshots)
            .join("objects/history"),
        b"snapshot history",
    );
    let foreign_output = paths.tool_output.join("tool_019abcdef0123456789ABCDEFG");
    write(&foreign_output, b"old foreign output");
    backdate(
        &foreign_output,
        std::time::Duration::from_secs(8 * 24 * 60 * 60),
    );

    let report = execute(
        &mut connection,
        &paths,
        &request(SessionPruneAction::Preview),
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("empty prune selection succeeds without a global sweep");

    assert!(report.selected_session_ids.is_empty());
    assert!(report.warnings.is_empty());
    assert!(report.artifacts.items.is_empty());
    assert_eq!(report.artifacts.total_bytes, 0);
    assert!(unreferenced.path_in(&paths.snapshots).is_dir());
    assert!(foreign_output.is_file());
}

#[test]
fn session_prune_with_a_selection_reclaims_its_unreferenced_snapshot_store() {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    let mut connection = pool.get().expect("check out connection");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_old', '/srv/old', 1, 1, '[]'),
                    ('prj_live', '/srv/live', 1, 1, '[]');",
        )
        .expect("insert projects");
    insert_session(&connection, "ses_old_only", "prj_old", None, OLD, false);
    insert_session(&connection, "ses_live", "prj_live", None, NEW, false);
    let temp = tempfile::tempdir().expect("temporary artifact root");
    let paths = ArtifactGcPaths::from_data_root(temp.path());
    let selected_store = StoreKey::new("prj_old", std::path::Path::new("/srv/old"));
    write(
        &selected_store
            .path_in(&paths.snapshots)
            .join("objects/history"),
        b"selected history",
    );
    let mut delete = request(SessionPruneAction::Delete);
    delete.scope = SessionPruneScope::AllProjects;
    delete.confirm_delete = true;

    let report = execute(
        &mut connection,
        &paths,
        &delete,
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("selected-session prune succeeds");

    assert_eq!(report.selected_session_ids, ["ses_old_only"]);
    assert!(report.artifacts.items.iter().any(|item| {
        item.kind == "snapshot_store" && item.reason == "unreferenced_snapshot" && item.removed
    }));
    assert!(!selected_store.path_in(&paths.snapshots).exists());
    assert_eq!(session_count(&connection), 1);
}

/// `session prune` is the sweeper for the store beside a working copy, not only for the
/// shared one under `$DATA`. The artifact a surviving transcript still points at stays,
/// because that path is how the model reads output the limits withheld.
#[test]
fn session_prune_reclaims_in_checkout_tool_output_and_keeps_a_survivors_artifact() {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    let mut connection = pool.get().expect("check out connection");
    migration::apply(&mut connection).expect("apply schema");
    let temp = tempfile::tempdir().expect("temporary artifact root");
    let worktree = temp.path().join("checkout");
    fs::create_dir_all(&worktree).expect("create checkout");
    connection
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_a', ?1, 1, 1, '[]')",
            rusqlite::params![worktree.to_string_lossy()],
        )
        .expect("insert project");
    insert_session(&connection, "ses_old", "prj_a", None, OLD, false);
    insert_session(&connection, "ses_live", "prj_a", None, NEW, false);
    let store = ToolOutputStore::in_worktree(&worktree);
    let selected = store
        .persist("shell", "ses_old", "authoritative test summary")
        .expect("persist the pruned session's output");
    let survivor = store
        .persist("shell", "ses_live", "output a live transcript still names")
        .expect("persist the surviving session's output");
    let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
    let mut delete = request(SessionPruneAction::Delete);
    delete.confirm_delete = true;

    let report = execute(
        &mut connection,
        &paths,
        &delete,
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("delete succeeds");

    assert_eq!(report.selected_session_ids, ["ses_old"]);
    assert!(!selected.path.exists());
    assert!(survivor.path.is_file());
    assert!(
        report.artifacts.items.iter().any(|item| {
            item.path == zuno_paths::wire_path(&selected.path)
                && item.kind == "tool_output"
                && item.reason == "deleted_session:ses_old"
                && item.removed
        }),
        "{:?}",
        report.artifacts.items
    );
}

/// The database row deletion is already committed when the artifact stage runs, so an
/// artifact gate re-derived from the surviving rows failed the whole operation for the
/// user who pruned everything: no report said which sessions went, the caller could not
/// forget their grants, and the selected ids — the only thing that could still attribute
/// those files — were discarded with the error. The shared snapshot root is the one class
/// that needs a survivor to attribute it, and it stays untouched.
#[test]
fn session_prune_deleting_every_session_still_reports_and_reclaims_its_tool_output() {
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let database_path = temp.path().join("opencode-local.db");
    let pool = Pool::open(&DbLocation::File(database_path.clone())).expect("open file database");
    let mut connection = pool.get().expect("check out connection");
    migration::apply(&mut connection).expect("apply schema");
    let worktree = temp.path().join("checkout");
    fs::create_dir_all(&worktree).expect("create checkout");
    connection
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_only', ?1, 1, 1, '[]')",
            rusqlite::params![worktree.to_string_lossy()],
        )
        .expect("insert project");
    insert_session(&connection, "ses_only", "prj_only", None, OLD, false);
    let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
    let output = ToolOutputStore::in_worktree(&worktree)
        .persist("shell", "ses_only", "authoritative test summary")
        .expect("persist the pruned session's output");
    let foreign = StoreKey::new("prj_other_channel", std::path::Path::new("/srv/other"));
    write(
        &foreign.path_in(&paths.snapshots).join("objects/history"),
        b"another channel's history",
    );
    let mut delete = request(SessionPruneAction::Delete);
    delete.scope = SessionPruneScope::AllProjects;
    delete.confirm_delete = true;

    let report = execute(
        &mut connection,
        &paths,
        &delete,
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("a delete that empties the database still produces a report");

    assert_eq!(report.selected_session_ids, ["ses_only"]);
    assert_eq!(report.changed_sessions, 1);
    assert_eq!(session_count(&connection), 0);
    assert!(!output.path.exists());
    assert!(
        report.artifacts.items.iter().any(|item| {
            item.path == zuno_paths::wire_path(&output.path)
                && item.kind == "tool_output"
                && item.reason == "deleted_session:ses_only"
                && item.removed
        }),
        "{:?}",
        report.artifacts.items
    );
    assert!(
        foreign.path_in(&paths.snapshots).is_dir(),
        "a database with no survivors cannot attribute a channel-shared snapshot store"
    );
    // Skipping the class silently was the cost of letting the operation finish: the
    // pre-mutation visibility check runs while the row being deleted is still there, so it
    // cannot fire for this case, and after the commit no row is left to attribute those
    // stores on any later pass. The report is the only place the bytes can still be named.
    assert_eq!(
        report.warnings,
        [snapshot_skip_warning(&database_path)],
        "a skipped artifact class is reported, not silent"
    );
}

/// The held-back attachment bytes reach the report an operator actually reads.
///
/// `artifact_gc` can only record the suppression; `session_prune` is the surface that
/// publishes it, and `warnings` is the one field the CLI's text and JSON output and the
/// server's maintenance handler both render. Without this edge the interface and its
/// producer existed with no consumer, and the operator still saw a clean zero.
///
/// The sentence is written out here instead of rendered through the crate's own `Display`,
/// for the same reason `snapshot_skip_warning` is: a test that asks the implementation to
/// spell a published value cannot notice that value changing.
#[test]
fn session_prune_reports_attachment_bytes_a_surviving_prose_row_holds_back() {
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let database_path = temp.path().join("zuno.db");
    let pool = Pool::open(&DbLocation::File(database_path.clone())).expect("open file database");
    let mut connection = pool.get().expect("check out connection");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_a', ?1, 1, 1, '[]')",
            rusqlite::params![temp.path().join("checkout").to_string_lossy()],
        )
        .expect("insert project");
    insert_session(&connection, "ses_old", "prj_a", None, OLD, false);
    insert_session(&connection, "ses_keep", "prj_a", None, NEW, false);
    connection
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES ('msg_keep', 'ses_keep', 1, 1, '{}')",
            [],
        )
        .expect("insert surviving message");

    // The pruned session's object, named only as prose by a row that survives. A tool
    // result or a model turn can author exactly this.
    let digest = "5e".repeat(32);
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES ('prt_prose', 'msg_keep', 'ses_keep', 1, 1, ?1)",
            rusqlite::params![format!("the deleted session's file was sha256:{digest}")],
        )
        .expect("insert prose payload");

    let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
    let object = paths
        .attachments
        .join("v1")
        .join(zuno_attachment::AttachmentStore::database_identity(
            database_path.to_string_lossy().as_bytes(),
        ))
        .join("objects")
        .join(&digest[..2])
        .join(&digest);
    write(&object, b"object");

    let mut delete = request(SessionPruneAction::Delete);
    delete.confirm_delete = true;
    let report = execute(
        &mut connection,
        &paths,
        &delete,
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("delete with a held-back attachment class");

    assert_eq!(report.selected_session_ids, ["ses_old"]);
    assert!(
        object.is_file(),
        "a digest a surviving row names is never reclaimed"
    );
    assert_eq!(
        report.warnings,
        [format!(
            "`{}` kept 1 attachment object whose 1 digest surviving rows name only as free \
             text; model- or tool-authored content can produce that spelling, so those bytes \
             are not reclaimable while such a row survives.",
            database_path.display()
        )],
        "the suppression must be visible where an operator reads the report"
    );
    let serialized = String::from_utf8(to_json_bytes(&report).expect("serialize report"))
        .expect("report JSON is UTF-8");
    assert!(
        serialized.contains("only as free text"),
        "the JSON surface carries the same sentence: {serialized}"
    );
}

/// Preview must promise exactly what delete can deliver.
///
/// A preview builds its survivor set by removing the selection from the live rows, so a
/// preview that selects every session reaches the snapshot decision with the same empty set
/// the delete will see. Without that symmetry the preview enumerated every store under the
/// shared `$DATA/snapshot` — including another channel database's — as reclaimable bytes,
/// and the `--delete` that followed reclaimed none of them and said nothing.
#[test]
fn session_prune_preview_that_selects_every_session_promises_no_snapshot_reclamation() {
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let database_path = temp.path().join("opencode-local.db");
    let pool = Pool::open(&DbLocation::File(database_path.clone())).expect("open file database");
    let mut connection = pool.get().expect("check out connection");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('prj_only', '/srv/only', 1, 1, '[]');",
        )
        .expect("insert project");
    insert_session(&connection, "ses_only", "prj_only", None, OLD, false);
    let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
    let foreign = StoreKey::new("prj_other_channel", std::path::Path::new("/srv/other"));
    write(
        &foreign.path_in(&paths.snapshots).join("objects/history"),
        b"another channel's history",
    );
    let mut preview = request(SessionPruneAction::Preview);
    preview.scope = SessionPruneScope::AllProjects;

    let report = execute(
        &mut connection,
        &paths,
        &preview,
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("preview succeeds");

    assert_eq!(report.selected_session_ids, ["ses_only"]);
    assert!(
        !report
            .artifacts
            .items
            .iter()
            .any(|item| item.kind == "snapshot_store"),
        "a preview must not offer bytes the delete cannot reclaim: {:?}",
        report.artifacts.items
    );
    assert_eq!(
        report.warnings,
        [snapshot_skip_warning(&database_path)],
        "the preview names the class it did not evaluate"
    );
    assert!(
        foreign.path_in(&paths.snapshots).is_dir(),
        "a preview never touches the filesystem"
    );
    assert_eq!(session_count(&connection), 1, "a preview is inert");
}

#[test]
fn session_prune_delete_requires_confirmation_before_any_mutation() {
    let fixture = Fixture::new();
    let mut connection = fixture.connection();
    let before = session_count(&connection);

    let error = execute(
        &mut connection,
        &fixture.paths(),
        &request(SessionPruneAction::Delete),
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect_err("unconfirmed deletion is refused");

    assert!(error.to_string().contains("confirmation"));
    assert_eq!(session_count(&connection), before);
    assert!(
        fixture
            .paths()
            .tool_output
            .join("tool_ses_old_00000000000000000000000000000001")
            .is_file()
    );
}

#[test]
fn session_prune_archive_and_confirmed_delete_share_the_same_selection() {
    let fixture = Fixture::new();
    let mut connection = fixture.connection();
    let archive = execute(
        &mut connection,
        &fixture.paths(),
        &request(SessionPruneAction::Archive { at_ms: NOW }),
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("archive succeeds");
    assert_eq!(archive.selected_session_ids, ["ses_child", "ses_old"]);
    assert_eq!(archive.changed_sessions, 2);
    let archived: i64 = connection
        .query_row(
            "SELECT count(*) FROM session WHERE time_archived = ?1",
            [NOW],
            |row| row.get(0),
        )
        .expect("count archived sessions");
    assert_eq!(archived, 2);

    let mut delete = request(SessionPruneAction::Delete);
    delete.confirm_delete = true;
    let deleted = execute(
        &mut connection,
        &fixture.paths(),
        &delete,
        &Reachable,
        &Remote,
        &mut Progress::default(),
    )
    .expect("confirmed delete succeeds");
    assert_eq!(deleted.selected_session_ids, archive.selected_session_ids);
    assert_eq!(deleted.changed_sessions, 2);
    assert_eq!(
        session_count(&connection),
        2,
        "shared and other-project survive"
    );
    assert!(deleted.artifacts.items.iter().all(|item| item.removed));
}
