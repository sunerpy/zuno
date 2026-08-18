use super::*;
use crate::theme::{Mode, Palette, Rgba, ThemeRegistry};

fn palette() -> Palette {
    ThemeRegistry::new()
        .resolve(crate::theme::DEFAULT_THEME, Mode::Dark)
        .palette
}

fn span_with(rows: &[Row], needle: &str) -> Span<'static> {
    rows.iter()
        .flatten()
        .find(|span| span.content == needle)
        .cloned()
        .unwrap_or_else(|| panic!("no span equals {needle:?} in {rows:?}"))
}

fn assert_colour(language: &str, source: &str, needle: &str, colour: Rgba) {
    let palette = palette();
    let rows = spans(Some(language), source, &palette)
        .unwrap_or_else(|| panic!("{language} did not produce highlighted rows"));
    assert_eq!(
        span_with(&rows, needle).style.fg,
        Some(colour.into()),
        "{language} did not highlight {needle:?} with its capture token"
    );
}

#[test]
fn highlight_capture_prefixes_follow_the_plan_palette_table() {
    let palette = palette();
    let cases = [
        ("comment", palette.syntax_comment, true),
        ("comment.documentation", palette.syntax_comment, true),
        ("string", palette.syntax_string, false),
        ("string.special", palette.syntax_string, false),
        ("number", palette.syntax_number, false),
        ("boolean", palette.syntax_number, false),
        ("constant.builtin", palette.syntax_number, false),
        ("keyword.return", palette.syntax_keyword, true),
        ("function.method", palette.syntax_function, false),
        ("method.call", palette.syntax_function, false),
        ("constructor", palette.syntax_function, false),
        ("type.builtin", palette.syntax_type, false),
        ("class", palette.syntax_type, false),
        ("module", palette.syntax_type, false),
        ("operator", palette.syntax_operator, false),
        ("punctuation.bracket", palette.syntax_punctuation, false),
        ("variable", palette.text, false),
    ];
    for (capture, colour, italic) in cases {
        let style = capture_style(capture, &palette);
        assert_eq!(style.fg, Some(colour.into()), "wrong token for {capture}");
        assert_eq!(
            style.add_modifier.contains(Modifier::ITALIC),
            italic,
            "wrong italic modifier for {capture}"
        );
    }
}

#[test]
fn highlight_all_eight_language_families_and_typescript_aliases() {
    let palette = palette();
    assert_colour("rust", "fn main() {}", "fn", palette.syntax_keyword);
    assert_colour(
        "python",
        "def main():\n    pass",
        "def",
        palette.syntax_keyword,
    );
    assert_colour("go", "func main() {}", "func", palette.syntax_keyword);
    assert_colour(
        "typescript",
        "interface User { name: string }",
        "interface",
        palette.syntax_keyword,
    );
    assert_colour(
        "javascript",
        "const answer = 42;",
        "const",
        palette.syntax_keyword,
    );
    assert_colour(
        "tsx",
        "const view = <Panel />;",
        "const",
        palette.syntax_keyword,
    );
    assert_colour("json", "{\"answer\": 42}", "42", palette.syntax_number);
    assert_colour("yaml", "answer: 42", "42", palette.syntax_number);
    assert_colour("toml", "answer = 42", "=", palette.syntax_operator);
    assert_colour("markdown", "\\* escaped\n", "\\*", palette.syntax_string);
}

#[test]
fn highlight_rust_captures_keyword_string_comment_function_type_operator_and_punctuation() {
    let palette = palette();
    let source =
        "// note\nfn paint(value: Widget) -> i32 { println!(\"blue\"); value * 42; Widget::new() }";
    let rows = spans(Some("rust"), source, &palette).expect("Rust is supported");
    for (needle, colour) in [
        ("// note", palette.syntax_comment),
        ("fn", palette.syntax_keyword),
        ("paint", palette.syntax_function),
        ("Widget", palette.syntax_type),
        ("\"blue\"", palette.syntax_string),
        ("42", palette.syntax_number),
        ("*", palette.syntax_operator),
        ("::", palette.syntax_punctuation),
    ] {
        assert_eq!(span_with(&rows, needle).style.fg, Some(colour.into()));
    }
    assert!(
        span_with(&rows, "// note")
            .style
            .add_modifier
            .contains(Modifier::ITALIC)
    );
    assert!(
        span_with(&rows, "fn")
            .style
            .add_modifier
            .contains(Modifier::ITALIC)
    );
}

#[test]
fn highlight_unknown_or_absent_language_requests_plain_fallback() {
    let palette = palette();
    assert!(spans(Some("brainfuck"), "+[-->]", &palette).is_none());
    assert!(spans(None, "fn main() {}", &palette).is_none());
}

#[test]
fn highlight_rejects_a_block_beyond_512_kib_before_parsing() {
    let palette = palette();
    let line = format!("//{}\n", "x".repeat(MAX_HIGHLIGHT_LINE_BYTES - 2));
    let source = line.repeat(MAX_HIGHLIGHT_BYTES / line.len() + 1);
    assert!(source.len() > MAX_HIGHLIGHT_BYTES);
    assert!(source.split('\n').count() < MAX_HIGHLIGHT_LINES);
    assert!(spans(Some("rust"), &source, &palette).is_none());
}

#[test]
fn highlight_rejects_more_than_10_000_lines_before_parsing() {
    let palette = palette();
    let source = "x\n".repeat(MAX_HIGHLIGHT_LINES);
    assert_eq!(source.split('\n').count(), MAX_HIGHLIGHT_LINES + 1);
    assert!(source.len() < MAX_HIGHLIGHT_BYTES);
    assert!(spans(Some("rust"), &source, &palette).is_none());
}

#[test]
fn highlight_leaves_only_the_line_beyond_4_kib_plain() {
    let palette = palette();
    let source = format!(
        "//{}\nfn small() {{}}",
        "x".repeat(MAX_HIGHLIGHT_LINE_BYTES)
    );
    let rows = spans(Some("rust"), &source, &palette).expect("the block itself is bounded");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][0].style.fg,
        Some(palette.markdown_code_block.into())
    );
    assert_eq!(
        span_with(&rows[1..], "fn").style.fg,
        Some(palette.syntax_keyword.into())
    );
}

#[test]
fn filetype_detection_uses_basename_before_suffixes() {
    assert_eq!(detect_filetype("Dockerfile"), Some("dockerfile"));
    assert_eq!(
        detect_filetype("containers/Dockerfile.dev"),
        Some("dockerfile")
    );
    assert_eq!(detect_filetype("Makefile"), Some("make"));
    assert_eq!(detect_filetype("Cargo.lock"), Some("toml"));
    assert_eq!(detect_filetype(".gitignore"), Some("gitignore"));
}

#[test]
fn filetype_detection_prefers_the_longest_compound_suffix() {
    assert_eq!(detect_filetype("view.html.erb"), Some("erb"));
    assert_eq!(detect_filetype("types.d.ts"), Some("typescript"));
    assert_eq!(detect_filetype("archive.tar.gz"), Some("gzip"));
}

#[test]
fn filetype_detection_falls_back_to_one_ordinary_extension() {
    assert_eq!(detect_filetype("src/main.rs"), Some("rust"));
    assert_eq!(detect_filetype("config/settings.yaml"), Some("yaml"));
    assert_eq!(detect_filetype("README.md"), Some("markdown"));
    assert_eq!(detect_filetype("no-extension"), None);
}
