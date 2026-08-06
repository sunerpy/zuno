//! End-to-end tests against a real `git` and a real worktree.
//!
//! These drive the store the way the engine will: track a tree, let the "agent"
//! edit files, then diff and restore.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use oc_snapshot::{GcOutcome, Location, SessionRef, Store, reference_counts};

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

    let big = "x".repeat(usize::try_from(oc_snapshot::LARGE_FILE_LIMIT).expect("usize") + 1);
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
        oc_snapshot::LARGE_FILE_LIMIT + 1
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
    let hostile = "it's a $(touch pwned) \"work\" tree";
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
    assert_eq!(location.worktree, fixture.worktree);
    assert!(location.git);

    let store = Store::open(location);
    assert_eq!(
        store.git_dir(),
        fixture
            .root
            .join(&root_commit)
            .join(oc_paths::Layout::worktree_hash(&fixture.worktree))
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

    let dead = oc_snapshot::unreferenced_stores(
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
fn writable(original: &fs::Permissions) -> fs::Permissions {
    let mut relaxed = original.clone();
    relaxed.set_readonly(false);
    relaxed
}
