//! Enforces that library crates carry typed errors, not dynamically typed ones.
//!
//! A dynamically typed error erases the structure `oc-error` exists to preserve: a
//! caller holding one cannot match on a variant or read a field, so its only
//! remaining option is to inspect rendered text — the exact defect this crate was
//! written to prevent.
//!
//! Two crates are exempt, both at the process edge where a failure is about to be
//! printed and nothing downstream will ever branch on it again:
//!
//! - `oc-cli`, which formats a failure and sets an exit code.
//! - `oc-testkit`, whose harnesses fail a test rather than recover.
//!
//! The scan is textual on purpose. It reports a violation in a crate that does not
//! currently compile, which is when the report is most useful, and it needs no
//! dependency on the crate under inspection.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const EXEMPT_CRATES: &[&str] = &["oc-cli", "oc-testkit"];

const BANNED_TOKENS: &[&str] = &["anyhow::", "anyhow!", "use anyhow", "extern crate anyhow"];

/// Floors, not exact counts: crates gain modules as the workspace grows, so exact
/// numbers would be a maintenance tax. The floors exist so that a scanner which
/// silently walks the wrong directory fails loudly instead of passing vacuously.
const MINIMUM_CRATES: usize = 33;
const MINIMUM_SOURCE_FILES: usize = 33;

#[derive(Debug)]
struct Violation {
    crate_name: String,
    file: PathBuf,
    line_number: usize,
    line: String,
    token: &'static str,
}

fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR is always <root>/crates/oc-error")
        .to_path_buf()
}

/// Removes trailing line comments so that prose *about* the ban — this file's own
/// documentation, or a note in another crate explaining the rule — does not read as
/// a violation.
///
/// The `"` guard keeps a URL such as `https://example.com` from truncating the rest
/// of a line. Its only failure mode is a missed detection on a line that both opens
/// a string literal and then uses a banned token, which cannot happen in an import.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) if !line[..at].contains('"') => &line[..at],
        _ => line,
    }
}

/// True when a manifest line declares the banned dependency, in any of the three
/// spellings cargo accepts, and false when the line merely mentions it in a comment.
///
/// Truncating at `#` can only cause a missed detection (a `#` inside a string
/// value), never a false accusation, which is the right way round for a guard that
/// gates every commit.
fn declares_banned_dependency(manifest_line: &str) -> bool {
    manifest_line
        .split('#')
        .next()
        .unwrap_or_default()
        .trim_start()
        .starts_with("anyhow")
}

/// Yields `crates/<name>/src/**/*.rs`, and nothing else. A crate's `tests/`,
/// `benches/` and `examples/` are out of scope: this rule is about what a library
/// hands to its callers.
fn library_sources(root: &Path) -> Vec<(String, PathBuf)> {
    let crates_dir = root.join("crates");
    let mut found = Vec::new();

    for entry in WalkDir::new(&crates_dir).into_iter().flatten() {
        let path = entry.path();
        if !entry.file_type().is_file() || path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }

        let relative = path
            .strip_prefix(&crates_dir)
            .expect("walked under crates_dir");
        let mut components = relative.components();
        let Some(crate_name) = components.next() else {
            continue;
        };
        if components.next().map(|c| c.as_os_str()) != Some("src".as_ref()) {
            continue;
        }

        found.push((
            crate_name.as_os_str().to_string_lossy().into_owned(),
            path.to_path_buf(),
        ));
    }

    found.sort();
    found
}

fn scan(root: &Path) -> (Vec<Violation>, usize, BTreeSet<String>) {
    let sources = library_sources(root);
    let mut violations = Vec::new();
    let mut crates_seen = BTreeSet::new();

    for (crate_name, path) in &sources {
        crates_seen.insert(crate_name.clone());
        if EXEMPT_CRATES.contains(&crate_name.as_str()) {
            continue;
        }

        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

        for (index, raw_line) in contents.lines().enumerate() {
            let code = strip_line_comment(raw_line);
            if let Some(token) = BANNED_TOKENS.iter().find(|token| code.contains(*token)) {
                violations.push(Violation {
                    crate_name: crate_name.clone(),
                    file: path.to_path_buf(),
                    line_number: index + 1,
                    line: raw_line.trim().to_owned(),
                    token,
                });
            }
        }
    }

    (violations, sources.len(), crates_seen)
}

#[test]
fn no_library_crate_uses_a_dynamically_typed_error() {
    let root = workspace_root();
    let (violations, files_scanned, crates_seen) = scan(&root);

    assert!(
        files_scanned >= MINIMUM_SOURCE_FILES,
        "scanned only {files_scanned} source files under {}; \
         the scan is looking in the wrong place and would pass vacuously",
        root.join("crates").display()
    );

    if !violations.is_empty() {
        let mut report = format!(
            "{} library crate source file(s) use a dynamically typed error.\n\
             Library crates must return a typed error from `oc-error`; only {} are exempt.\n\n",
            violations.len(),
            EXEMPT_CRATES.join(" and ")
        );
        for v in &violations {
            report.push_str(&format!(
                "  {}:{}  [crate {}]  matched {:?}\n    {}\n",
                v.file.display(),
                v.line_number,
                v.crate_name,
                v.token,
                v.line
            ));
        }
        panic!("{report}");
    }

    assert!(
        crates_seen.len() >= MINIMUM_CRATES,
        "expected at least {MINIMUM_CRATES} crates under {}, saw {}: {crates_seen:?}",
        root.join("crates").display(),
        crates_seen.len()
    );
}

#[test]
fn no_library_crate_declares_the_dependency_at_all() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut offenders = Vec::new();
    let mut manifests_checked = 0_usize;

    for entry in std::fs::read_dir(&crates_dir).expect("crates/ is readable") {
        let dir = entry.expect("readable dir entry").path();
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        manifests_checked += 1;

        let crate_name = dir
            .file_name()
            .expect("crate directory has a name")
            .to_string_lossy()
            .into_owned();
        if EXEMPT_CRATES.contains(&crate_name.as_str()) {
            continue;
        }

        let contents = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", manifest.display()));
        for (index, raw_line) in contents.lines().enumerate() {
            if declares_banned_dependency(raw_line) {
                offenders.push(format!(
                    "  {}:{}  [crate {}]\n    {}",
                    manifest.display(),
                    index + 1,
                    crate_name,
                    raw_line.trim()
                ));
            }
        }
    }

    assert!(
        manifests_checked >= MINIMUM_CRATES,
        "checked only {manifests_checked} manifests under {}; the scan is looking in the wrong place",
        crates_dir.display()
    );
    assert!(
        offenders.is_empty(),
        "{} library crate manifest(s) declare a dynamically typed error dependency; \
         only {} may:\n{}",
        offenders.len(),
        EXEMPT_CRATES.join(" and "),
        offenders.join("\n")
    );
}

#[test]
fn the_scanner_detects_a_violation_when_one_is_present() {
    let cases = [
        "use anyhow::Result;",
        "use anyhow::{Context, Result};",
        "fn f() -> anyhow::Result<()> { Ok(()) }",
        "return Err(anyhow!(\"boom\"));",
        "extern crate anyhow;",
    ];
    for case in cases {
        let code = strip_line_comment(case);
        assert!(
            BANNED_TOKENS.iter().any(|token| code.contains(token)),
            "scanner missed a violation in {case:?}"
        );
    }
}

#[test]
fn the_scanner_ignores_prose_about_the_ban() {
    let cases = [
        "// do not use anyhow::Result in a library crate",
        "//! Library crates never return anyhow::Error.",
        "    /// See anyhow! for what this crate deliberately avoids.",
    ];
    for case in cases {
        let code = strip_line_comment(case);
        assert!(
            !BANNED_TOKENS.iter().any(|token| code.contains(token)),
            "scanner reported a false violation in {case:?}"
        );
    }
}

#[test]
fn the_manifest_scanner_detects_every_spelling_cargo_accepts() {
    for declaration in [
        "anyhow = \"1.0\"",
        "anyhow = { workspace = true }",
        "anyhow.workspace = true",
        "  anyhow = { version = \"1.0\", features = [\"backtrace\"] }",
    ] {
        assert!(
            declares_banned_dependency(declaration),
            "manifest scanner missed {declaration:?}"
        );
    }

    for innocent in [
        "# anyhow = \"1.0\"",
        "thiserror = { workspace = true }",
        "oc-error = { workspace = true }",
        "description = \"why anyhow is not used here\"",
        "",
    ] {
        assert!(
            !declares_banned_dependency(innocent),
            "manifest scanner falsely accused {innocent:?}"
        );
    }
}

#[test]
fn a_url_before_a_violation_does_not_hide_it() {
    let line = "let doc = \"https://docs.rs/anyhow\"; use anyhow::Result;";
    let code = strip_line_comment(line);
    assert!(
        BANNED_TOKENS.iter().any(|token| code.contains(token)),
        "a string literal containing // must not truncate the scanned line"
    );
}
