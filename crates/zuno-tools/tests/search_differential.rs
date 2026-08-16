//! The differential: the embedded engine against the real binary's `debug rg`.
//!
//! # What is compared
//!
//! `opencode debug rg files` and `debug rg search` are thin wrappers over the same
//! `Ripgrep` service the `glob` and `grep` tools use
//! (`packages/opencode/src/cli/cmd/debug/ripgrep.ts:35-76`), which makes them the
//! only place the oracle's raw search results are observable. Both are compared over
//! a 5,000-file tree that also holds a gitignored file, a gitignored directory, a
//! hidden file, a hidden directory, and a `.git` directory.
//!
//! # The oracle has no result order
//!
//! It passes no `--sort`, so its walk is parallel and five consecutive runs over one
//! unchanged tree return five different orders. Comparing the two streams
//! positionally would fail against the oracle itself. Instead:
//!
//! 1. Both sides are **sorted** and compared line for line, which catches any
//!    missing, extra, or altered result — the whole semantic content.
//! 2. The engine's own output is separately asserted to *already be* in that sorted
//!    order, which is this port's ordering contract.
//! 3. [`the_comparison_reports_a_real_divergence`] feeds the same comparison
//!    deliberately wrong data and asserts it fails, so a green run cannot be vacuous.
//!
//! The `search` comparison is over the **full JSON records** — path, line number,
//! absolute offset, line text and every submatch span — so a port that found the
//! right files with the wrong offsets still diverges.
//!
//! # The oracle truncates its own stdout at 64 KiB
//!
//! Measured, reproducibly, on this machine: under `zuno-testkit`'s scripted
//! environment (a temporary `HOME` and cleared environment) with stdout on a pipe,
//! `debug rg files` over this tree writes **exactly 65536 bytes** and loses the rest,
//! deterministically across runs. The same command with stdout redirected to a file
//! writes all 85108 bytes, and with the host environment intact the pipe receives all
//! 85108 too. It is a flush race in the oracle's exit path, not a search difference —
//! the lost region cuts mid-directory.
//!
//! So no single oracle invocation here is allowed to approach that limit. The tree is
//! covered by [`FILE_PARTITION`], a set of globs whose results are disjoint and whose
//! union is every one of the 5,007 files; each invocation returns at most 1,000 paths
//! (about 17 KB). [`the_union_of_the_partition_is_the_whole_tree`] then compares that
//! union against **one** engine call over the whole tree, so the full-tree comparison
//! is not lost. [`oracle_files`] additionally fails if any invocation's stdout ever
//! approaches 64 KiB, so a future fixture cannot re-enter the silent truncation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use zuno_search::{EmbeddedEngine, GlobRequest, GrepRequest, NeverCancelled};
use zuno_testkit::{Normalizer, Oracle, ScriptedEnv, diff_normalized, pinned_oracle_or_skip};

/// The limit `debug rg` applies when `--limit` is absent
/// (`cli/cmd/debug/ripgrep.ts:40,73`).
const DEBUG_RG_LIMIT: usize = 10_000;

/// The glob `debug rg files` applies when `--glob` is absent
/// (`cli/cmd/debug/ripgrep.ts:39`).
const DEBUG_RG_DEFAULT_GLOB: &str = "**/*";

/// The point at which a captured oracle stdout is treated as suspect.
///
/// The observed loss is at exactly 65536 bytes; this leaves a wide margin so the
/// guard fires before a partition grows into the truncation rather than after.
const STDOUT_BUDGET: usize = 40_000;

/// How many files sit in the `pkg####` directories.
const FIXTURE_PACKAGE_FILES: usize = 5_000;

/// How many files the tree holds in total.
const FIXTURE_FILES: usize = FIXTURE_PACKAGE_FILES + 7;

/// Globs whose results are disjoint and whose union is every file in the tree.
///
/// The five `pkg` globs cover the bulk. The last four cover the cases that make the
/// override precedence observable, and their shape is not incidental: a whitelist
/// glob is matched against directories as well as files, and a hidden or gitignored
/// directory that the glob does not match is *pruned before the walk reaches its
/// children*. `.hidden_dir/**` alone therefore returns nothing, because it does not
/// match `.hidden_dir` itself. `{.hidden_dir,.hidden_dir/**}` matches both and
/// returns the file. Verified against the real binary both ways.
const FILE_PARTITION: &[(&str, usize)] = &[
    ("pkg000?/**", 1_000),
    ("pkg001?/**", 1_000),
    ("pkg002?/**", 1_000),
    ("pkg003?/**", 1_000),
    ("pkg004?/**", 1_000),
    ("**/*.md", 1),
    ("{.gitignore,.hidden_file.ts,ignored.ts}", 3),
    ("{.hidden_dir,.hidden_dir/**}", 1),
    ("{node_modules,node_modules/**}", 1),
    ("{build,build/**}", 1),
];

/// The pinned release, screened by the central oracle.
///
/// A bare `opencode` on `PATH` can be a package-manager launcher that dies under a
/// scripted environment, which is why this file used to address one install path
/// directly. [`pinned_oracle_or_skip`] discovers the route and refuses any candidate
/// that does not report [`zuno_testkit::PINNED_RELEASE`], so the release is pinned
/// without a path in this file selecting one.
fn locate_oracle() -> Option<Oracle> {
    let program = pinned_oracle_or_skip(
        "the zuno-search differentials",
        "no ripgrep behaviour was compared against a real release",
    )?;
    Oracle::at_binary(program).ok()
}

/// Builds the tree.
///
/// The `.git` directory is required, not decoration: `ignore` and ripgrep both
/// default to `require_git`, so a `.gitignore` outside a repository is applied by
/// neither, and a fixture without it would silently test nothing about ignore files.
fn fixture() -> Option<TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("zuno-t41-differential-")
        .tempdir()
        .expect("a temporary directory");
    let root = dir.path();

    if !git_init(root) {
        return None;
    }

    fs::write(
        root.join(".gitignore"),
        "ignored.ts\nnode_modules/\nbuild/\n",
    )
    .expect("the ignore file");

    let per_directory = 100;
    for package in 0..(FIXTURE_PACKAGE_FILES / per_directory) {
        let package_dir = root.join(format!("pkg{package:04}"));
        fs::create_dir_all(&package_dir).expect("a package directory");
        for index in 0..per_directory {
            let extension = if index % 2 == 0 { "ts" } else { "js" };
            let body = if index % 10 == 0 {
                format!("// pkg{package} file{index}\nexport const needle = {index}\n")
            } else {
                format!("// pkg{package} file{index}\nexport const value = {index}\n")
            };
            fs::write(package_dir.join(format!("f{index:04}.{extension}")), body)
                .expect("a package file");
        }
    }

    for (relative, body) in [
        ("ignored.ts", "export const needle = \"gitignored\"\n"),
        (".hidden_file.ts", "export const needle = \"hidden file\"\n"),
        ("README.md", "the needle is here\n"),
    ] {
        fs::write(root.join(relative), body).expect("a top-level fixture file");
    }

    for (directory, name, body) in [
        (
            "node_modules/pkg",
            "index.ts",
            "export const needle = \"gitignored dir\"\n",
        ),
        (
            ".hidden_dir",
            "inner.ts",
            "export const needle = \"hidden dir\"\n",
        ),
        (
            "build",
            "out.js",
            "export const needle = \"gitignored build\"\n",
        ),
    ] {
        let path = root.join(directory);
        fs::create_dir_all(&path).expect("a fixture subdirectory");
        fs::write(path.join(name), body).expect("a fixture file");
    }

    fs::write(
        root.join(".git/description"),
        "the needle must never be read out of the object store\n",
    )
    .expect("a file inside the git directory");

    Some(dir)
}

fn git_init(root: &Path) -> bool {
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .is_ok_and(|status| status.success())
}

struct Harness {
    oracle: Oracle,
    fixture: TempDir,
}

impl Harness {
    /// Builds the fixture and points a scripted-environment oracle at it.
    ///
    /// `None` when the oracle or `git` is unavailable, which every caller reports as
    /// a skip rather than passing quietly.
    fn build() -> Option<Self> {
        let oracle = locate_oracle()?;
        let fixture = fixture()?;
        let env = ScriptedEnv::new()
            .ok()?
            .with_working_dir(fixture.path())
            .ok()?;
        Some(Self {
            oracle: oracle.with_env(env),
            fixture,
        })
    }

    fn root(&self) -> PathBuf {
        self.fixture.path().to_path_buf()
    }

    fn label(&self) -> String {
        format!(
            "oracle {} ({})",
            self.oracle.reported_version(),
            self.oracle.program().display()
        )
    }

    fn files(&self, glob: Option<&str>) -> Vec<String> {
        let mut args = vec!["debug", "rg", "files"];
        if let Some(glob) = glob {
            args.push("--glob");
            args.push(glob);
        }
        let outcome = self.oracle.run(&args).expect("the oracle runs");
        assert!(
            outcome.is_success(),
            "`{}` failed:\n{}",
            args.join(" "),
            outcome.render()
        );
        assert!(
            outcome.stdout.len() < STDOUT_BUDGET,
            "`{}` produced {} bytes of stdout, past the {STDOUT_BUDGET}-byte budget. The oracle \
             silently loses everything past 65536 bytes when stdout is a pipe under a scripted \
             environment (see the module docs), so a comparison this large would be against \
             truncated data. Narrow the partition.",
            args.join(" "),
            outcome.stdout.len()
        );
        outcome
            .stdout
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn matches(&self, pattern: &str, glob: Option<&str>) -> Vec<serde_json::Value> {
        let mut args = vec!["debug", "rg", "search", pattern];
        if let Some(glob) = glob {
            args.push("--glob");
            args.push(glob);
        }
        let outcome = self.oracle.run(&args).expect("the oracle runs");
        assert!(
            outcome.is_success(),
            "`{}` failed:\n{}",
            args.join(" "),
            outcome.render()
        );
        assert!(
            outcome.stdout.len() < STDOUT_BUDGET,
            "`{}` produced {} bytes of stdout, past the {STDOUT_BUDGET}-byte budget; see the \
             module docs on the oracle's 64 KiB stdout truncation.",
            args.join(" "),
            outcome.stdout.len()
        );
        serde_json::from_str::<Vec<serde_json::Value>>(outcome.stdout.trim()).unwrap_or_else(
            |error| {
                panic!(
                    "the oracle's JSON does not parse: {error}\nstdout ({} bytes):\n{}",
                    outcome.stdout.len(),
                    outcome.stdout
                )
            },
        )
    }
}

fn engine_files(root: &Path, pattern: &str) -> Vec<String> {
    let items = EmbeddedEngine
        .glob(
            &GlobRequest::new(root, pattern, DEBUG_RG_LIMIT),
            &NeverCancelled,
        )
        .expect("the glob succeeds")
        .items;

    let paths: Vec<String> = items.into_iter().map(|entry| entry.path).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(
        paths, sorted,
        "the embedded engine must emit path-sorted results for {pattern}"
    );
    paths
}

fn engine_matches(root: &Path, pattern: &str, include: Option<&str>) -> Vec<serde_json::Value> {
    let records: Vec<serde_json::Value> = EmbeddedEngine
        .grep(
            &GrepRequest::new(root, pattern, DEBUG_RG_LIMIT)
                .with_include(include.map(str::to_owned)),
            &NeverCancelled,
        )
        .expect("the grep succeeds")
        .items
        .into_iter()
        .map(|found| serde_json::to_value(found).expect("a match serialises"))
        .collect();

    let keys: Vec<(String, u64, u64)> = records.iter().map(sort_key).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        keys, sorted,
        "the embedded engine must emit matches in path then line order for {pattern}"
    );
    records
}

fn render_paths(mut paths: Vec<String>) -> String {
    paths.sort();
    paths.join("\n")
}

/// Renders match records sorted by path, line and offset, one field per line.
///
/// Pretty-printed so a divergence names the field that differs rather than putting
/// two very long single lines in front of the reader.
fn render_matches(mut records: Vec<serde_json::Value>) -> String {
    records.sort_by_key(sort_key);
    serde_json::to_string_pretty(&records).expect("the records render")
}

fn sort_key(record: &serde_json::Value) -> (String, u64, u64) {
    (
        record
            .pointer("/entry/path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        record
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        record
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    )
}

fn skip(reason: &str) {
    eprintln!("SKIPPED: {reason}");
}

#[test]
fn debug_rg_files_matches_the_embedded_engine_partition_by_partition() {
    let Some(harness) = Harness::build() else {
        skip("no opencode oracle or no git; the files differential did not run");
        return;
    };
    let root = harness.root();

    for (pattern, expected) in FILE_PARTITION {
        let ours = engine_files(&root, pattern);
        let theirs = harness.files(Some(pattern));

        assert_eq!(
            ours.len(),
            *expected,
            "the fixture no longer holds {expected} files for {pattern}; the partition and the \
             tree have to be changed together or the union stops covering everything"
        );

        let report = diff_normalized(
            harness.label(),
            &render_paths(theirs.clone()),
            "zuno-search embedded",
            &render_paths(ours.clone()),
            &Normalizer::default(),
        );
        assert!(
            report.is_identical(),
            "glob {pattern}: {} oracle paths vs {} ours\n{}",
            theirs.len(),
            ours.len(),
            report.render()
        );
    }
}

#[test]
fn the_union_of_the_partition_is_the_whole_tree() {
    let Some(harness) = Harness::build() else {
        skip("no opencode oracle or no git; the whole-tree differential did not run");
        return;
    };
    let root = harness.root();

    let mut theirs: Vec<String> = FILE_PARTITION
        .iter()
        .flat_map(|(pattern, _)| harness.files(Some(pattern)))
        .collect();
    theirs.sort();
    theirs.dedup();

    let ours = engine_files(&root, DEBUG_RG_DEFAULT_GLOB);

    assert_eq!(
        ours.len(),
        FIXTURE_FILES,
        "the engine must see every file in the tree in one call"
    );

    let report = diff_normalized(
        harness.label(),
        &render_paths(theirs.clone()),
        "zuno-search embedded",
        &render_paths(ours.clone()),
        &Normalizer::default(),
    );
    assert!(
        report.is_identical(),
        "the union of {} oracle invocations ({} paths) differs from one engine call ({} paths)\n{}",
        FILE_PARTITION.len(),
        theirs.len(),
        ours.len(),
        report.render()
    );
}

#[test]
fn debug_rg_search_matches_the_embedded_engine_field_for_field() {
    let Some(harness) = Harness::build() else {
        skip("no opencode oracle or no git; the search differential did not run");
        return;
    };
    let root = harness.root();

    // Every case walks the whole 5,007-file tree; the patterns are chosen to match
    // few *lines*, which keeps the oracle's JSON well inside its stdout budget
    // without narrowing what is searched.
    let cases: [(&str, Option<&str>); 5] = [
        ("export const needle = 0$", None),
        ("needle", Some("pkg000[0-4]/**")),
        ("needle", Some("*.md")),
        ("needle", Some("{.hidden_dir,.hidden_dir/**}")),
        ("needle", Some("{build,build/**}")),
    ];

    for (pattern, include) in cases {
        let ours = engine_matches(&root, pattern, include);
        let theirs = harness.matches(pattern, include);

        let report = diff_normalized(
            harness.label(),
            &render_matches(theirs.clone()),
            "zuno-search embedded",
            &render_matches(ours.clone()),
            &Normalizer::default(),
        );
        assert!(
            report.is_identical(),
            "grep {pattern} include {include:?}: {} oracle matches vs {} ours\n{}",
            theirs.len(),
            ours.len(),
            report.render()
        );
        assert!(
            !ours.is_empty(),
            "grep {pattern} include {include:?} matched nothing on either side, which would make \
             the comparison vacuous"
        );
    }
}

#[test]
fn a_pattern_with_no_matches_is_an_empty_result_on_both_sides() {
    let Some(harness) = Harness::build() else {
        skip("no opencode oracle or no git; the empty-result differential did not run");
        return;
    };

    let ours = engine_matches(&harness.root(), "zzzznomatchzzzz", None);
    assert!(ours.is_empty(), "the engine returns no matches");

    let outcome = harness
        .oracle
        .run(["debug", "rg", "search", "zzzznomatchzzzz"])
        .expect("the oracle runs");
    assert!(
        outcome.is_success(),
        "a pattern with no matches must exit zero, not fail:\n{}",
        outcome.render()
    );
    assert_eq!(
        outcome.stdout.trim(),
        "[]",
        "the oracle returns an empty array"
    );

    let theirs: Vec<serde_json::Value> =
        serde_json::from_str(outcome.stdout.trim()).expect("the oracle's JSON parses");
    let report = diff_normalized(
        harness.label(),
        &render_matches(theirs),
        "zuno-search embedded",
        &render_matches(ours),
        &Normalizer::default(),
    );
    assert!(report.is_identical(), "{}", report.render());
}

#[test]
fn the_comparison_reports_a_real_divergence() {
    // The negative control. Without it, a bug that made both sides equally wrong — or
    // a renderer that collapsed everything to an empty string — would pass silently.
    let normalizer = Normalizer::default();
    let truth = vec!["a.ts".to_owned(), "b.ts".to_owned(), "c.ts".to_owned()];

    let missing = diff_normalized(
        "oracle",
        &render_paths(truth.clone()),
        "subject",
        &render_paths(vec!["a.ts".to_owned(), "c.ts".to_owned()]),
        &normalizer,
    );
    assert!(
        !missing.is_identical(),
        "a missing path must be reported: {}",
        missing.render()
    );

    let altered = diff_normalized(
        "oracle",
        &render_paths(truth),
        "subject",
        &render_paths(vec![
            "a.ts".to_owned(),
            "b.tsx".to_owned(),
            "c.ts".to_owned(),
        ]),
        &normalizer,
    );
    assert!(!altered.is_identical(), "an altered path must be reported");

    let record = |start: u64, end: u64| {
        vec![serde_json::json!({
            "entry": { "path": "a.ts", "type": "file" },
            "line": 1,
            "offset": 0,
            "text": "needle\n",
            "submatches": [{ "text": "needle", "start": start, "end": end }],
        })]
    };
    let spans = diff_normalized(
        "oracle",
        &render_matches(record(0, 6)),
        "subject",
        &render_matches(record(1, 7)),
        &normalizer,
    );
    assert!(
        !spans.is_identical(),
        "a wrong submatch span must be reported, which is what makes the field-for-field \
         comparison meaningful"
    );
}
