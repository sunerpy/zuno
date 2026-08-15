//! The plan document's naming and both of its locations.
//!
//! The load-bearing assertions are [`the_file_name_is_created_dash_slug_dot_md`]
//! and [`both_locations_are_reachable_and_distinct`]: together they pin the exact
//! contract `session/session.ts:331-335` states, which is the whole reason this
//! module exists as its own file rather than as a helper inside a session type.

use super::*;

const CREATED: i64 = 1_764_000_000_000;

fn key() -> PlanKey<'static> {
    PlanKey {
        created: CREATED,
        slug: "swift-otter",
    }
}

#[test]
fn the_file_name_is_created_dash_slug_dot_md() {
    assert_eq!(
        key().file_name().expect("a plain slug resolves"),
        "1764000000000-swift-otter.md"
    );
}

#[test]
fn both_locations_are_reachable_and_distinct() {
    let worktree = Path::new("/tmp/oc-t79-repo");
    let local = plan_path(PlanLocation::Worktree(worktree), key()).expect("worktree path");
    assert_eq!(
        local,
        worktree
            .join(".zuno")
            .join("plans")
            .join("1764000000000-swift-otter.md")
    );

    let global = plan_path(PlanLocation::Global, key()).expect("global path");
    assert_eq!(
        global.parent(),
        Some(oc_paths::data().join("plans").as_path())
    );
    assert!(global.ends_with("plans/1764000000000-swift-otter.md"));
    assert!(
        !global.starts_with(worktree),
        "the fallback must not land inside the worktree: {global:?}"
    );
    assert_ne!(local, global);
}

#[test]
fn the_two_locations_agree_on_the_directory_name_but_not_the_root() {
    let worktree = Path::new("/tmp/oc-t79-repo");
    let local = PlanLocation::Worktree(worktree).directory();
    let global = PlanLocation::Global.directory();
    assert_eq!(local.file_name(), global.file_name());
    assert_eq!(
        local.file_name().and_then(|name| name.to_str()),
        Some("plans")
    );
    assert_eq!(
        local
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some(".zuno"),
        "the project-local location nests plans under .zuno"
    );
    assert_ne!(
        global
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some(".zuno"),
        "the global location is not a project directory"
    );
}

#[test]
fn a_slug_that_would_escape_the_directory_is_refused() {
    for hostile in [
        "../../etc/passwd",
        "a/b",
        "..",
        ".",
        "",
        "with/slash",
        "/absolute",
        " leading",
        "trailing ",
    ] {
        let key = PlanKey {
            created: CREATED,
            slug: hostile,
        };
        assert_eq!(
            key.file_name(),
            None,
            "{hostile:?} must not become a plan file name"
        );
        assert_eq!(
            plan_path(PlanLocation::Worktree(Path::new("/tmp/oc-t79-repo")), key),
            None,
            "{hostile:?} must not resolve to a plan path"
        );
        let error = write_plan(PlanLocation::Global, key, "body").expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[test]
fn writing_creates_the_directory_and_leaves_no_temporary_behind() {
    let root = tempfile::tempdir().expect("temporary worktree");
    let path = write_plan(
        PlanLocation::Worktree(root.path()),
        key(),
        "# Plan\n\nstep one\n",
    )
    .expect("write");

    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "# Plan\n\nstep one\n"
    );
    assert_eq!(
        path,
        root.path()
            .join(".zuno")
            .join("plans")
            .join("1764000000000-swift-otter.md")
    );

    let leftovers: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("list")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn a_rewrite_replaces_the_document_rather_than_appending() {
    let root = tempfile::tempdir().expect("temporary worktree");
    write_plan(PlanLocation::Worktree(root.path()), key(), "first\n").expect("first write");
    let path =
        write_plan(PlanLocation::Worktree(root.path()), key(), "second\n").expect("second write");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "second\n"
    );
}

#[test]
fn reading_a_missing_plan_is_not_an_error() {
    let root = tempfile::tempdir().expect("temporary worktree");
    assert_eq!(
        read_plan(PlanLocation::Worktree(root.path()), key()).expect("read"),
        None
    );
    write_plan(PlanLocation::Worktree(root.path()), key(), "body\n").expect("write");
    assert_eq!(
        read_plan(PlanLocation::Worktree(root.path()), key()).expect("read"),
        Some("body\n".to_owned())
    );
}

#[test]
fn two_sessions_in_the_same_millisecond_get_different_documents() {
    let root = tempfile::tempdir().expect("temporary worktree");
    let first = write_plan(
        PlanLocation::Worktree(root.path()),
        PlanKey {
            created: CREATED,
            slug: "swift-otter",
        },
        "first\n",
    )
    .expect("first");
    let second = write_plan(
        PlanLocation::Worktree(root.path()),
        PlanKey {
            created: CREATED,
            slug: "quiet-harbor",
        },
        "second\n",
    )
    .expect("second");
    assert_ne!(first, second);
    assert_eq!(std::fs::read_to_string(&first).expect("first"), "first\n");
    assert_eq!(
        std::fs::read_to_string(&second).expect("second"),
        "second\n"
    );
}

/// A reader spinning against 200 rewrites must never observe a partial document.
///
/// This is the property the temp+rename buys, and the mutation proof for it is
/// loud: `oc-goal` measured 292,699 of 293,732 reads observing a truncated file
/// once the rename was replaced with a direct write.
#[test]
fn a_rewrite_is_atomic_under_a_concurrent_reader() {
    let root = tempfile::tempdir().expect("temporary worktree");
    let long = format!("# Plan\n\n{}\n", "step\n".repeat(400));
    let path = write_plan(PlanLocation::Worktree(root.path()), key(), &long).expect("seed");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let path = path.clone();
        let stop = std::sync::Arc::clone(&stop);
        let long = long.clone();
        std::thread::spawn(move || {
            let mut reads = 0usize;
            let mut partial = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match std::fs::read_to_string(&path) {
                    Ok(seen) => {
                        reads += 1;
                        if seen != long {
                            partial += 1;
                        }
                    }
                    // A missing file is the truncate window too, so it counts.
                    Err(_) => {
                        reads += 1;
                        partial += 1;
                    }
                }
            }
            (reads, partial)
        })
    };

    for _ in 0..200 {
        write_plan(PlanLocation::Worktree(root.path()), key(), &long).expect("rewrite");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (reads, partial) = reader.join().expect("reader thread");

    assert!(
        reads >= 50,
        "the reader must actually have observed the file; only {reads} reads"
    );
    assert_eq!(
        partial, 0,
        "{partial} of {reads} reads saw a partial document"
    );
}
