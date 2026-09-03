//! End-to-end tests against a real `git` and a real worktree.
//!
//! These drive the store the way the engine will: track a tree, let the "agent"
//! edit files, then diff and restore.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use zuno_snapshot::{
    FileOperation, GcOutcome, Location, SessionRef, SnapshotError, Store, TurnCheckpoint,
    TurnRestore, reference_counts,
};

/// A temp directory holding a worktree and a snapshot root as siblings, so the
/// store's own files can never appear as untracked worktree content.
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
}

impl Fixture {
    fn new(worktree_name: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("data").join("snapshot");
        let worktree = temp.path().join(worktree_name);
        fs::create_dir_all(&worktree).expect("create worktree");
        fs::create_dir_all(&root).expect("create snapshot root");

        git(&worktree, &["init", "-q", "."]);
        git(&worktree, &["config", "user.email", "test@example.com"]);
        git(&worktree, &["config", "user.name", "Test"]);
        write(&worktree.join("a.txt"), "hello\n");
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "init"]);

        Self {
            _temp: temp,
            root,
            worktree,
        }
    }

    fn store(&self) -> Store {
        Store::open(Location::new(&self.root, "proj", &self.worktree))
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.worktree.join(relative)
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).expect("read worktree file")
    }

    fn write(&self, relative: &str, contents: &str) {
        write(&self.path(relative), contents);
    }
}

/// Whether the store can still resolve `oid`.
///
/// `git gc` moves loose objects into packs — unreachable ones into a cruft pack —
/// so the loose file disappearing proves nothing. Only `cat-file -e` distinguishes
/// "reclaimed" from "repacked".
fn resolves(git_dir: &Path, oid: &str) -> bool {
    Command::new("git")
        .args([
            "--git-dir".as_ref(),
            git_dir.as_os_str(),
            "cat-file".as_ref(),
            "-e".as_ref(),
            oid.as_ref(),
        ])
        .output()
        .expect("spawn git cat-file")
        .status
        .success()
}

fn hash_object(fixture: &Fixture, git_dir: &Path, relative: &str, contents: &str) -> String {
    fixture.write(relative, contents);
    git(
        &fixture.worktree,
        &[
            "--git-dir",
            &git_dir.to_string_lossy(),
            "hash-object",
            "-w",
            "--",
            &fixture.path(relative).to_string_lossy(),
        ],
    )
    .trim()
    .to_owned()
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|error| panic!("spawn git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write file");
}

#[test]
fn tracks_mutates_diffs_and_restores() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();

    let hash = store
        .track()
        .expect("track")
        .expect("snapshots are enabled");
    assert_eq!(hash.len(), 40, "write-tree returns a tree hash: {hash}");
    assert!(store.exists(), "the store is created on first track");

    fixture.write("a.txt", "hello\nworld\n");
    fixture.write("nested/new.txt", "fresh\n");

    let diff = store.diff(&hash).expect("diff");
    assert!(diff.contains("diff --git a/a.txt b/a.txt"), "{diff}");
    assert!(diff.contains("+world"), "{diff}");
    assert!(diff.contains("nested/new.txt"), "{diff}");

    let patch = store.patch(&hash).expect("patch");
    assert_eq!(patch.hash, hash);
    let expected = fixture.path("a.txt").to_string_lossy().replace('\\', "/");
    assert!(patch.files.contains(&expected), "{:?}", patch.files);

    store.restore(&hash).expect("restore");
    assert_eq!(
        fixture.read("a.txt"),
        "hello\n",
        "restore returns tracked content to the snapshot"
    );

    let after = store.diff(&hash).expect("diff after restore");
    assert!(
        !after.contains("a.txt"),
        "the restored file no longer differs: {after}"
    );
}

#[test]
fn restore_keeps_files_created_after_the_snapshot() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let hash = store.track().expect("track").expect("enabled");

    fixture.write("added-later.txt", "new\n");
    store.restore(&hash).expect("restore");

    assert!(
        fixture.path("added-later.txt").exists(),
        "restore is read-tree + checkout-index; deleting extra files is revert's job"
    );
}

#[test]
fn a_second_track_records_the_mutation() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();

    let first = store.track().expect("track").expect("enabled");
    fixture.write("a.txt", "hello\nworld\n");
    let second = store.track().expect("track").expect("enabled");

    assert_ne!(first, second, "a changed worktree writes a different tree");
    store.restore(&first).expect("restore");
    assert_eq!(fixture.read("a.txt"), "hello\n");
    store.restore(&second).expect("restore");
    assert_eq!(fixture.read("a.txt"), "hello\nworld\n");
}

#[test]
fn a_turn_checkpoint_undoes_and_redoes_every_captured_file_with_a_complete_report() {
    let fixture = Fixture::new("wt");
    fixture.write("removed-by-turn.txt", "restore this exact content\n");
    let store = fixture.store();
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");

    fixture.write("a.txt", "changed by the turn\n");
    fixture.write("added-by-turn.txt", "new and still untracked by user git\n");
    fs::remove_file(fixture.path("removed-by-turn.txt")).expect("turn deletes file");
    let checkpoint = turn.finish().expect("finish turn");
    assert!(
        git(
            &fixture.worktree,
            &["status", "--short", "--", "added-by-turn.txt"]
        )
        .starts_with("??"),
        "the fixture must exercise a file that is untracked in the user's repository"
    );

    let undo = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect("undo exact post-turn tree");
    assert_eq!(undo.from(), checkpoint.after());
    assert_eq!(undo.to(), checkpoint.before());
    assert_eq!(
        undo.files()
            .iter()
            .map(|file| (file.path.as_str(), file.operation))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", FileOperation::Modified),
            ("added-by-turn.txt", FileOperation::Deleted),
            ("removed-by-turn.txt", FileOperation::Created),
        ]
    );
    assert_eq!(fixture.read("a.txt"), "hello\n");
    assert!(
        !fixture.path("added-by-turn.txt").exists(),
        "undo must remove a file created by the turn rather than merely restoring old files"
    );
    assert_eq!(
        fixture.read("removed-by-turn.txt"),
        "restore this exact content\n",
        "undo must recreate a file deleted by the turn with its original bytes"
    );

    let redo = store
        .restore_turn(&checkpoint, TurnRestore::Redo)
        .expect("redo exact pre-turn tree");
    assert_eq!(redo.from(), checkpoint.before());
    assert_eq!(redo.to(), checkpoint.after());
    assert_eq!(
        redo.files()
            .iter()
            .map(|file| (file.path.as_str(), file.operation))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", FileOperation::Modified),
            ("added-by-turn.txt", FileOperation::Created),
            ("removed-by-turn.txt", FileOperation::Deleted),
        ]
    );
    assert_eq!(fixture.read("a.txt"), "changed by the turn\n");
    assert_eq!(
        fixture.read("added-by-turn.txt"),
        "new and still untracked by user git\n"
    );
    assert!(
        !fixture.path("removed-by-turn.txt").exists(),
        "redo must delete the file again; an assertion that only read surviving files would miss this"
    );
}

#[test]
fn a_turn_checkpoint_is_serializable_for_session_owned_ordering() {
    let checkpoint = TurnCheckpoint::new("before-tree", "after-tree");
    let encoded = serde_json::to_string(&checkpoint).expect("serialize checkpoint");
    let decoded: TurnCheckpoint = serde_json::from_str(&encoded).expect("deserialize checkpoint");

    assert_eq!(decoded, checkpoint);
    assert_eq!(decoded.before(), "before-tree");
    assert_eq!(decoded.after(), "after-tree");
}

#[test]
fn undo_refuses_a_manually_drifted_file_without_touching_any_other_file() {
    let fixture = Fixture::new("wt");
    fixture.write("second.txt", "before second\n");
    let store = fixture.store();
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");
    fixture.write("a.txt", "after first\n");
    fixture.write("second.txt", "after second\n");
    let checkpoint = turn.finish().expect("finish turn");

    fixture.write("a.txt", "manual edit after the turn\n");
    let error = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect_err("manual drift must refuse undo");

    match error {
        SnapshotError::WorktreeDrift {
            expected,
            actual,
            files,
        } => {
            assert_eq!(expected, checkpoint.after());
            assert_ne!(actual, expected);
            assert_eq!(files, vec!["a.txt"]);
        }
        other => panic!("unexpected refusal: {other}"),
    }
    assert_eq!(fixture.read("a.txt"), "manual edit after the turn\n");
    assert_eq!(
        fixture.read("second.txt"),
        "after second\n",
        "the all-or-nothing preflight must not undo an unchanged sibling before reporting drift"
    );
}

#[test]
fn undo_refuses_a_file_deleted_after_the_checkpoint_and_does_not_recreate_it() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");
    fixture.write("a.txt", "after turn\n");
    let checkpoint = turn.finish().expect("finish turn");

    fs::remove_file(fixture.path("a.txt")).expect("manual delete after turn");
    let error = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect_err("a missing expected file must refuse undo");

    assert!(
        matches!(
            error,
            SnapshotError::WorktreeDrift { ref files, .. } if files == &["a.txt"]
        ),
        "unexpected refusal: {error}"
    );
    assert!(
        !fixture.path("a.txt").exists(),
        "refusal must not silently recreate a file the user deleted after the turn"
    );
}

#[test]
fn undo_refuses_an_affected_file_that_became_gitignored() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");
    fixture.write("generated.txt", "created by turn\n");
    let checkpoint = turn.finish().expect("finish turn");

    fixture.write(".git/info/exclude", "generated.txt\n");
    let error = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect_err("an affected path becoming ignored must refuse undo");

    assert!(
        matches!(
            error,
            SnapshotError::IgnoredFiles { ref files } if files == &["generated.txt"]
        ),
        "unexpected refusal: {error}"
    );
    assert_eq!(
        fixture.read("generated.txt"),
        "created by turn\n",
        "the ignored file must survive the refusal byte-for-byte"
    );
}

#[test]
fn a_disabled_store_creates_no_turn_checkpoint() {
    let fixture = Fixture::new("wt");
    let store =
        Store::open(Location::new(&fixture.root, "proj", &fixture.worktree).with_enabled(false));

    assert!(
        store
            .begin_turn()
            .expect("disabled begin is inert")
            .is_none(),
        "a caller must not receive a checkpoint it could later mistake for restorable state"
    );
    assert!(!store.exists(), "an inert capture must not create a store");
}

#[test]
fn diff_is_empty_when_the_worktree_is_unchanged() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let hash = store.track().expect("track").expect("enabled");

    assert_eq!(store.diff(&hash).expect("diff"), "");
    assert_eq!(
        store.patch(&hash).expect("patch").files,
        Vec::<String>::new()
    );
}

#[test]
fn gitignored_files_stay_out_of_the_snapshot() {
    let fixture = Fixture::new("wt");
    fixture.write(".gitignore", "secret.txt\n");
    git(&fixture.worktree, &["add", "-A"]);
    git(&fixture.worktree, &["commit", "-qm", "ignore"]);

    let store = fixture.store();
    let hash = store.track().expect("track").expect("enabled");
    fixture.write("secret.txt", "do not store me\n");
    fixture.write("kept.txt", "store me\n");

    let patch = store.patch(&hash).expect("patch");
    assert!(
        patch.files.iter().any(|file| file.ends_with("kept.txt")),
        "{:?}",
        patch.files
    );
    assert!(
        !patch.files.iter().any(|file| file.ends_with("secret.txt")),
        "{:?}",
        patch.files
    );

    let listed = git(
        &fixture.worktree,
        &[
            "--git-dir",
            &store.git_dir().to_string_lossy(),
            "ls-files",
            "--cached",
        ],
    );
    assert!(!listed.contains("secret.txt"), "{listed}");
}

#[test]
fn large_untracked_files_are_excluded_instead_of_stored() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    store.track().expect("track").expect("enabled");

    let big = "x".repeat(usize::try_from(zuno_snapshot::LARGE_FILE_LIMIT).expect("usize") + 1);
    fixture.write("huge.bin", &big);
    fixture.write("small.txt", "tiny\n");
    store.track().expect("track").expect("enabled");

    let listed = git(
        &fixture.worktree,
        &[
            "--git-dir",
            &store.git_dir().to_string_lossy(),
            "ls-files",
            "--cached",
        ],
    );
    assert!(listed.contains("small.txt"), "{listed}");
    assert!(
        !listed.contains("huge.bin"),
        "a {} byte untracked file is excluded: {listed}",
        zuno_snapshot::LARGE_FILE_LIMIT + 1
    );

    let excludes = fs::read_to_string(store.git_dir().join("info").join("exclude"))
        .expect("read store excludes");
    assert!(excludes.contains("/huge.bin"), "{excludes}");
}

#[test]
fn gc_prunes_unreachable_objects_and_never_removes_the_store() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let tracked = store.track().expect("track").expect("enabled");
    let git_dir = store.git_dir().to_path_buf();

    let object = hash_object(
        &fixture,
        &git_dir,
        "loose-source.txt",
        "unreachable-object-for-gc\n",
    );
    let loose = git_dir
        .join("objects")
        .join(&object[..2])
        .join(&object[2..]);
    assert!(loose.is_file(), "{}", loose.display());
    assert!(resolves(&git_dir, &object));

    // `--prune=7.days` only reclaims objects older than a week, so the object has
    // to be backdated for the sweep to be observable inside one test run.
    backdate(&loose, Duration::from_secs(30 * 24 * 60 * 60));

    assert_eq!(store.gc().expect("gc"), GcOutcome::Collected);

    assert!(
        !resolves(&git_dir, &object),
        "the unreachable object is reclaimed"
    );
    assert!(git_dir.is_dir(), "gc must never remove the store directory");
    assert!(git_dir.join("objects").is_dir());
    assert!(git_dir.join("config").is_file());
    assert!(git_dir.join("index").is_file());
    assert!(
        resolves(&git_dir, &tracked),
        "the tracked snapshot still resolves after gc"
    );
    assert_eq!(
        store
            .track()
            .expect("the store is still usable after gc")
            .expect("enabled")
            .len(),
        40
    );
}

#[test]
fn gc_keeps_an_unreachable_object_inside_the_prune_window() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    store.track().expect("track").expect("enabled");

    let object = hash_object(
        &fixture,
        store.git_dir(),
        "recent.txt",
        "recent-unreachable-object\n",
    );

    assert_eq!(store.gc().expect("gc"), GcOutcome::Collected);
    assert!(
        resolves(store.git_dir(), &object),
        "an object younger than the 7 day window survives, repacked but retrievable"
    );
}

/// The revert horizon, made executable.
///
/// The *latest* tree stays reachable through the index's cache-tree, but a tree
/// that a later `track()` superseded is unreachable, so the hourly sweep reclaims
/// it once it passes the seven-day window. Todos 82 and 83 must treat an old
/// snapshot hash as possibly unresolvable rather than assume it lasts forever.
#[test]
fn gc_reclaims_a_snapshot_superseded_more_than_the_prune_window_ago() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let git_dir = store.git_dir().to_path_buf();

    // Both trees must be content the source repository has never committed,
    // otherwise `write-tree` resolves them through `objects/info/alternates` and
    // no object is written into the store to reclaim.
    //
    // Each revision must also differ in *length* from the one before it, not only
    // in bytes. `Store::seed` copies the source repository's index, whose cached
    // mtime has one-second granularity (git is built without `USE_NSEC`), so an
    // edit made in the same second as the commit is indistinguishable by stat
    // alone. `git add -A` then trusts the stale entry, `write-tree` hands back the
    // *source's* tree, and the assertions below fail because that tree lives in
    // the alternate rather than in the store. Differing sizes make `ce_match_stat`
    // notice unconditionally, which is what makes this test deterministic under
    // load — the second-boundary crossing that defeats git's racily-clean
    // fallback is otherwise pure timing.
    fixture.write("a.txt", "first revision\n");
    let old = store.track().expect("track").expect("enabled");
    fixture.write("a.txt", "second\n");
    let latest = store.track().expect("track").expect("enabled");
    assert_ne!(old, latest);

    let loose = git_dir.join("objects").join(&old[..2]).join(&old[2..]);
    assert!(
        loose.is_file(),
        "the superseded tree is local to the store: {}",
        loose.display()
    );
    backdate(&loose, Duration::from_secs(30 * 24 * 60 * 60));
    assert_eq!(store.gc().expect("gc"), GcOutcome::Collected);

    assert!(
        !resolves(&git_dir, &old),
        "a superseded snapshot past the prune window is reclaimed"
    );
    assert!(
        resolves(&git_dir, &latest),
        "the latest snapshot is reachable through the index and always survives"
    );
    assert!(git_dir.is_dir(), "the store directory is untouched");
}

#[test]
fn a_hostile_worktree_path_is_not_interpreted_by_a_shell() {
    #[cfg(unix)]
    let hostile = "it's a $(touch pwned) \"work\" tree";
    // Quotes are not legal Windows filename characters. Command substitution,
    // spaces, apostrophes and parentheses remain hostile shell input while also
    // naming a real NTFS directory.
    #[cfg(windows)]
    let hostile = "it's a $(touch pwned) work tree";
    let fixture = Fixture::new(hostile);
    let canary = fixture.worktree.parent().expect("parent").join("pwned");
    let store = fixture.store();

    let hash = store.track().expect("track").expect("enabled");

    // A filename is a second injection surface: it reaches git on stdin as a
    // NUL-separated `:(top,literal)` pathspec.
    let nasty = "a file's; rm -rf $HOME `id`.txt";
    fixture.write(nasty, "payload\n");
    fixture.write("a.txt", "hello\nworld\n");

    let patch = store.patch(&hash).expect("patch");
    assert!(
        patch.files.iter().any(|file| file.ends_with(nasty)),
        "the hostile filename is snapshotted as itself: {:?}",
        patch.files
    );
    let diff = store.diff(&hash).expect("diff");
    assert!(diff.contains("+world"), "{diff}");

    store.track().expect("track").expect("enabled");
    fixture.write("a.txt", "clobbered\n");
    store.restore(&hash).expect("restore");
    assert_eq!(fixture.read("a.txt"), "hello\n");
    assert_eq!(fixture.read(nasty), "payload\n");

    assert!(!canary.exists(), "no command substitution ran");
    assert!(!fixture.path("pwned").exists());
    assert!(
        store.git_dir().starts_with(&fixture.root),
        "the store stays under the snapshot root: {}",
        store.git_dir().display()
    );
}

#[test]
fn one_store_serves_every_session_in_a_worktree() {
    let fixture = Fixture::new("wt");
    let first = fixture.store();
    let second = Store::open(
        Location::new(&fixture.root, "proj", &fixture.worktree)
            .with_directory(fixture.path("nested")),
    );
    fs::create_dir_all(fixture.path("nested")).expect("create nested");

    assert_eq!(
        first.git_dir(),
        second.git_dir(),
        "two sessions in one worktree share one store"
    );

    let hash = first.track().expect("track").expect("enabled");
    fixture.write("nested/from-second.txt", "second session\n");
    let scoped = second.patch(&hash).expect("patch");
    assert!(
        scoped
            .files
            .iter()
            .any(|file| file.ends_with("nested/from-second.txt")),
        "{:?}",
        scoped.files
    );

    let counts = reference_counts(
        &fixture.root,
        vec![
            SessionRef::new("ses_one", "proj", &fixture.worktree),
            SessionRef::new("ses_two", "proj", &fixture.worktree),
        ],
    )
    .expect("reference counts");

    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].count(), 2);
    assert_eq!(counts[0].path, first.git_dir());
    assert!(counts[0].on_disk);
}

#[test]
fn discovery_keys_the_store_on_the_projects_root_commit() {
    let fixture = Fixture::new("wt");
    let location = Location::discover_in(&fixture.root, &fixture.worktree);
    let root_commit = git(&fixture.worktree, &["rev-list", "--max-parents=0", "HEAD"])
        .trim()
        .to_owned();

    assert_eq!(
        location.project_id, root_commit,
        "a remote-less repository is keyed on its root commit"
    );
    assert_eq!(
        location
            .worktree
            .canonicalize()
            .expect("canonical discovered worktree"),
        fixture
            .worktree
            .canonicalize()
            .expect("canonical fixture worktree")
    );
    assert!(location.git);

    let worktree_hash = zuno_paths::Layout::worktree_hash(&location.worktree);
    let store = Store::open(location);
    assert_eq!(
        store.git_dir(),
        fixture.root.join(&root_commit).join(worktree_hash)
    );

    let hash = store.track().expect("track").expect("enabled");
    assert_eq!(hash.len(), 40);
}

#[test]
fn a_store_is_reference_counted_across_projects_without_being_touched() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    store.track().expect("track").expect("enabled");

    let orphan = Store::open(Location::new(&fixture.root, "gone", &fixture.worktree));
    fs::create_dir_all(orphan.git_dir()).expect("create orphan store");

    let dead = zuno_snapshot::unreferenced_stores(
        &fixture.root,
        vec![SessionRef::new("ses_one", "proj", &fixture.worktree)],
    )
    .expect("query");

    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].path, orphan.git_dir());
    assert!(
        orphan.git_dir().is_dir(),
        "the query reports, it does not delete"
    );
    assert!(store.git_dir().is_dir());
}

/// Move a file's modification time `age` into the past.
///
/// `git prune` decides what to reclaim from the object file's mtime, so this is
/// the only way to observe a `--prune=7.days` sweep inside one test run. Loose
/// objects are written read-only, hence the permission dance.
fn backdate(path: &Path, age: Duration) {
    let original = fs::metadata(path).expect("metadata").permissions();
    fs::set_permissions(path, writable(&original)).expect("relax permissions");

    let when = SystemTime::now()
        .checked_sub(age)
        .expect("a representable timestamp");
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open for set_modified")
        .set_modified(when)
        .expect("set mtime");

    fs::set_permissions(path, original).expect("restore permissions");
}

#[cfg(unix)]
fn writable(original: &fs::Permissions) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt as _;

    let mut relaxed = original.clone();
    relaxed.set_mode(original.mode() | 0o200);
    relaxed
}

#[cfg(not(unix))]
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "this branch has no Unix mode bits; clearing the platform read-only attribute is the portable inverse"
)]
fn writable(original: &fs::Permissions) -> fs::Permissions {
    let mut relaxed = original.clone();
    relaxed.set_readonly(false);
    relaxed
}

// -- unreadable paths must not silently rewind a checkpoint (F1) ---------------

/// Set a Unix mode, returning the previous permissions so a test can put them back.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt as _;

    let previous = fs::metadata(path).expect("metadata").permissions();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
    previous
}

/// A file `git add` cannot open aborts the whole staging pass, so tolerating the
/// failure did not cost one file — it silently rewound the entire checkpoint to the
/// previous tree while reporting success.
///
/// Unix-only because the reproduction needs a file whose *content* is unreadable
/// while its directory entry stays visible; Windows has no portable equivalent of
/// mode 000 through `std::fs`.
#[cfg(unix)]
#[test]
fn an_unreadable_file_is_skipped_and_named_instead_of_rewinding_the_capture() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let baseline = store.track().expect("track").expect("enabled");

    fixture.write("a.txt", "hello\nworld\n");
    fixture.write("locked.txt", "unreadable\n");
    set_mode(&fixture.path("locked.txt"), 0o000);

    let capture = store
        .capture()
        .expect("an unreadable path must not fail the capture")
        .expect("enabled");
    set_mode(&fixture.path("locked.txt"), 0o644);

    assert_ne!(
        capture.tree(),
        baseline,
        "the edit to a.txt must reach the tree; equality here is the silent rewind"
    );
    assert_eq!(
        capture.exclusions().unreadable(),
        ["locked.txt"],
        "the skipped path must be named so a client can surface it"
    );
    assert!(capture.exclusions().oversized().is_empty());
    assert!(capture.exclusions().ignored().is_empty());

    let listed = git(
        &fixture.worktree,
        &[
            "--git-dir",
            &store.git_dir().to_string_lossy(),
            "ls-tree",
            "-r",
            "--name-only",
            capture.tree(),
        ],
    );
    assert!(listed.contains("a.txt"), "{listed}");
    assert!(
        !listed.contains("locked.txt"),
        "the unreadable path is absent, not stale: {listed}"
    );
    assert_eq!(
        git(
            &fixture.worktree,
            &[
                "--git-dir",
                &store.git_dir().to_string_lossy(),
                "cat-file",
                "-p",
                &format!("{}:a.txt", capture.tree()),
            ]
        ),
        "hello\nworld\n",
        "the capture must hold the post-edit content"
    );
}

/// An unreadable path is tolerated; a broken `git add` is not. `index.lock` reproduces
/// a real hard failure — a second process holding the store's index — which exits 128
/// and stages nothing at all.
#[test]
fn a_hard_git_add_failure_fails_the_capture_instead_of_returning_a_stale_tree() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let baseline = store.track().expect("track").expect("enabled");

    fixture.write("a.txt", "hello\nworld\n");
    let lock = store.git_dir().join("index.lock");
    fs::write(&lock, "").expect("create index.lock");

    let error = store
        .track()
        .expect_err("a contended index must not be reported as a successful capture");
    fs::remove_file(&lock).expect("release index.lock");

    match error {
        SnapshotError::Git { ref args, code, .. } => {
            assert!(args.contains(&"add".to_owned()), "{args:?}");
            assert_eq!(code, Some(128), "git refuses a held index.lock immediately");
        }
        other => panic!("unexpected failure: {other}"),
    }
    assert!(
        error.worktree_untouched(),
        "a failed capture cannot have modified the worktree"
    );
    assert_eq!(
        store.track().expect("track").expect("enabled").len(),
        40,
        "the store stays usable once the lock is gone"
    );
    assert_ne!(
        store.track().expect("track").expect("enabled"),
        baseline,
        "the recovered capture records the edit"
    );
}

/// A failure to *list* the worktree is a failure to know what to capture, so it must
/// not degrade into "nothing changed".
#[test]
fn a_failed_worktree_listing_fails_the_capture() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    store.track().expect("track").expect("enabled");

    fixture.write("a.txt", "hello\nworld\n");
    fs::write(store.git_dir().join("index"), b"not an index").expect("corrupt the store index");

    let error = store
        .track()
        .expect_err("an unreadable index must not be reported as an empty change set");
    match error {
        SnapshotError::Git { ref args, .. } => assert!(
            args.contains(&"diff-files".to_owned()) || args.contains(&"ls-files".to_owned()),
            "{args:?}"
        ),
        other => panic!("unexpected failure: {other}"),
    }
}

// -- a partly applied restore is uncertain, never refused (F2) -----------------

/// `git apply --index --check` does not test whether the target paths are writable,
/// so a patch that passes the preflight can still die half-written. The worktree is
/// then a mixture of both boundaries, which is an uncertain outcome — never a
/// refusal, and never something to replay.
#[cfg(unix)]
#[test]
fn a_partly_applied_undo_is_persisted_as_uncertain_and_blocks_further_restores() {
    let fixture = Fixture::new("wt");
    fixture.write("zz/z.txt", "before z\n");
    let store = fixture.store();
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");
    fixture.write("a.txt", "after a\n");
    fixture.write("zz/z.txt", "after z\n");
    let checkpoint = turn.finish().expect("finish turn");

    // `a.txt` sorts before `zz/z.txt`, so the patch rewrites it and then cannot
    // replace the file inside the read-only directory.
    let previous = set_mode(&fixture.path("zz"), 0o555);
    let error = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect_err("a half-written apply must not be reported as success");
    fs::set_permissions(fixture.path("zz"), previous).expect("restore directory permissions");

    assert!(
        !error.worktree_untouched(),
        "the honest disposition is uncertainty, not refusal: {error}"
    );
    let evidence = match error {
        SnapshotError::RestoreUncertain {
            restore,
            ref expected,
            ref actual,
            ref evidence,
            ..
        } => {
            assert_eq!(restore, TurnRestore::Undo);
            assert_eq!(expected, checkpoint.before());
            let observed = actual.as_deref().expect("the mixed tree is observable");
            assert_ne!(observed, checkpoint.before());
            assert_ne!(observed, checkpoint.after());
            evidence.clone()
        }
        other => panic!("unexpected failure: {other}"),
    };
    assert_eq!(
        fixture.read("a.txt"),
        "hello\n",
        "the first file was already rewritten, which is why this is not a refusal"
    );
    assert_eq!(
        fixture.read("zz/z.txt"),
        "after z\n",
        "the second file kept its post-turn content"
    );

    let record = store
        .uncertain_restore()
        .expect("read evidence")
        .expect("an uncertain outcome is persisted");
    assert_eq!(record.restore, TurnRestore::Undo);
    assert_eq!(record.from, checkpoint.after());
    assert_eq!(record.to, checkpoint.before());
    assert!(record.observed.is_some());
    assert!(record.cause.contains("apply"), "{}", record.cause);
    assert!(
        evidence.is_file() && evidence.starts_with(store.git_dir()),
        "evidence lives inside the store, not the user's repository: {}",
        evidence.display()
    );

    let refused = store
        .restore_turn(&checkpoint, TurnRestore::Redo)
        .expect_err("an unresolved uncertain outcome must block further restores");
    assert!(
        matches!(refused, SnapshotError::RestoreUnresolved { .. }),
        "unexpected failure: {refused}"
    );
    assert!(
        refused.worktree_untouched(),
        "refusing on account of an earlier incident touches nothing"
    );

    assert!(
        store.resolve_uncertain_restore().expect("resolve"),
        "inspection is explicit, and clears exactly one record"
    );
    assert!(!store.resolve_uncertain_restore().expect("resolve"));
    assert!(store.uncertain_restore().expect("read").is_none());
    assert!(!evidence.exists());
}

/// Recovery evidence that cannot be decoded is not the same as no incident.
#[test]
fn an_undecodable_uncertain_record_fails_closed() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let turn = store.begin_turn().expect("begin").expect("enabled");
    fixture.write("a.txt", "after\n");
    let checkpoint = turn.finish().expect("finish");

    fs::write(store.uncertain_path(), b"{ not json").expect("write corrupt evidence");

    let error = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect_err("a corrupt record must not be ignored");
    assert!(
        matches!(
            error,
            SnapshotError::Store {
                operation: "parse",
                ..
            }
        ),
        "unexpected failure: {error}"
    );
    assert_eq!(
        fixture.read("a.txt"),
        "after\n",
        "failing closed leaves the worktree alone"
    );
}

// -- capture covers the whole worktree, not the startup directory (F4) ---------

/// A session started in a subdirectory still captures the whole worktree, because
/// `write-tree` has no pathspec and restoration diffs and applies across the whole
/// worktree: a subdirectory-scoped staging pass produced a whole-worktree tree that
/// omitted every change outside it.
#[test]
fn a_session_started_in_a_subdirectory_still_captures_the_whole_worktree() {
    let fixture = Fixture::new("wt");
    fs::create_dir_all(fixture.path("nested")).expect("create nested");
    fixture.write("nested/inside.txt", "before inside\n");
    fixture.write("outside.txt", "before outside\n");
    let store = Store::open(
        Location::new(&fixture.root, "proj", &fixture.worktree)
            .with_directory(fixture.path("nested")),
    );
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");

    fixture.write("nested/inside.txt", "after inside\n");
    fixture.write("outside.txt", "after outside\n");
    let checkpoint = turn.finish().expect("finish turn");

    let undo = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect("undo");
    assert_eq!(
        undo.files()
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["nested/inside.txt", "outside.txt"],
        "a change above the startup directory must be part of the checkpoint"
    );
    assert_eq!(fixture.read("outside.txt"), "before outside\n");
    assert_eq!(fixture.read("nested/inside.txt"), "before inside\n");
}

// -- a failed turn still needs its checkpoint (F5) -----------------------------

/// Whether a turn succeeded is independent of whether it wrote files, so the
/// checkpoint has to be taken for a failed turn too.
#[test]
fn a_turn_that_ends_in_failure_still_produces_a_usable_checkpoint() {
    let fixture = Fixture::new("wt");
    let store = fixture.store();
    let turn = store
        .begin_turn()
        .expect("begin turn")
        .expect("snapshots are enabled");

    fixture.write("a.txt", "written before the turn failed\n");
    let failure: Result<(), String> = Err("the provider stream died".to_owned());
    let (checkpoint, outcome) = turn.finish_with(failure);

    assert_eq!(
        outcome,
        Err("the provider stream died".to_owned()),
        "the turn's own verdict is handed back untouched"
    );
    let checkpoint = checkpoint.expect("a failed turn still has a post-turn tree");
    assert_ne!(checkpoint.before(), checkpoint.after());

    let undo = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect("undo a failed turn");
    assert_eq!(
        undo.files()
            .iter()
            .map(|file| (file.path.as_str(), file.operation))
            .collect::<Vec<_>>(),
        vec![("a.txt", FileOperation::Modified)]
    );
    assert_eq!(fixture.read("a.txt"), "hello\n");
}

// -- success and exclusions are reportable (F3, F6) ---------------------------

/// A successful restore carries its own unambiguous sentence, so no client has to
/// invent success wording or publish it as a discardable advisory detail.
#[test]
fn a_successful_restore_reports_itself_unambiguously() {
    let fixture = Fixture::new("wt");
    fixture.write("gone.txt", "removed by the turn\n");
    let store = fixture.store();
    let turn = store.begin_turn().expect("begin").expect("enabled");
    fixture.write("a.txt", "changed\n");
    fixture.write("added.txt", "new\n");
    fs::remove_file(fixture.path("gone.txt")).expect("delete");
    let checkpoint = turn.finish().expect("finish");

    let undo = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect("undo");
    assert_eq!(undo.counts(), (1, 1, 1));
    let summary = undo.summary();
    assert!(
        summary.starts_with("undo complete: 3 file(s) restored"),
        "{summary}"
    );
    assert!(
        summary.contains("1 created, 1 modified, 1 deleted"),
        "{summary}"
    );
    assert!(
        !summary.starts_with("warning"),
        "success must not be dressed as a warning to survive a filter: {summary}"
    );
    assert_eq!(summary, undo.to_string());
    assert_eq!(TurnRestore::Undo.to_string(), "undo");
    assert_eq!(TurnRestore::Redo.to_string(), "redo");

    let redo = store
        .restore_turn(&checkpoint, TurnRestore::Redo)
        .expect("redo");
    assert!(
        redo.summary()
            .starts_with("redo complete: 3 file(s) restored")
    );
}

/// The two silent exclusions — an oversized untracked file and a path the repository
/// ignores — stay excluded, but stop being silent.
///
/// `ignored.txt` is edited *before* the turn opens, so the drop happens during the
/// pre-turn capture and the path is absent from both boundaries. That is the case a
/// restore is allowed to proceed through; a path that is present in one boundary and
/// then becomes ignored is a refusal instead
/// (see `undo_refuses_an_affected_file_that_became_gitignored`).
#[test]
fn every_excluded_path_is_reported_through_the_checkpoint_and_the_restore_report() {
    let fixture = Fixture::new("wt");
    fixture.write(".gitignore", "ignored.txt\n");
    fixture.write("ignored.txt", "tracked before it was ignored\n");
    git(&fixture.worktree, &["add", "-Af"]);
    git(&fixture.worktree, &["commit", "-qm", "ignore"]);
    fixture.write("ignored.txt", "edited while ignored\n");

    let store = fixture.store();
    let turn = store.begin_turn().expect("begin").expect("enabled");
    assert_eq!(
        turn.exclusions().ignored(),
        ["ignored.txt"],
        "the pre-turn capture drops the newly-ignored path and says so"
    );
    fixture.write("a.txt", "changed\n");
    fixture.write(
        "huge.bin",
        &"x".repeat(usize::try_from(zuno_snapshot::LARGE_FILE_LIMIT).expect("usize") + 1),
    );
    let checkpoint = turn.finish().expect("finish");

    let exclusions = checkpoint.exclusions();
    assert_eq!(exclusions.oversized(), ["huge.bin"]);
    assert_eq!(exclusions.ignored(), ["ignored.txt"]);
    assert_eq!(exclusions.paths(), vec!["huge.bin", "ignored.txt"]);
    assert!(!exclusions.is_empty());
    let summary = exclusions
        .summary()
        .expect("a non-empty exclusion set reads");
    assert!(
        summary.contains("2 path(s) are outside this snapshot and were not restored"),
        "{summary}"
    );
    assert!(
        summary.contains("1 over the 2 MiB untracked-file limit"),
        "{summary}"
    );
    assert!(summary.contains("1 matching an ignore rule"), "{summary}");

    let undo = store
        .restore_turn(&checkpoint, TurnRestore::Undo)
        .expect("undo");
    assert_eq!(
        undo.exclusions(),
        exclusions,
        "the restore report carries the checkpoint's exclusions"
    );
    assert!(
        undo.summary().contains("outside this snapshot"),
        "a client that prints the summary tells the user what undo did not cover: {}",
        undo.summary()
    );
    assert_eq!(
        fixture.read("ignored.txt"),
        "edited while ignored\n",
        "an excluded path keeps whatever content it had"
    );
    assert!(
        fixture.path("huge.bin").is_file(),
        "an excluded oversized file is left alone, not deleted"
    );
    assert_eq!(fixture.read("a.txt"), "hello\n");
}

/// A checkpoint stored before exclusions were recorded still decodes.
#[test]
fn a_checkpoint_without_recorded_exclusions_still_decodes() {
    let decoded: TurnCheckpoint =
        serde_json::from_str(r#"{"before":"tree-a","after":"tree-b"}"#).expect("decode");
    assert_eq!(decoded, TurnCheckpoint::new("tree-a", "tree-b"));
    assert!(decoded.exclusions().is_empty());
}

/// Every pre-application refusal must remain honest about having touched nothing.
#[test]
fn a_refusal_before_the_patch_is_applied_reports_an_untouched_worktree() {
    for error in [
        SnapshotError::SnapshotsDisabled,
        SnapshotError::WorktreeDrift {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
            files: vec!["a.txt".to_owned()],
        },
        SnapshotError::IgnoredFiles {
            files: vec!["a.txt".to_owned()],
        },
        SnapshotError::RestoreVerification {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        },
    ] {
        assert!(error.worktree_untouched(), "{error}");
    }
}
