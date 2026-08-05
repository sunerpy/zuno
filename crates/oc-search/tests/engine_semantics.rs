//! The ignore, hidden-file and override semantics, pinned against observations of
//! the real binary.
//!
//! Every expectation in this file was first recorded by running the real
//! `opencode debug rg` (1.18.12) over the same tree; the transcripts are in
//! `.omo/evidence/task-41-opencode-rust.txt`. They are asserted here as unit
//! expectations so a regression is caught without spawning a process, and the
//! differential test in `oc-tools` re-checks them against the binary itself.

use oc_search::{
    AlreadyCancelled, Backend, EmbeddedEngine, GlobRequest, GrepRequest, NeverCancelled,
    SearchError,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// The tree every case runs against.
///
/// `.gitignore` excludes `ignored.ts` and `node_modules/`; `.hidden_file.ts` and
/// `.hidden_dir/` are hidden; `.git/` holds a file that must never surface.
fn fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let root = dir.path();

    for sub in [
        "src",
        "lib",
        "nested/deep",
        ".hidden_dir",
        "node_modules/pkg",
        ".git",
    ] {
        fs::create_dir_all(root.join(sub)).expect("a fixture subdirectory");
    }

    write(root, "src/a.ts", "alpha needle here\nbeta\n");
    write(root, "src/b.ts", "gamma\nneedle in b\n");
    write(root, "lib/c.js", "needle c\n");
    write(root, "nested/deep/d.ts", "needle deep\n");
    write(root, ".hidden_file.ts", "needle hidden\n");
    write(root, ".hidden_dir/e.ts", "needle in hidden dir\n");
    write(root, "ignored.ts", "needle ignored\n");
    write(root, "node_modules/pkg/f.ts", "needle nm\n");
    write(root, "README.md", "needle readme\n");
    write(root, ".gitignore", "ignored.ts\nnode_modules/\n");
    write(root, ".git/config", "needle in the object store\n");

    dir
}

fn write(root: &Path, relative: &str, contents: &str) {
    fs::write(root.join(relative), contents).expect("a fixture file");
}

fn paths(root: &Path, pattern: &str) -> Vec<String> {
    EmbeddedEngine
        .glob(&GlobRequest::new(root, pattern, 10_000), &NeverCancelled)
        .expect("the glob succeeds")
        .items
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

fn matched(root: &Path, pattern: &str, include: Option<&str>) -> Vec<String> {
    EmbeddedEngine
        .grep(
            &GrepRequest::new(root, pattern, 10_000).with_include(include.map(str::to_owned)),
            &NeverCancelled,
        )
        .expect("the grep succeeds")
        .items
        .into_iter()
        .map(|found| format!("{}:{}", found.entry.path, found.line))
        .collect()
}

#[test]
fn a_star_glob_whitelists_everything_including_ignored_and_hidden_paths() {
    // Recorded from `opencode debug rg files`, which passes `--glob=**/*`: all ten
    // files come back, gitignored and hidden alike, because an override match
    // outranks every ignore file and the hidden rule.
    let dir = fixture();

    assert_eq!(
        paths(dir.path(), "**/*"),
        vec![
            ".gitignore",
            ".hidden_dir/e.ts",
            ".hidden_file.ts",
            "README.md",
            "ignored.ts",
            "lib/c.js",
            "nested/deep/d.ts",
            "node_modules/pkg/f.ts",
            "src/a.ts",
            "src/b.ts",
        ]
    );
}

#[test]
fn the_git_directory_is_excluded_even_by_a_glob_that_matches_everything() {
    let dir = fixture();
    let listed = paths(dir.path(), "**/*");
    assert!(
        !listed.iter().any(|path| path.starts_with(".git/")),
        "the trailing !**/.git/** exclusion must beat any include: {listed:?}"
    );
}

#[test]
fn a_narrow_glob_prunes_directories_it_does_not_whitelist() {
    // Recorded from `opencode debug rg files --glob '**/*.ts'`. The asymmetry is the
    // point: `.hidden_file.ts` and `ignored.ts` are whitelisted directly, so they
    // survive being hidden and being gitignored, while `.hidden_dir/e.ts` and
    // `node_modules/pkg/f.ts` do not appear because `**/*.ts` never matched their
    // parent directories, which were pruned before the walk reached the children.
    let dir = fixture();

    assert_eq!(
        paths(dir.path(), "**/*.ts"),
        vec![
            ".hidden_file.ts",
            "ignored.ts",
            "nested/deep/d.ts",
            "src/a.ts",
            "src/b.ts",
        ]
    );
}

#[test]
fn a_directory_scoped_glob_returns_only_that_subtree() {
    let dir = fixture();
    assert_eq!(paths(dir.path(), "src/**"), vec!["src/a.ts", "src/b.ts"]);
}

#[test]
fn glob_results_are_path_sorted_on_every_run() {
    let dir = fixture();
    let first = paths(dir.path(), "**/*");
    for _ in 0..5 {
        assert_eq!(
            paths(dir.path(), "**/*"),
            first,
            "the order must not vary between runs; the oracle's does"
        );
    }
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted);
}

#[test]
fn grep_searches_hidden_paths_because_the_oracle_always_passes_hidden() {
    // Recorded from `opencode debug rg search needle`: seven matches, including both
    // hidden paths, and excluding the two gitignored ones and `.git/config`.
    let dir = fixture();

    assert_eq!(
        matched(dir.path(), "needle", None),
        vec![
            ".hidden_dir/e.ts:1",
            ".hidden_file.ts:1",
            "README.md:1",
            "lib/c.js:1",
            "nested/deep/d.ts:1",
            "src/a.ts:1",
            "src/b.ts:2",
        ]
    );
}

#[test]
fn a_grep_include_whitelist_reaches_ignored_and_hidden_files() {
    // Recorded from `opencode debug rg search needle --glob '*.ts'`. `ignored.ts`
    // appears even though it is gitignored, because `--glob` whitelisted it, and
    // `.hidden_dir/e.ts` appears because grep also passes `--hidden`, so the
    // directory was not pruned.
    let dir = fixture();

    assert_eq!(
        matched(dir.path(), "needle", Some("*.ts")),
        vec![
            ".hidden_dir/e.ts:1",
            ".hidden_file.ts:1",
            "ignored.ts:1",
            "nested/deep/d.ts:1",
            "src/a.ts:1",
            "src/b.ts:2",
        ]
    );
}

#[test]
fn grep_never_reads_the_git_directory() {
    let dir = fixture();
    let found = matched(dir.path(), "object store", None);
    assert!(
        found.is_empty(),
        "the object store must stay out of results: {found:?}"
    );
}

#[test]
fn a_pattern_with_no_matches_is_an_empty_result_and_not_an_error() {
    let dir = fixture();
    let results = EmbeddedEngine
        .grep(
            &GrepRequest::new(dir.path(), "zzzznomatchzzzz", 10_000),
            &NeverCancelled,
        )
        .expect("a pattern that matches nothing still succeeds");

    assert!(results.items.is_empty());
    assert!(!results.truncated);
}

#[test]
fn a_glob_with_no_matches_is_an_empty_result_and_not_an_error() {
    let dir = fixture();
    let results = EmbeddedEngine
        .glob(
            &GlobRequest::new(dir.path(), "**/*.nothing", 10_000),
            &NeverCancelled,
        )
        .expect("a glob that matches nothing still succeeds");

    assert!(results.items.is_empty());
}

#[test]
fn a_match_carries_the_line_terminator_the_offset_and_the_submatch_spans() {
    let dir = fixture();
    let results = EmbeddedEngine
        .grep(
            &GrepRequest::new(dir.path(), "needle", 10_000),
            &NeverCancelled,
        )
        .expect("the grep succeeds");

    let found = results
        .items
        .iter()
        .find(|found| found.entry.path == "src/a.ts")
        .expect("src/a.ts matches");

    assert_eq!(found.line, 1);
    assert_eq!(found.offset, 0);
    assert_eq!(found.text, "alpha needle here\n");
    assert_eq!(found.submatches.len(), 1);
    assert_eq!(found.submatches[0].text, "needle");
    assert_eq!(found.submatches[0].start, 6);
    assert_eq!(found.submatches[0].end, 12);

    let second = results
        .items
        .iter()
        .find(|found| found.entry.path == "src/b.ts")
        .expect("src/b.ts matches");
    assert_eq!(second.line, 2);
    assert_eq!(
        second.offset, 6,
        "the offset is the byte position of the line, not of the match"
    );
}

#[test]
fn every_occurrence_on_a_line_becomes_a_submatch_of_one_record() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    write(dir.path(), "a.txt", "needle needle needle\n");

    let results = EmbeddedEngine
        .grep(
            &GrepRequest::new(dir.path(), "needle", 10_000),
            &NeverCancelled,
        )
        .expect("the grep succeeds");

    assert_eq!(results.items.len(), 1, "one record per line, not per match");
    assert_eq!(results.items[0].submatches.len(), 3);
    assert_eq!(results.items[0].submatches[2].start, 14);
}

#[test]
fn the_limit_truncates_and_says_so() {
    let dir = fixture();

    let globbed = EmbeddedEngine
        .glob(&GlobRequest::new(dir.path(), "**/*", 3), &NeverCancelled)
        .expect("the glob succeeds");
    assert_eq!(globbed.items.len(), 3);
    assert!(globbed.truncated);
    assert_eq!(
        globbed
            .items
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec![".gitignore", ".hidden_dir/e.ts", ".hidden_file.ts"],
        "truncation takes the first of a stable order, so it is reproducible"
    );

    let grepped = EmbeddedEngine
        .grep(&GrepRequest::new(dir.path(), "needle", 2), &NeverCancelled)
        .expect("the grep succeeds");
    assert_eq!(grepped.items.len(), 2);
    assert!(grepped.truncated);
}

#[test]
fn a_limit_that_exactly_fits_is_not_reported_as_truncated() {
    let dir = fixture();
    let globbed = EmbeddedEngine
        .glob(&GlobRequest::new(dir.path(), "src/**", 2), &NeverCancelled)
        .expect("the glob succeeds");

    assert_eq!(globbed.items.len(), 2);
    assert!(
        !globbed.truncated,
        "the engine reports what it saw; the weaker len == limit test belongs to the tool"
    );
}

#[test]
fn an_invalid_regex_is_a_typed_model_correctable_failure() {
    let dir = fixture();
    let error = EmbeddedEngine
        .grep(
            &GrepRequest::new(dir.path(), "(unclosed", 10),
            &NeverCancelled,
        )
        .expect_err("an unclosed group cannot compile");

    assert!(matches!(error, SearchError::InvalidPattern { .. }));
    assert!(error.is_model_correctable());
}

#[test]
fn an_invalid_glob_is_a_typed_model_correctable_failure() {
    let dir = fixture();
    let error = EmbeddedEngine
        .glob(
            &GlobRequest::new(dir.path(), "[unclosed", 10),
            &NeverCancelled,
        )
        .expect_err("an unclosed character class cannot compile");

    assert!(matches!(error, SearchError::InvalidGlob { .. }));
    assert!(error.is_model_correctable());
}

#[test]
fn a_missing_root_and_a_file_root_are_distinguished() {
    let dir = fixture();

    let missing = EmbeddedEngine
        .glob(
            &GlobRequest::new(dir.path().join("nowhere"), "**/*", 10),
            &NeverCancelled,
        )
        .expect_err("a missing root fails");
    assert!(matches!(missing, SearchError::RootMissing { .. }));
    assert!(!missing.is_model_correctable());

    let file = EmbeddedEngine
        .glob(
            &GlobRequest::new(dir.path().join("README.md"), "**/*", 10),
            &NeverCancelled,
        )
        .expect_err("a file root fails");
    assert!(matches!(file, SearchError::RootNotDirectory { .. }));
}

#[test]
fn a_fired_interrupt_stops_the_walk_rather_than_returning_a_short_list() {
    let dir = fixture();

    let globbed = EmbeddedEngine
        .glob(
            &GlobRequest::new(dir.path(), "**/*", 10_000),
            &AlreadyCancelled,
        )
        .expect_err("a cancelled walk does not return partial results");
    assert!(matches!(globbed, SearchError::Cancelled));

    let grepped = EmbeddedEngine
        .grep(
            &GrepRequest::new(dir.path(), "needle", 10_000),
            &AlreadyCancelled,
        )
        .expect_err("a cancelled search does not return partial results");
    assert!(matches!(grepped, SearchError::Cancelled));
}

#[test]
fn a_binary_file_does_not_contribute_matches() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    fs::write(dir.path().join("blob.bin"), b"needle\x00needle\n").expect("a binary fixture");
    write(dir.path(), "plain.txt", "needle\n");

    let results = EmbeddedEngine
        .grep(
            &GrepRequest::new(dir.path(), "needle", 10_000),
            &NeverCancelled,
        )
        .expect("the grep succeeds");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["plain.txt"],
        "binary detection quits at the first NUL, exactly as rg does by default"
    );
}

#[test]
fn a_nested_gitignore_is_honoured() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    fs::create_dir_all(dir.path().join("pkg")).expect("a subdirectory");
    fs::create_dir_all(dir.path().join(".git")).expect("a git directory");
    write(dir.path(), ".git/config", "");
    write(dir.path(), "pkg/.gitignore", "secret.ts\n");
    write(dir.path(), "pkg/secret.ts", "needle\n");
    write(dir.path(), "pkg/public.ts", "needle\n");

    assert_eq!(
        matched(dir.path(), "needle", None),
        vec!["pkg/public.ts:1"],
        "a .gitignore nested below the root applies to its own subtree"
    );
}

#[test]
fn a_gitignore_outside_a_repository_is_not_applied() {
    // `ignore`'s `require_git` default is true, and so is ripgrep's. Verified against
    // rg 15.1.0: with no `.git` anywhere, `rg --json --hidden -- needle .` over a
    // tree whose `.gitignore` names `secret.ts` returns `secret.ts` anyway. A port
    // that applied the file unconditionally would hide a result the oracle shows.
    let dir = tempfile::tempdir().expect("a temporary directory");
    write(dir.path(), ".gitignore", "secret.ts\n");
    write(dir.path(), "secret.ts", "needle\n");
    write(dir.path(), "public.ts", "needle\n");

    assert_eq!(
        matched(dir.path(), "needle", None),
        vec!["public.ts:1", "secret.ts:1"]
    );
}

#[test]
fn a_long_line_is_capped_and_marked() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let line = format!("needle{}\n", "x".repeat(3_000));
    write(dir.path(), "long.txt", &line);

    let results = EmbeddedEngine
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("the grep succeeds");

    assert_eq!(results.items[0].text.len(), 2_003);
    assert!(results.items[0].text.ends_with("..."));
}

#[test]
fn both_backends_agree_when_a_system_ripgrep_is_available() {
    let dir = fixture();
    let Some(program) = oc_search::locate_ripgrep() else {
        eprintln!("no system rg on PATH; the cross-backend comparison is skipped");
        return;
    };

    let embedded = Backend::embedded();
    let external = Backend::ripgrep(&program);

    for pattern in ["**/*", "**/*.ts", "src/**"] {
        let request = GlobRequest::new(dir.path(), pattern, 10_000);
        assert_eq!(
            embedded
                .glob(&request, &NeverCancelled)
                .expect("embedded")
                .items,
            external
                .glob(&request, &NeverCancelled)
                .expect("ripgrep")
                .items,
            "the two backends disagree on glob {pattern}"
        );
    }

    for include in [None, Some("*.ts".to_owned())] {
        let request = GrepRequest::new(dir.path(), "needle", 10_000).with_include(include.clone());
        assert_eq!(
            embedded
                .grep(&request, &NeverCancelled)
                .expect("embedded")
                .items,
            external
                .grep(&request, &NeverCancelled)
                .expect("ripgrep")
                .items,
            "the two backends disagree on grep with include {include:?}"
        );
    }
}
