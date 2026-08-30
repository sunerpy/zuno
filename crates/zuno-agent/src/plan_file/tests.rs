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
    let worktree = Path::new("/tmp/zuno-t79-repo");
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
        Some(zuno_paths::data().join("plans").as_path())
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
    let worktree = Path::new("/tmp/zuno-t79-repo");
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
            plan_path(PlanLocation::Worktree(Path::new("/tmp/zuno-t79-repo")), key),
            None,
            "{hostile:?} must not resolve to a plan path"
        );
    }
}
