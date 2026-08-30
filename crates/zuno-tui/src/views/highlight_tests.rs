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

/// One snippet per grammar, with a token whose capture is asserted.
///
/// The name is the primary alias, which is also what proves the alias resolves.
const SAMPLES: [(Grammar, &str, &str, &str); 16] = [
    (Grammar::Rust, "rust", "fn main() {}", "fn"),
    (Grammar::Python, "py", "def main():\n    pass", "def"),
    (Grammar::Go, "golang", "func main() {}", "func"),
    (
        Grammar::TypeScript,
        "ts",
        "interface User { name: string }",
        "interface",
    ),
    (Grammar::JavaScript, "js", "const answer = 42;", "const"),
    (Grammar::Tsx, "tsx", "const view = <Panel />;", "const"),
    (Grammar::Json, "jsonc", "{\"answer\": 42}", "42"),
    (Grammar::Yaml, "yml", "answer: 42", "42"),
    (Grammar::Toml, "toml", "answer = 42", "="),
    (Grammar::Markdown, "md", "\\* escaped\n", "\\*"),
    (Grammar::Bash, "sh", "echo \"hi\"\n", "echo"),
    (Grammar::C, "c", "int main(void) { return 0; }", "return"),
    (
        Grammar::Cpp,
        "c++",
        "class Panel { public: int rows; };",
        "class",
    ),
    (
        Grammar::Java,
        "java",
        "class Panel { void draw() {} }",
        "class",
    ),
    (
        Grammar::Php,
        "php",
        "<?php function draw() {} ?>",
        "function",
    ),
    (Grammar::Ruby, "rb", "def draw\n  nil\nend", "def"),
];

#[test]
fn highlight_every_grammar_in_the_table_produces_spans_for_its_own_alias() {
    // Derived over `ALL_GRAMMARS`, not a hand-written list of languages. P2-2 shipped ten
    // and its test named ten; a batch that adds eight is exactly when a hand-list stops
    // covering the table, which is the failure the diff scope's nine-of-eleven comment was.
    assert_eq!(
        SAMPLES.len(),
        ALL_GRAMMARS.len(),
        "the sample table has {} entries for {} grammars",
        SAMPLES.len(),
        ALL_GRAMMARS.len()
    );
    for (index, grammar) in ALL_GRAMMARS.into_iter().enumerate() {
        // The compile-enforced half: a variant added to `Grammar` must be given an ordinal,
        // and an ordinal outside this array means it was never added to it.
        assert_eq!(
            grammar_ordinal(grammar),
            index,
            "{grammar:?} sits at index {index} but its ordinal disagrees, so `ALL_GRAMMARS` \
             is missing a variant or lists one twice"
        );
        let sample = SAMPLES
            .iter()
            .find(|(candidate, ..)| *candidate == grammar)
            .unwrap_or_else(|| {
                panic!("{grammar:?} has no sample, so nothing proves its grammar loads")
            });
        let (_, alias, source, needle) = sample;
        assert_eq!(
            Grammar::detect(alias),
            Some(grammar),
            "`{alias}` does not resolve to {grammar:?}, so a fence written that way falls \
             back to plain text"
        );
        let palette = palette();
        let rows = spans(Some(alias), source, &palette)
            .unwrap_or_else(|| panic!("{grammar:?} produced no highlighted rows for {alias}"));
        let span = rows
            .iter()
            .flatten()
            .find(|span| span.content == *needle)
            .unwrap_or_else(|| {
                panic!("{grammar:?} did not emit `{needle}` as its own span: {rows:?}")
            });
        // The token carries *some* capture colour rather than the plain body colour, which
        // is what says the query loaded and matched. Which token maps to which palette
        // entry is upstream's query's business and differs per language.
        assert_ne!(
            span.style.fg,
            Some(palette.text.into()),
            "{grammar:?} emitted `{needle}` unhighlighted, so its highlights query did not \
             match — the grammar loads but paints nothing"
        );
    }
}

#[test]
fn highlight_ships_the_measured_grammars_and_names_the_ones_it_left_out() {
    // §7.2's second batch is c/cpp/java/ruby/php/html/css/sql/diff. Everything but the last
    // two is present, and this states the pair so a future reader does not re-litigate it
    // from memory. `tree-sitter-sql`'s only unyanked release wants `tree-sitter ^0.19.3`
    // against this workspace's 0.26, and `tree-sitter-diff` is not published.
    for present in ["c", "cpp", "java", "ruby", "php", "bash"] {
        assert!(
            Grammar::detect(present).is_some(),
            "`{present}` was named as shipped and does not resolve"
        );
    }
    // `css` and `html` are in this list on purpose: their crates were added, built and
    // measured, and were removed again because §7.2's fourteen capture names do not include
    // the `@tag` / `@attribute` / `@property` their queries emit. Whoever adds them must
    // change the capture table too, and that recolours six shipped languages.
    for absent in ["sql", "diff", "css", "html"] {
        assert!(
            Grammar::detect(absent).is_none(),
            "`{absent}` now resolves; if that is intended, the `Grammar` doc comment and \
             the capture table need updating rather than this guard deleting"
        );
    }
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

#[test]
fn filetype_detection_reaches_a_grammar_for_every_extension_it_claims() {
    // A path is the second way a language is chosen — `Grammar::detect` falls through to
    // `detect_filetype` when a fence's word is not an alias. So an extension that maps to a
    // filetype name no `Grammar` answers to is a dead entry: the detection "succeeds" and
    // the block still renders plain. The knowingly-dead ones are named below.
    for (path, expected) in [
        ("main.c", "c"),
        ("panel.h", "c"),
        ("panel.cc", "cpp"),
        ("panel.hpp", "cpp"),
        ("Panel.java", "java"),
        ("index.php", "php"),
        ("draw.rb", "ruby"),
        ("setup.sh", "bash"),
        ("deploy.bash", "bash"),
    ] {
        assert_eq!(
            detect_filetype(path),
            Some(expected),
            "`{path}` no longer detects as {expected}"
        );
        assert!(
            Grammar::detect(path).is_some(),
            "`{path}` detects as a filetype but reaches no grammar, so it renders plain"
        );
    }
    // The knowingly-dead entries: recognised filetypes with no grammar behind them. Listed
    // so that adding one of these grammars later is noticed here rather than leaving a
    // detection rule nothing consumes.
    for (path, filetype) in [
        ("Dockerfile", "dockerfile"),
        ("Makefile", "make"),
        (".gitignore", "gitignore"),
        ("view.html.erb", "erb"),
        ("bundle.tar.gz", "gzip"),
    ] {
        assert_eq!(detect_filetype(path), Some(filetype));
        assert!(
            Grammar::detect(path).is_none(),
            "`{path}` now reaches a grammar; move it into the list above"
        );
    }
}

#[test]
#[ignore = "printer, not an assertion: run with --ignored --nocapture to eyeball coverage"]
fn highlight_new_grammar_coverage_probe() {
    let palette = palette();
    let plain = ratatui::style::Color::from(palette.text);
    for (label, source) in [
        ("bash", "# c\necho \"hi\" | wc -l"),
        ("c", "/* c */\nint main(void) { return 0; }"),
        ("cpp", "class Panel { public: int rows = 2; };"),
        ("java", "class P { void draw() { int x = 1; } }"),
        ("php", "<?php // c\nfunction draw($a) { return 1; } ?>"),
        ("ruby", "# c\ndef draw(a)\n  \"s\"\nend"),
    ] {
        let rows = spans(Some(label), source, &palette).expect("rows");
        let total: usize = rows.iter().map(Vec::len).sum();
        let coloured = rows
            .iter()
            .flatten()
            .filter(|span| span.style.fg != Some(plain))
            .count();
        println!("{label:6} spans={total:3} coloured={coloured:3}");
        for span in rows.iter().flatten() {
            if span.style.fg != Some(plain) {
                print!("[{}]", span.content);
            }
        }
        println!("\n");
    }
}

#[test]
fn highlight_cpp_covers_the_c_constructs_its_own_query_omits() {
    // `tree-sitter-cpp`'s highlights query does not restate C's, so a C++ block written in
    // C — which is most real C++ — matches nothing at all on its own. Measured: this snippet
    // yields **zero** coloured spans without `tree_sitter_c::HIGHLIGHT_QUERY` joined in, and
    // eleven with it. The `class`-based sample in `SAMPLES` cannot see this, because `class`
    // is one of the few things the C++ query does cover by itself.
    let palette = palette();
    let plain = ratatui::style::Color::from(palette.text);
    let source =
        "/* entry */\n#include <stdio.h>\nint main(void) {\n  int x = 1;\n  return sizeof(x);\n}";
    let rows = spans(Some("cpp"), source, &palette).expect("cpp produced no rows");
    let coloured = rows
        .iter()
        .flatten()
        .filter(|span| span.style.fg != Some(plain))
        .count();
    assert!(
        coloured >= 8,
        "a C-style C++ block coloured only {coloured} spans, so C's query is no longer \
         joined into the C++ configuration and most C++ renders plain"
    );
    for needle in ["int", "return", "/* entry */"] {
        assert!(
            rows.iter()
                .flatten()
                .any(|span| span.content == needle && span.style.fg != Some(plain)),
            "`{needle}` is plain in a C++ block"
        );
    }
}
