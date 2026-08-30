//! The three states a diagnostic report must keep distinct.

use super::*;

fn error(line: u32, message: &str) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        line,
        column: 5,
        source: Some(String::from("rustc")),
        message: message.to_owned(),
    }
}

fn warning(line: u32, message: &str) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        line,
        column: 1,
        source: Some(String::from("clippy")),
        message: message.to_owned(),
    }
}

fn text(report: &Report, width: u16, limit: usize) -> Vec<String> {
    report
        .lines(width, limit, &ViewContext::defaults())
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn lsp_severity_treats_an_absent_number_as_an_error() {
    // A message with no stated severity that turns out to be a compile failure is the
    // expensive direction to be wrong in.
    assert_eq!(Severity::from_lsp(None), Severity::Error);
    assert_eq!(Severity::from_lsp(Some(1)), Severity::Error);
    assert_eq!(Severity::from_lsp(Some(2)), Severity::Warning);
    assert_eq!(Severity::from_lsp(Some(3)), Severity::Information);
    assert_eq!(Severity::from_lsp(Some(4)), Severity::Hint);
    assert_eq!(
        Severity::from_lsp(Some(99)),
        Severity::Error,
        "an unknown severity must not be silently downgraded"
    );
}

#[test]
fn lsp_an_unchecked_file_never_reads_as_a_clean_one() {
    // This is the assertion the whole module exists for: "no diagnostics" on a file no
    // server is checking is a false clean bill of health, and it is a claim about
    // correctness rather than about a list being empty.
    let unchecked = Report::unchecked("src/main.zig");
    let clean = Report::checked("src/main.rs", "rust", Vec::new());
    assert_ne!(unchecked.summary(), clean.summary());
    assert!(
        unchecked.summary().contains("no language server"),
        "{}",
        unchecked.summary()
    );
    assert!(
        clean.summary().contains("no problems"),
        "{}",
        clean.summary()
    );
    assert!(!unchecked.is_checked());
    assert!(clean.is_checked());
}

#[test]
fn lsp_clean_summary_uses_neutral_text_instead_of_a_green_sentence() {
    let context = ViewContext::defaults();
    let report = Report::checked("src/main.rs", "rust", Vec::new());
    let line = report
        .lines(80, 5, &context)
        .into_iter()
        .next()
        .expect("the clean summary row");
    let summary = line
        .spans
        .iter()
        .find(|span| span.content.contains("no problems"))
        .expect("the clean summary text");
    assert_eq!(
        summary.style.fg,
        context.text().fg,
        "the complete no-problems sentence was painted green"
    );
}

#[test]
fn lsp_summary_counts_each_severity_and_names_the_server() {
    let report = Report::checked(
        "src/lib.rs",
        "rust",
        vec![
            warning(9, "unused import"),
            error(3, "mismatched types"),
            error(7, "cannot find value"),
            Diagnostic {
                severity: Severity::Hint,
                line: 1,
                column: 1,
                source: None,
                message: String::from("consider renaming"),
            },
        ],
    );
    let summary = report.summary();
    assert!(summary.contains("2 errors"), "{summary}");
    assert!(summary.contains("1 warning"), "{summary}");
    assert!(summary.contains("1 other"), "{summary}");
    assert!(summary.contains("(rust)"), "{summary}");
    assert!(summary.starts_with("src/lib.rs:"), "{summary}");
}

#[test]
fn lsp_summary_uses_the_singular_for_one() {
    let report = Report::checked("a.rs", "rust", vec![error(1, "boom")]);
    assert!(
        report.summary().contains("1 error ("),
        "{}",
        report.summary()
    );
    assert!(
        !report.summary().contains("1 errors"),
        "{}",
        report.summary()
    );
}

#[test]
fn lsp_orders_errors_before_warnings_and_then_by_position() {
    let report = Report::checked(
        "src/lib.rs",
        "rust",
        vec![
            warning(2, "second warning"),
            error(90, "late error"),
            warning(1, "first warning"),
            error(4, "early error"),
        ],
    );
    let messages = report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "early error",
            "late error",
            "first warning",
            "second warning"
        ],
        "the row a user reads first must be the one that stops the build"
    );
}

#[test]
fn lsp_renders_a_row_per_diagnostic_with_a_one_based_position() {
    // One-based because LSP ranges are zero-based and every editor is not; a report whose
    // line numbers are off by one is worse than none.
    let report = Report::checked("src/lib.rs", "rust", vec![error(42, "mismatched types")]);
    let rows = text(&report, 120, 10);
    assert!(rows.iter().any(|row| row.contains("42:5")), "{rows:?}");
    assert!(
        rows.iter().any(|row| row.contains("mismatched types")),
        "{rows:?}"
    );
    assert!(rows.iter().any(|row| row.contains("[rustc]")), "{rows:?}");
    assert!(rows.iter().any(|row| row.contains("error")), "{rows:?}");
}

#[test]
fn lsp_caps_its_rows_and_says_how_many_are_hidden() {
    let report = Report::checked(
        "src/lib.rs",
        "rust",
        (1..=40).map(|line| error(line, "boom")).collect(),
    );
    let rows = text(&report, 120, 5);
    // One summary, five diagnostics, one "more" row.
    assert_eq!(rows.len(), 7, "{rows:?}");
    assert!(
        rows.last().is_some_and(|row| row.contains("… 35 more")),
        "a truncated report must state the count: {rows:?}"
    );
}

#[test]
fn lsp_rows_fill_the_width_they_were_given_even_with_wide_glyphs() {
    let report = Report::checked(
        "源码/主模块.rs",
        "rust",
        vec![Diagnostic {
            severity: Severity::Error,
            line: 12,
            column: 3,
            source: Some(String::from("rustc")),
            message: "类型不匹配：期望 String，实际 &str，请显式转换".repeat(3),
        }],
    );
    for width in [60_u16, 120, 200] {
        for line in report.lines(width, 10, &ViewContext::defaults()) {
            let used: usize = line
                .spans
                .iter()
                .map(|span| crate::views::display_width(&span.content))
                .sum();
            assert_eq!(used, usize::from(width), "at width {width}");
        }
    }
}

#[test]
fn lsp_paints_a_clean_report_differently_from_a_failing_one() {
    let context = ViewContext::defaults();
    let clean = Report::checked("a.rs", "rust", Vec::new());
    let failing = Report::checked("a.rs", "rust", vec![error(1, "boom")]);
    let style_of = |report: &Report| {
        report.lines(80, 10, &context)[0].spans[0]
            .style
            .fg
            .expect("a foreground")
    };
    assert_ne!(
        style_of(&clean),
        style_of(&failing),
        "a clean file and a broken one must not look the same"
    );
    assert_ne!(
        style_of(&Report::unchecked("a.zig")),
        style_of(&clean),
        "an unchecked file must not look like a clean one"
    );
}
