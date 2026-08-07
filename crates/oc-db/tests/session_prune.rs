use std::collections::BTreeSet;
use std::fs;

use oc_db::artifact_gc::ArtifactGcPaths;
use oc_db::prune::{RemoteUnshare, SharedSession, UnshareError};
use oc_db::retention::{DAY_MILLIS, Liveness, LivenessProbe, RetentionKey};
use oc_db::session_prune::{
    ProgressPhase, SessionPruneAction, SessionPruneProgress, SessionPruneProgressSink,
    SessionPruneRequest, SessionPruneScope, execute, to_json_bytes,
};
use oc_db::{Connection, Pool, migration};
use oc_paths::DbLocation;
use oc_snapshot::StoreKey;

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
        write(
            &paths.legacy_storage.join("session_diff/ses_old.json"),
            b"{}",
        );
        Self { pool, temp }
    }

    fn paths(&self) -> ArtifactGcPaths {
        ArtifactGcPaths::from_data_root(self.temp.path())
    }

    fn connection(&self) -> oc_db::PooledConnection<'_> {
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
    assert_eq!(report.artifacts.total_bytes, 12);
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
    let warning = format!(
        "`{}` contains 0 sessions; artifact reclamation is skipped because shared snapshot stores cannot be attributed and may belong to another channel's database.",
        database_path.display()
    );

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
    assert!(
        !fixture
            .paths()
            .legacy_storage
            .join("session_diff/ses_old.json")
            .exists()
    );
}
