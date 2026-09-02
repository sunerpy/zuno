//! The property the exclude design promises, checked against real git: a goal document
//! written into a fresh repository does not appear in `git status`.
//!
//! Nothing is stubbed. The block is written the way the host writes it, through
//! `zuno_paths::ensure_managed_block` with the registry's patterns; the document is
//! written by the real `GoalProjection`; and git itself is asked what it sees. The test
//! therefore fails if the registry stops naming the projection directory, if the block
//! stops being written or stops matching, or if the projection starts writing anywhere
//! the block does not cover — and it proves the block is the cause by removing it and
//! watching the directory reappear.

use std::path::Path;
use std::process::Command;

use zuno_goal::{GoalProjection, GoalStore, Ingest};
use zuno_paths::{
    ExcludeOutcome, IGNORE_PATTERNS, MANAGED_BLOCK_BEGIN, MANAGED_BLOCK_END, ensure_managed_block,
    resolve_exclude_path,
};

const SESSION: &str = "ses_generated_paths";

/// Run git in `cwd`, or `None` when git is not installed at all.
///
/// The null device stands in for the global and system configuration so a developer's
/// own `core.excludesFile` or `status.showUntrackedFiles` cannot make the tree look
/// clean, or dirty, for a reason this test is not about.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let output = match Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", null_device)
        .env("GIT_CONFIG_SYSTEM", null_device)
        .env("GIT_AUTHOR_NAME", "zuno-goal")
        .env("GIT_AUTHOR_EMAIL", "zuno-goal@example.test")
        .env("GIT_COMMITTER_NAME", "zuno-goal")
        .env("GIT_COMMITTER_EMAIL", "zuno-goal@example.test")
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("spawn git {args:?}: {error}"),
    };
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("git output is UTF-8"))
}

/// Skipped, loudly, when git is not installed: the projection's own behaviour is
/// covered by the unit tests in `projection_tests.rs`, and only git can say what git
/// reports.
#[test]
fn a_goal_document_written_into_a_fresh_repository_leaves_git_status_clean() {
    let worktree = tempfile::tempdir().expect("create worktree");
    let spill = tempfile::tempdir().expect("create spill directory");
    let root = worktree.path();
    if git(root, &["init", "--initial-branch=main", "."]).is_none() {
        eprintln!("skipping: git is not installed, so the exclusion cannot be checked against it");
        return;
    }
    let status = || {
        git(root, &["status", "--porcelain"]).expect("git is installed; the first call proved it")
    };
    std::fs::write(root.join("README.md"), "# fixture\n").expect("write a tracked file");
    git(root, &["add", "README.md"]).expect("git is installed");
    git(root, &["commit", "-qm", "initial"]).expect("git is installed");
    assert_eq!(
        status(),
        "",
        "the fixture must start clean, or the test proves nothing"
    );

    // The host writes the block once at startup, before any turn renders a document.
    let outcome = ensure_managed_block(root, IGNORE_PATTERNS).expect("write the exclude block");
    assert_eq!(outcome, ExcludeOutcome::Created);

    let store = GoalStore::open_memory(spill.path().to_owned()).expect("open goal store");
    let projection = GoalProjection::new(Some(root), SESSION)
        .expect("a plain session id resolves to a document path");
    let goal = store
        .create_goal(SESSION, "leave the tree clean", Some(10_000))
        .expect("create goal");
    projection.write(&goal).expect("render the projection");
    assert!(
        projection.path().is_file(),
        "the projection must actually have written {}",
        projection.path().display()
    );
    assert!(
        projection.path().starts_with(root),
        "a repository gets a project-local document, not the global fallback"
    );
    assert!(zuno_paths::is_generated(root, projection.path()));

    assert_eq!(
        status(),
        "",
        "a generated document must not appear as the user's uncommitted work"
    );

    // A document the projection cannot parse is preserved beside the original before
    // being rebuilt, so the directory now holds a second generated file.
    std::fs::write(projection.path(), "not a goal document\n").expect("corrupt the document");
    let ingest = projection
        .ingest(&store)
        .expect("ingest the corrupted document");
    let Ingest::Salvaged { backup } = &ingest else {
        panic!("expected the document to be salvaged, got {ingest:?}");
    };
    assert!(backup.is_file(), "{}", backup.display());
    assert!(zuno_paths::is_generated(root, backup));
    assert_eq!(status(), "", "the salvage copy is generated state too");

    // Remove only the managed block and git sees the directory again: the clean
    // status above came from the block, not from anything else about the fixture.
    let exclude = resolve_exclude_path(root).expect("resolve the exclude file");
    let content = std::fs::read_to_string(&exclude).expect("read the exclude file");
    let begin = content
        .find(MANAGED_BLOCK_BEGIN)
        .expect("the block is present");
    let end = content
        .find(MANAGED_BLOCK_END)
        .expect("the block is closed")
        + MANAGED_BLOCK_END.len();
    std::fs::write(
        &exclude,
        format!("{}{}", &content[..begin], &content[end..]),
    )
    .expect("remove the block");
    let dirty = status();
    assert!(
        dirty.contains(".zuno/"),
        "without the block git must report the generated directory, got {dirty:?}"
    );
}
