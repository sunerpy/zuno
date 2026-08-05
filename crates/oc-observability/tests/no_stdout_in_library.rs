//! Keeps the library half of this crate free of stdout, permanently.
//!
//! `tests/stdout_purity.rs` proves the guarantee holds for the code that exists today
//! by running a real process. This test protects the *next* author: a `println!`
//! added while debugging, or a `fmt::layer()` left with its stdout default, would
//! reintroduce the leak, and the runtime test only catches it if the new code happens
//! to run on a covered path.
//!
//! It is the same shape as `oc-error/tests/no_anyhow_in_libraries.rs` — a textual
//! scan that reports a violation even in a crate that does not currently compile,
//! which is when the report is most useful.
//!
//! # The one exemption
//!
//! `src/bin/oc-log-probe.rs` writes to stdout deliberately: it stands in for an ACP
//! peer, and there is no way to prove logs stay off stdout without something putting
//! protocol frames there. It is a test fixture, not library code, and it is named
//! here so the exemption is a decision rather than an oversight.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Relative to `src/`. The only file allowed to touch stdout.
const EXEMPT: &[&str] = &["bin/oc-log-probe.rs"];

/// Every way to reach stdout that does not require `unsafe`, which the workspace
/// forbids outright.
const BANNED_TOKENS: &[&str] = &[
    "println!",
    "print!",
    "io::stdout",
    "stdout()",
    "Stdout",
    "with_writer(std::io::stdout",
];

/// A floor, not an exact count. It exists so that a scan pointed at the wrong
/// directory fails loudly instead of passing vacuously.
const MINIMUM_SOURCE_FILES: usize = 5;

#[derive(Debug)]
struct Violation {
    file: PathBuf,
    line_number: usize,
    line: String,
    token: &'static str,
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Removes trailing line comments so that prose *about* the ban — this crate's own
/// documentation of the rule — does not read as a violation.
///
/// The `"` guard keeps a URL from truncating the rest of a line. Its only failure mode
/// is a missed detection on a line that opens a string literal and then reaches
/// stdout, which cannot happen in a `println!`.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) if !line[..at].contains('"') => &line[..at],
        _ => line,
    }
}

/// The banned token in `code`, if any, matched only at an identifier boundary.
///
/// The boundary check is what keeps `eprintln!` — the legitimate stderr diagnostic —
/// from matching the banned `print!`, since `print!` is a substring of it. Without it
/// the rule would forbid the very thing it wants people to use instead, which is how a
/// guard ends up disabled.
fn banned_token(code: &str) -> Option<&'static str> {
    BANNED_TOKENS.iter().copied().find(|token| {
        code.match_indices(token).any(|(at, _)| {
            at == 0
                || !code[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
}

fn scan() -> (Vec<Violation>, usize, usize) {
    let src = src_dir();
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    let mut exempted = 0_usize;

    for entry in WalkDir::new(&src).into_iter().flatten() {
        let path = entry.path();
        if !entry.file_type().is_file() || path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }

        let relative = path
            .strip_prefix(&src)
            .expect("walked under src")
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT.contains(&relative.as_str()) {
            exempted += 1;
            continue;
        }
        scanned += 1;

        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for (index, raw_line) in contents.lines().enumerate() {
            let code = strip_line_comment(raw_line);
            if let Some(token) = banned_token(code) {
                violations.push(Violation {
                    file: path.to_path_buf(),
                    line_number: index + 1,
                    line: raw_line.trim().to_owned(),
                    token,
                });
            }
        }
    }

    (violations, scanned, exempted)
}

#[test]
fn no_library_source_file_can_reach_stdout() {
    let (violations, scanned, _) = scan();

    assert!(
        scanned >= MINIMUM_SOURCE_FILES,
        "scanned only {scanned} files under {}; the scan is looking in the wrong place \
         and would pass vacuously",
        src_dir().display()
    );

    if !violations.is_empty() {
        let mut report = format!(
            "{} source line(s) can write to stdout.\n\
             ACP and any stdio protocol frame JSON on stdout, so a log byte there is a \
             corrupt frame and the editor disconnects. Route output to stderr or to the \
             log file instead.\n\
             Only {} may touch stdout.\n\n",
            violations.len(),
            EXEMPT.join(", ")
        );
        for v in &violations {
            report.push_str(&format!(
                "  {}:{}  matched {:?}\n    {}\n",
                v.file.display(),
                v.line_number,
                v.token,
                v.line
            ));
        }
        panic!("{report}");
    }
}

/// The exemption has to exist. If `oc-log-probe.rs` were renamed or deleted, the
/// exemption list would silently stop matching anything and the runtime guarantee
/// would lose the only test that can check it.
#[test]
fn the_exempt_fixture_still_exists() {
    let (_, _, exempted) = scan();
    assert_eq!(
        exempted,
        EXEMPT.len(),
        "every exempt path must exist; {EXEMPT:?} matched {exempted} file(s) under {}",
        src_dir().display()
    );
}

/// A scanner that cannot detect a violation is worse than no scanner, because it
/// reads as a passing guarantee.
#[test]
fn the_scanner_detects_every_way_to_reach_stdout() {
    for case in [
        r#"println!("debugging");"#,
        r#"print!("no newline");"#,
        "let mut out = std::io::stdout();",
        "writeln!(io::stdout(), \"x\")",
        "fn writer() -> Stdout { std::io::stdout() }",
        ".with_writer(std::io::stdout)",
    ] {
        let code = strip_line_comment(case);
        assert!(
            banned_token(code).is_some(),
            "scanner missed a violation in {case:?}"
        );
    }
}

#[test]
fn the_scanner_ignores_prose_about_the_ban() {
    for case in [
        "// never call println! in library code",
        "//! fmt::layer() defaults to stdout, so with_writer is mandatory",
        "    /// Routed to stderr, never io::stdout.",
    ] {
        let code = strip_line_comment(case);
        assert!(
            banned_token(code).is_none(),
            "scanner reported a false violation in {case:?}"
        );
    }
}

/// The stderr sink is the legitimate terminal destination, so the scan must not
/// mistake it for a violation. Otherwise the rule would be unimplementable.
#[test]
fn the_scanner_permits_the_stderr_sink() {
    for case in [
        ".with_writer(std::io::stderr)",
        "eprintln!(\"a real diagnostic\");",
        "std::io::stderr().is_terminal()",
    ] {
        let code = strip_line_comment(case);
        assert!(
            banned_token(code).is_none(),
            "scanner falsely accused the stderr sink in {case:?}"
        );
    }
}
