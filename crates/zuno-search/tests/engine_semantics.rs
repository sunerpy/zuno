//! The ignore, hidden-file and override semantics, pinned against observations of
//! the real binary.
//!
//! Every expectation is exercised against the official binary that Zuno ships
//! beside or resolves from `PATH`; there is intentionally no second walker whose
//! semantics can drift.

use std::fs;
use std::path::Path;
use tempfile::TempDir;
use zuno_search::{
    AlreadyCancelled, GlobRequest, GrepRequest, NeverCancelled, Ripgrep, SearchError,
};

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

/// Writes a file whose name contains a byte that is a path separator on another
/// platform, returning the name when this filesystem accepted it.
///
/// `None` rather than a panic because a filesystem may reject the sequence, and an
/// assertion about a file that was never created passes vacuously. The returned name is
/// the identifier the engines have to hand back verbatim: on Unix `\` is an ordinary
/// filename byte, so `a\b.ts` is one flat file and not `a/b.ts`.
#[cfg(unix)]
fn plant(root: &Path, name: &str, contents: &str) -> Option<String> {
    fs::write(root.join(name), contents).ok()?;
    Some(name.to_owned())
}

fn engine() -> Ripgrep {
    Ripgrep::discover().expect("the test environment provides supported rg")
}

fn paths(root: &Path, pattern: &str) -> Vec<String> {
    engine()
        .glob(&GlobRequest::new(root, pattern, 10_000), &NeverCancelled)
        .expect("the glob succeeds")
        .items
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

fn matched(root: &Path, pattern: &str, include: Option<&str>) -> Vec<String> {
    engine()
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

fn tempdir_without_git_ancestor() -> TempDir {
    let default = tempfile::tempdir().expect("a temporary directory");
    if !default
        .path()
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
    {
        return default;
    }

    #[cfg(unix)]
    for base in ["/var/tmp", "/dev/shm"] {
        let base = Path::new(base);
        if base
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists())
        {
            continue;
        }
        if let Ok(dir) = tempfile::Builder::new()
            .prefix("zuno-search-")
            .tempdir_in(base)
        {
            return dir;
        }
    }

    panic!("the test environment has no writable temporary root outside a .git ancestor");
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
    let results = engine()
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
    let results = engine()
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
    let results = engine()
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

    let results = engine()
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

    let globbed = engine()
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

    let grepped = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 2), &NeverCancelled)
        .expect("the grep succeeds");
    assert_eq!(grepped.items.len(), 2);
    assert!(grepped.truncated);
}

#[test]
fn a_limit_that_exactly_fits_is_not_reported_as_truncated() {
    let dir = fixture();
    let globbed = engine()
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
    let error = engine()
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
    let error = engine()
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

    let missing = engine()
        .glob(
            &GlobRequest::new(dir.path().join("nowhere"), "**/*", 10),
            &NeverCancelled,
        )
        .expect_err("a missing root fails");
    assert!(matches!(missing, SearchError::RootMissing { .. }));
    assert!(!missing.is_model_correctable());

    let file = engine()
        .glob(
            &GlobRequest::new(dir.path().join("README.md"), "**/*", 10),
            &NeverCancelled,
        )
        .expect_err("a file root fails");
    assert!(matches!(file, SearchError::RootNotDirectory { .. }));
}

#[test]
fn a_fired_interrupt_stops_the_process_rather_than_returning_a_short_list() {
    let dir = fixture();

    let globbed = engine()
        .glob(
            &GlobRequest::new(dir.path(), "**/*", 10_000),
            &AlreadyCancelled,
        )
        .expect_err("a cancelled process does not return partial results");
    assert!(matches!(globbed, SearchError::Cancelled));

    let grepped = engine()
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

    let results = engine()
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
    // Ripgrep applies repository ignore files only when it discovers repository
    // context. With no `.git` anywhere, both files remain visible.
    let dir = tempdir_without_git_ancestor();
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

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("the grep succeeds");

    assert_eq!(results.items[0].text.len(), 2_003);
    assert!(results.items[0].text.ends_with("..."));
}

#[test]
fn an_invalid_include_glob_is_a_typed_failure_rather_than_an_empty_result() {
    // `include` reaches `rg` as a glob, so it has to be classified as one. While only
    // the regex was classified, `rg` rejected the invocation, searched nothing, and the
    // empty output was reported as "no matches" — which the model reads as proof the
    // pattern is absent from the tree.
    let dir = fixture();

    let error = engine()
        .grep(
            &GrepRequest::new(dir.path(), "needle", 10).with_include(Some("[unclosed".to_owned())),
            &NeverCancelled,
        )
        .expect_err("an unclosed character class in the include cannot compile");

    assert!(matches!(&error, SearchError::InvalidGlob { pattern, .. } if pattern == "[unclosed"));
    assert!(error.is_model_correctable());
}

#[test]
fn an_invocation_rg_refused_is_not_reported_as_no_matches() {
    // A regex that parses but exceeds `rg`'s compiled-size limit: exit 2, a diagnostic
    // on stderr, and not one record. Nothing was searched, so an empty result would be
    // a lie about the tree.
    let dir = fixture();

    let error = engine()
        .grep(
            &GrepRequest::new(dir.path(), "a{1000}{1000}{100}", 10),
            &NeverCancelled,
        )
        .expect_err("a rejected invocation is a failure, not an empty result");

    match &error {
        // Older regex builds reject the repetition while parsing instead; either way
        // the diagnostic reaches the caller and either way it is the model's to fix.
        SearchError::Rejected { message } | SearchError::InvalidPattern { message, .. } => {
            assert!(!message.is_empty(), "the diagnostic reaches the caller");
        }
        other => panic!("unexpected failure: {other:?}"),
    }
    assert!(
        error.is_model_correctable(),
        "a refused invocation must not reach the harness as a permanent tool failure"
    );
}

#[test]
fn a_line_that_is_not_valid_utf8_is_rendered_lossily_instead_of_failing_the_search() {
    // `rg` reports such a line as base64 `bytes` rather than `text`. Rejecting the
    // record failed the whole grep, discarding every legitimate match with it, and no
    // narrower query could avoid it: the bad byte is a property of the repository.
    let dir = tempfile::tempdir().expect("a temporary directory");
    fs::write(dir.path().join("bad.txt"), b"needle \xff\xfe here\n").expect("a latin-1 fixture");
    write(dir.path(), "good.txt", "needle good\n");

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("one undecodable line does not fail the search");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["bad.txt", "good.txt"]
    );
    assert!(
        results.items[0].text.contains('\u{fffd}'),
        "the invalid bytes are replaced: {:?}",
        results.items[0].text
    );
}

#[test]
fn one_oversized_record_is_dropped_without_losing_the_other_matches() {
    // Sized against the decode cap, not against a number: the line has to be larger
    // than one record may be, and a 70 KiB line is now answered rather than dropped.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let huge = format!("needle{}\n", "x".repeat(1_500 * 1024));
    write(dir.path(), "huge.txt", &huge);
    write(dir.path(), "small.txt", "needle\n");

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("a record over the decode cap does not fail the search");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["small.txt"]
    );
    assert!(
        results.truncated,
        "huge.txt still holds a match that is not in the list: {results:?}"
    );
}

#[test]
fn a_run_whose_every_record_is_undecodable_still_fails_loudly() {
    // The other half of the rule: dropping records must not turn into reporting an
    // absent pattern when nothing at all could be decoded.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let huge = format!("needle{}\n", "x".repeat(1_500 * 1024));
    write(dir.path(), "huge.txt", &huge);

    let error = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect_err("nothing decoded, so the search reports why");

    assert!(matches!(error, SearchError::Ripgrep { .. }));
}

#[test]
fn the_per_file_match_cap_does_not_change_the_limited_result() {
    // `grep` bounds `rg`'s output with a per-file `--max-count` of one more than the
    // limit. That cap is the largest one that cannot change the answer, so both the
    // reported lines and the truncation flag stay what an uncapped run produced.
    let dir = tempfile::tempdir().expect("a temporary directory");
    write(dir.path(), "a.txt", &"needle\n".repeat(50));

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 3), &NeverCancelled)
        .expect("the grep succeeds");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.line)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(results.truncated);
}

/// Restores `mode` on `path`, so a fixture the test made unreadable can be cleaned up.
#[cfg(unix)]
fn restore_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

/// A tree holding one readable file and one directory the walk cannot enter.
#[cfg(unix)]
fn tree_with_an_unreadable_directory() -> Option<(TempDir, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("a temporary directory");
    fs::create_dir_all(dir.path().join("src")).expect("a fixture subdirectory");
    write(dir.path(), "src/a.ts", "export const a = 1\n");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("the directory to lock");
    fs::write(locked.join("x.ts"), "secret\n").expect("a file inside it");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode is set");
    if fs::read_dir(&locked).is_ok() {
        // Running with an identity that ignores the mode (root, or a filesystem
        // without POSIX permissions): there is no partial walk to observe, so the
        // case cannot be asserted here rather than being asserted vacuously.
        restore_mode(&locked, 0o755);
        return None;
    }
    Some((dir, locked))
}

#[cfg(unix)]
#[test]
fn a_glob_matching_nothing_stays_empty_when_the_walk_hit_an_unreadable_directory() {
    // The most common `glob` outcome is "my pattern matched nothing", and `rg --files`
    // writes nothing at all for it. One unreadable directory anywhere under the root
    // makes `rg` exit 2 with a diagnostic, so keying "rg refused the invocation" on an
    // empty stdout turned that outcome into a hard, non-correctable tool failure.
    let Some((dir, locked)) = tree_with_an_unreadable_directory() else {
        return;
    };

    let results = engine().glob(
        &GlobRequest::new(dir.path(), "**/*.nothing", 10_000),
        &NeverCancelled,
    );
    restore_mode(&locked, 0o755);

    let results = results.expect("a partial walk that matched nothing is an empty result");
    assert!(results.items.is_empty(), "unexpected items: {results:?}");
    assert!(!results.truncated);
}

#[cfg(unix)]
#[test]
fn a_glob_still_reports_what_it_reached_when_the_walk_hit_an_unreadable_directory() {
    // The other direction: suppressing the walk diagnostic must not suppress results.
    let Some((dir, locked)) = tree_with_an_unreadable_directory() else {
        return;
    };

    let results = engine().glob(
        &GlobRequest::new(dir.path(), "**/*.ts", 10_000),
        &NeverCancelled,
    );
    restore_mode(&locked, 0o755);

    let results = results.expect("a partial walk returns what it reached");
    assert_eq!(
        results
            .items
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.ts"]
    );
}

#[cfg(unix)]
#[test]
fn a_glob_rg_refused_is_still_a_typed_failure_on_a_partially_unreadable_tree() {
    // Silencing the walk diagnostic must not silence the diagnostic that says the
    // pattern itself was thrown out, even when both would have been printed.
    let Some((dir, locked)) = tree_with_an_unreadable_directory() else {
        return;
    };

    let error = engine().glob(
        &GlobRequest::new(dir.path(), "[unclosed", 10_000),
        &NeverCancelled,
    );
    restore_mode(&locked, 0o755);

    let error = error.expect_err("an unclosed character class cannot compile");
    assert!(matches!(&error, SearchError::InvalidGlob { pattern, .. } if pattern == "[unclosed"));
    assert!(error.is_model_correctable());
}

#[test]
fn a_bundled_file_with_one_very_long_matching_line_reports_it_and_says_it_truncated() {
    // The reviewer's input: line 1 is a 70 KiB matching line, followed by 20 ordinary
    // matching lines, asked for with a limit of 3. Dropping the long record and then
    // computing `truncated` from what survived answered `items=3 truncated=false
    // lines=[2,3,4]` for a file holding 21 matches — a complete-looking answer that
    // silently lost a match and denied there were more.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut bundle = format!("needle{}\n", "x".repeat(70 * 1024));
    for index in 0..20 {
        bundle.push_str(&format!("needle {index}\n"));
    }
    write(dir.path(), "bundle.js", &bundle);

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 3), &NeverCancelled)
        .expect("the grep succeeds");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.line)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the long line is match 1 and must not be skipped"
    );
    assert!(
        results.truncated,
        "21 matches exist and 3 were reported: {results:?}"
    );
}

#[test]
fn a_record_over_the_decode_cap_is_dropped_and_the_result_says_there_is_more() {
    // A record too large to decode at all is still evidence that a match exists, so
    // the result may not claim to be complete. `truncated` here is below the limit,
    // which is exactly the case `items.len() > limit` cannot express.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut huge = format!("needle{}\n", "y".repeat(1_500 * 1024));
    huge.push_str("needle tail\n");
    write(dir.path(), "huge.js", &huge);

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("a record over the decode cap does not fail the search");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.line)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert!(
        results.truncated,
        "a dropped record means there is more: {results:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_match_in_a_file_whose_name_is_not_utf8_is_dropped_rather_than_renamed() {
    // `rg` reports such a path as base64 `bytes`. Rendering it lossily handed the
    // model `bad\u{fffd}.txt` as a `RelativePath` to feed back into `read` or `edit`,
    // which names no file on disk. The record is dropped instead, and dropping it is
    // what `truncated` reports.
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().expect("a temporary directory");
    let odd = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
    if fs::write(dir.path().join(odd), "needle odd\n").is_err() {
        // A filesystem that rejects the byte sequence, so there is nothing to assert.
        return;
    }
    write(dir.path(), "good.txt", "needle good\n");

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("the readable match still comes back");

    assert_eq!(
        results
            .items
            .iter()
            .map(|found| found.entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["good.txt"],
        "no path may contain U+FFFD: {results:?}"
    );
    assert!(
        results.truncated,
        "the dropped record means there is more: {results:?}"
    );
}

#[test]
fn a_regex_over_ripgreps_compiled_size_limit_is_the_models_to_correct() {
    // `rg: compiled regex exceeds size limit of 104857600`: exit 2, that diagnostic on
    // stderr, not one record. Typing it as an opaque backend failure told the harness
    // that a one-token edit to the model's own pattern could not fix it.
    let dir = fixture();
    let pattern = "a{1000}{1000}{100}";

    let error = engine()
        .grep(&GrepRequest::new(dir.path(), pattern, 10), &NeverCancelled)
        .expect_err("a rejected invocation is a failure, not an empty result");

    match &error {
        SearchError::InvalidPattern {
            pattern: reported,
            message,
        } => {
            assert_eq!(reported, pattern, "the failure names the model's own regex");
            assert!(
                message.contains("exceeds size limit"),
                "rg's own diagnostic reaches the model: {message:?}"
            );
        }
        other => panic!("unexpected failure: {other:?}"),
    }
    assert!(
        error.is_model_correctable(),
        "the model can fix its own regex"
    );
}

#[test]
fn a_grep_restricted_to_a_file_ripgrep_could_not_read_is_not_reported_as_no_matches() {
    // `--no-messages` suppresses the "No such file or directory" diagnostic and
    // `--json` still emits its summary, so output alone cannot tell this apart from a
    // file that simply has no match. The summary's own `searches: 0` can.
    let dir = fixture();
    let mut request = GrepRequest::new(dir.path(), "needle", 10);
    request.file = Some("does-not-exist.txt".to_owned());

    let error = engine()
        .grep(&request, &NeverCancelled)
        .expect_err("a file that was never searched is not an absent pattern");

    assert!(
        matches!(&error, SearchError::Rejected { message } if message.contains("does-not-exist.txt")),
        "unexpected failure: {error:?}"
    );
    assert!(error.is_model_correctable());
}

#[cfg(unix)]
#[test]
fn a_glob_matching_nothing_stays_empty_when_the_walk_hit_a_dangling_symlink() {
    // The same defect reached through a trigger no identity can ignore: `chmod 000` is
    // not enforced for root, so the permission cases above stand down there, while a
    // symlink to nothing makes `rg --follow` exit 2 with a diagnostic for every user.
    let dir = tempfile::tempdir().expect("a temporary directory");
    write(dir.path(), "a.ts", "export const a = 1\n");
    std::os::unix::fs::symlink(dir.path().join("absent"), dir.path().join("dangling"))
        .expect("a symlink to nothing");

    let mut request = GlobRequest::new(dir.path(), "**/*.nothing", 10_000);
    request.follow = true;

    let results = engine()
        .glob(&request, &NeverCancelled)
        .expect("a partial walk that matched nothing is an empty result");

    assert!(results.items.is_empty(), "unexpected items: {results:?}");
    assert!(!results.truncated);
}

#[cfg(unix)]
#[test]
fn a_grep_whose_every_match_is_in_a_file_with_a_non_utf8_name_still_answers() {
    // The reviewer's input, verbatim: `bad\xff.txt` holds the only `needle` and
    // `clean.txt` holds none. Folding the rejected *path* into the same channel as a
    // record Zuno could not decode at all made this meet the loud-when-empty rule, so
    // a legacy latin-1 filename from an old tarball turned a working search into
    // `SearchError::Ripgrep` — not model-correctable, so `map_search_error` yields the
    // deliberately non-retryable `ToolError::Failed`. A path Zuno cannot *name* is a
    // different fact from a record Zuno cannot *read*: the first still proves what the
    // answer is, so it costs `truncated`, not the call.
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().expect("a temporary directory");
    let odd = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
    if fs::write(dir.path().join(odd), "needle odd\n").is_err() {
        // A filesystem that rejects the byte sequence, so there is nothing to assert.
        return;
    }
    write(dir.path(), "clean.txt", "nothing to find here\n");

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "needle", 10), &NeverCancelled)
        .expect("an unnameable path costs its record, not the search");

    assert!(
        results.items.is_empty(),
        "no path may contain U+FFFD: {results:?}"
    );
    assert!(
        results.truncated,
        "a match exists that the list cannot name: {results:?}"
    );
    // The readable half of the same tree still answers, which is the property the hard
    // failure destroyed: the failure was for the whole call, not for the one record.
    let clean = engine()
        .grep(
            &GrepRequest::new(dir.path(), "nothing to find", 10),
            &NeverCancelled,
        )
        .expect("the search still runs over this tree");
    assert_eq!(
        clean
            .items
            .iter()
            .map(|found| found.entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["clean.txt"]
    );
}

#[cfg(unix)]
#[test]
fn a_glob_never_reports_a_path_that_names_no_file() {
    // The reviewer's input, verbatim: a directory holding `bad\xff.ts`,
    // `two\nlines.ts` and `good.ts`, asked for with `**/*.ts`. Parsing `rg --files`
    // as `into_lossy_string(stdout).lines()` answered
    // `["bad\u{fffd}.ts", "good.ts", "lines.ts", "two"], truncated=false`: one phantom
    // identifier, two paths that were never on disk, and no signal at all. `glob` is
    // the tool the model uses to *obtain* paths it feeds to `read` and `edit`.
    //
    // Widened with the two names the *separator* rewrite invented out of real files:
    // `a\b.ts` was answered as `a/b.ts` and `\lead.ts` as `lead.ts`, and on this
    // platform neither of those is a file. The oracle below is the same one, so the
    // fixture was the only thing missing.
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().expect("a temporary directory");
    let odd = std::ffi::OsStr::from_bytes(b"bad\xff.ts");
    let newline = std::ffi::OsStr::from_bytes(b"two\nlines.ts");
    if fs::write(dir.path().join(odd), "a\n").is_err()
        || fs::write(dir.path().join(newline), "b\n").is_err()
    {
        // A filesystem that rejects either byte sequence, so there is nothing to
        // assert rather than an assertion that passes vacuously.
        return;
    }
    write(dir.path(), "good.ts", "c\n");
    let mut expected = vec!["good.ts".to_owned(), "two\nlines.ts".to_owned()];
    expected.extend(
        ["a\\b.ts", "\\lead.ts"]
            .into_iter()
            .filter_map(|name| plant(dir.path(), name, "d\n")),
    );
    expected.sort();

    let results = engine()
        .glob(
            &GlobRequest::new(dir.path(), "**/*.ts", 10_000),
            &NeverCancelled,
        )
        .expect("the readable paths still come back");

    assert_eq!(
        results
            .items
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>(),
        expected,
        "a newline or a backslash in a name is one real path, and no path may contain \
         U+FFFD: {results:?}"
    );
    assert!(
        results.truncated,
        "a path was dropped, so the list is not all there is: {results:?}"
    );
    // The oracle is the filesystem, not this crate's own rule: every path handed back
    // has to open, because the model feeds it to `read` and `edit`.
    for entry in &results.items {
        assert!(
            fs::metadata(dir.path().join(&entry.path)).is_ok(),
            "a returned path must name a real file: {:?}",
            entry.path
        );
    }
    // And the names the newline, lossy and separator parses used to answer with are
    // exactly the ones that do not: nothing readable was lost by dropping them.
    for phantom in ["bad\u{fffd}.ts", "lines.ts", "two", "a/b.ts", "lead.ts"] {
        assert!(
            fs::metadata(dir.path().join(phantom)).is_err(),
            "{phantom:?} was never on disk"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_backslash_in_a_name_never_aliases_one_real_file_onto_another() {
    // The reviewer's input, verbatim: a real nested `a/b.ts` holding "real nested",
    // plus flat files literally named `a\b.ts`, `back\slash.ts`, `\lead.ts` and
    // `.\dotdot..ts`, each holding "needle content", asked for with `**/*.ts` at limit
    // 10_000. `normalize_relative` rewrote every `\` to `/` and trimmed leading
    // separators on *every* platform, so `glob` answered
    // `["a/b.ts", "a/b.ts", "back/slash.ts", "dotdot..ts", "lead.ts"], truncated=false`
    // — one identifier standing for two different real files, three naming none — and
    // `grep "needle content"` answered `["a/b.ts", ...]` while
    // `fs::read_to_string(root.join("a/b.ts"))` is "real nested\n".
    //
    // That is the dangerous direction of the phantom-identifier class: a U+FFFD name
    // fails loudly in `read`, an alias succeeds against the *wrong* real file, and a
    // write-capable `edit` or `apply_patch` keyed on it then modifies a file that never
    // matched — with `truncated = false`, so there is no signal at all.
    let dir = tempfile::tempdir().expect("a temporary directory");
    fs::create_dir_all(dir.path().join("a")).expect("the nested directory");
    write(dir.path(), "a/b.ts", "real nested\n");

    let planted: Vec<String> = ["a\\b.ts", "back\\slash.ts", "\\lead.ts", ".\\dotdot..ts"]
        .into_iter()
        .filter_map(|name| plant(dir.path(), name, "needle content\n"))
        .collect();
    if planted.is_empty() {
        // A filesystem that rejects a backslash in a name, so there is nothing to
        // assert rather than an assertion that passes vacuously.
        return;
    }
    let mut expected = planted.clone();
    expected.push("a/b.ts".to_owned());
    expected.sort();

    let results = engine()
        .glob(
            &GlobRequest::new(dir.path(), "**/*.ts", 10_000),
            &NeverCancelled,
        )
        .expect("the glob succeeds");
    let items: Vec<String> = results
        .items
        .iter()
        .map(|entry| entry.path.clone())
        .collect();

    assert_eq!(
        items, expected,
        "every real file exactly once, under the name it actually has: {results:?}"
    );
    let mut unique = items.clone();
    unique.dedup();
    assert_eq!(
        unique, items,
        "two different real files were aliased onto one identifier: {items:?}"
    );
    assert!(
        !results.truncated,
        "every path was nameable, so nothing was dropped: {results:?}"
    );
    // The oracle is the filesystem: every identifier the model is handed has to open.
    for entry in &results.items {
        assert!(
            fs::metadata(dir.path().join(&entry.path)).is_ok(),
            "a returned path must name a real file: {:?}",
            entry.path
        );
    }

    // And grep's oracle is the file's content: a reported path must contain what was
    // searched for. The nested `a/b.ts` holds "real nested" and no needle, so naming it
    // is the alias, not a match.
    let hits = engine()
        .grep(
            &GrepRequest::new(dir.path(), "needle content", 10_000),
            &NeverCancelled,
        )
        .expect("the grep succeeds");
    assert_eq!(
        hits.items.len(),
        planted.len(),
        "one match per planted file: {hits:?}"
    );
    for found in &hits.items {
        let contents = fs::read_to_string(dir.path().join(&found.entry.path))
            .expect("a reported match must open");
        assert!(
            contents.contains("needle content"),
            "grep named {:?}, which holds {contents:?}",
            found.entry.path
        );
    }
    assert!(
        !hits.items.iter().any(|found| found.entry.path == "a/b.ts"),
        "the real nested file contains no needle and must never be reported: {hits:?}"
    );
    assert!(
        !hits.truncated,
        "every match was nameable, so nothing was dropped: {hits:?}"
    );
}

#[test]
fn a_refusal_caused_by_the_include_glob_is_not_blamed_on_a_valid_regex() {
    // The reviewer's input, verbatim: the regex is fine and the *include* is not, but
    // the include's own text satisfied the regex substring rules, so the failure came
    // back as `InvalidPattern { pattern: "needle" }` — an error that contradicts
    // itself and sends the model editing a pattern that was never at fault.
    let dir = fixture();

    for include in ["[regex exceeds size limit", "[regex parse error"] {
        let error = engine()
            .grep(
                &GrepRequest::new(dir.path(), "needle", 10).with_include(Some(include.to_owned())),
                &NeverCancelled,
            )
            .expect_err("an unclosed character class cannot compile");

        match &error {
            SearchError::InvalidGlob { pattern, .. } => {
                assert_eq!(pattern, include, "the failure must name the glob rg quoted")
            }
            other => panic!("the include was rejected, not the regex: {other:?}"),
        }
        assert!(error.is_model_correctable());
    }
}

#[test]
fn one_enormous_submatch_is_capped_like_the_line_that_carries_it() {
    // The reviewer's shape: a 300 KiB line that is entirely one match. `Match::text`
    // was capped and `Submatch::text` was not, so raising the record cap raised the
    // bytes one kept match retains by the same factor — 100 files of this shape
    // retained 30 MB of submatch text that no caller reads.
    let dir = tempfile::tempdir().expect("a temporary directory");
    write(
        dir.path(),
        "wide.txt",
        &format!("{}\n", "x".repeat(300 * 1024)),
    );

    let results = engine()
        .grep(&GrepRequest::new(dir.path(), "x+", 10), &NeverCancelled)
        .expect("the grep succeeds");

    let found = &results.items[0];
    assert_eq!(
        found.submatches.len(),
        1,
        "the whole line is one match: {found:?}"
    );
    assert!(
        found.submatches[0].text.chars().count() <= 2_010,
        "a submatch may not retain more than the line it is quoted from: {}",
        found.submatches[0].text.chars().count()
    );
    assert!(
        found.submatches[0].text.ends_with("..."),
        "the cut is marked"
    );
}
