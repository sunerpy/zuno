//! Bounded tree-sitter highlighting for fenced markdown code.
//!
//! This module stays beside the other view files so the palette-discipline scan sees
//! every colour choice. The renderer owns no cross-call state: a future cache must be
//! injected above the pure markdown render boundary rather than hidden here.
//! Colours originate in the owning view's `ViewContext` and arrive here only as its
//! borrowed [`Palette`]; this helper never resolves a theme or invents a colour itself.

use crate::theme::Palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::sync::OnceLock;
use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

type Row = Vec<Span<'static>>;

/// A block larger than 512 KiB falls back before tree-sitter sees it.
pub(super) const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
/// Ten thousand source lines is the most one render may parse for highlighting.
pub(super) const MAX_HIGHLIGHT_LINES: usize = 10_000;
/// A line beyond 4 KiB remains visible but is emitted with the plain code style.
pub(super) const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

const HIGHLIGHT_NAMES: [&str; 14] = [
    "comment",
    "string",
    "number",
    "boolean",
    "constant",
    "keyword",
    "function",
    "method",
    "constructor",
    "type",
    "class",
    "module",
    "operator",
    "punctuation",
];

/// A language this build can highlight.
///
/// `§7.2`'s second batch is `c`, `cpp`, `java`, `ruby`, `php`, `html`, `css`, `sql` and
/// `diff`. Six of those are here, plus `bash`, which the workspace already carried for
/// `zuno-tools`. Four are absent, each for a checkable reason rather than a preference:
///
/// * `tree-sitter-sql`'s only unyanked release is `0.0.2`, which requires
///   `tree-sitter ^0.19.3`. This workspace resolves `tree-sitter 0.26.11`, and a `Language`
///   from 0.19 is not the type `tree_sitter_highlight 0.26` accepts, so it could not be
///   passed to [`make_configuration`] even if a second copy were allowed into the graph.
/// * `tree-sitter-diff` is not published under that name at all.
/// * `css` and `html` were built, measured and then **left out**. Their queries capture
///   `@tag`, `@attribute` and `@property`, none of which are in [`HIGHLIGHT_NAMES`], so the
///   words a reader looks at stay unstyled: measured on `.panel { color: red; }` CSS
///   coloured three spans (the comment, a number and a unit) and HTML two (a comment and an
///   attribute *value*) — every tag name plain. Adding those three capture names is the fix,
///   and it is deliberately not done here: `@property` is also emitted by Go, Python, Rust,
///   TOML, YAML and JavaScript, so it would recolour six already-shipped languages that
///   `P2-2` verified by eye. That is a `§7.2` table change, not a grammar addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grammar {
    Rust,
    Python,
    Go,
    TypeScript,
    JavaScript,
    Tsx,
    Json,
    Yaml,
    Toml,
    Markdown,
    Bash,
    C,
    Cpp,
    Java,
    Php,
    Ruby,
}

/// Each grammar's position in [`ALL_GRAMMARS`], and its slot in [`CONFIGURATIONS`].
///
/// The match is exhaustive, so a new variant cannot compile without being given a number
/// here, and `highlight_every_grammar_in_the_table_produces_spans_for_its_own_alias`
/// rejects a number outside the array's range. Together that is what stops a grammar being
/// added and never tested — the failure a hand-kept list produces silently, as the diff
/// scope's comment did when it named nine of eleven bare characters.
///
/// It is production code rather than test-only because [`configured`] indexes the
/// configuration cache with it. The same exhaustiveness that made it a good test helper is
/// what makes it a safe index: a new grammar cannot reach the cache without a slot.
const fn grammar_ordinal(grammar: Grammar) -> usize {
    match grammar {
        Grammar::Rust => 0,
        Grammar::Python => 1,
        Grammar::Go => 2,
        Grammar::TypeScript => 3,
        Grammar::JavaScript => 4,
        Grammar::Tsx => 5,
        Grammar::Json => 6,
        Grammar::Yaml => 7,
        Grammar::Toml => 8,
        Grammar::Markdown => 9,
        Grammar::Bash => 10,
        Grammar::C => 11,
        Grammar::Cpp => 12,
        Grammar::Java => 13,
        Grammar::Php => 14,
        Grammar::Ruby => 15,
    }
}

/// Every grammar, for the guards that must not miss one.
#[cfg(test)]
const ALL_GRAMMARS: [Grammar; GRAMMAR_COUNT] = [
    Grammar::Rust,
    Grammar::Python,
    Grammar::Go,
    Grammar::TypeScript,
    Grammar::JavaScript,
    Grammar::Tsx,
    Grammar::Json,
    Grammar::Yaml,
    Grammar::Toml,
    Grammar::Markdown,
    Grammar::Bash,
    Grammar::C,
    Grammar::Cpp,
    Grammar::Java,
    Grammar::Php,
    Grammar::Ruby,
];

/// How many grammars this build can highlight, and so how large the cache is.
///
/// Kept beside [`grammar_ordinal`] because the two are one fact: the ordinals are
/// `0..GRAMMAR_COUNT` and a variant without a slot cannot compile.
const GRAMMAR_COUNT: usize = 16;

/// One built-and-configured [`HighlightConfiguration`] per grammar.
///
/// # What this fixes, measured
///
/// [`configuration`] compiles a tree-sitter query from its grammar's `HIGHLIGHTS_QUERY`
/// source text, and until this cache existed it ran **once per code fence, per frame**.
/// The transcript re-renders every message on every frame, so a session with 465
/// assistant replies re-compiled 465 queries per frame. Measured on this project at 100
/// columns, five runs (`crates/zuno-tui/tests/render_cost.rs`, and
/// `docs/perf-methodology.md`): rendering prose alone cost a median 11.544 µs, one Rust
/// fence took that to 17.237 ms, and two fences to 34.571 ms — exactly twice, so there
/// was no amortisation of any kind. A JSON fence added only 20.236 µs, which is what
/// identifies the cost as query compilation rather than parsing: Rust's query is large
/// and JSON's is tiny, and the ~850x gap tracks the query, not the source.
///
/// # Why this cache needs no invalidation key
///
/// A configuration is a pure function of the grammar. Its inputs are a `Language` and a
/// `&'static str` query, both fixed at compile time, and [`HIGHLIGHT_NAMES`] is a
/// constant. It does **not** depend on width, and it does **not** depend on the palette:
/// colour is applied afterwards by [`capture_style`] from the events, which is why a live
/// theme change cannot stale this entry. So the key is the grammar alone, and there is no
/// condition under which a cached entry becomes wrong. That is the whole reason this, and
/// not the transcript, is the right place to start caching.
///
/// # The bound
///
/// Exactly [`GRAMMAR_COUNT`] slots, allocated as part of the static and never grown: the
/// cache is keyed on a `Copy` enum, so it cannot grow with content, session length, or
/// uptime. At its bound it holds all 16 configurations — measured below at 5,865,048
/// bytes of RSS for the full set, against M1's 1,198,872 KiB W-real median, or 0.478%.
/// This is deliberately *not* a content-keyed cache: the perf plan
/// §10.2's rule against unbounded growth in a long-running interactive process is the class
/// of defect this plan exists to fix, and a per-source cache would reintroduce it.
///
/// A failed build is cached as `None` rather than retried. §6.5 records the reference
/// implementation's own finding here: an uncached failure re-runs the expensive failing
/// path on every redraw, which is the worst of both outcomes.
static CONFIGURATIONS: [OnceLock<Option<HighlightConfiguration>>; GRAMMAR_COUNT] =
    [const { OnceLock::new() }; GRAMMAR_COUNT];

/// `grammar`'s configured highlighter, built at most once per process.
fn configured(grammar: Grammar) -> Option<&'static HighlightConfiguration> {
    CONFIGURATIONS[grammar_ordinal(grammar)]
        .get_or_init(|| {
            let mut configuration = configuration(grammar)?;
            configuration.configure(&HIGHLIGHT_NAMES);
            Some(configuration)
        })
        .as_ref()
}

pub(super) fn spans(hint: Option<&str>, source: &str, palette: &Palette) -> Option<Vec<Row>> {
    if source.len() > MAX_HIGHLIGHT_BYTES || source.split('\n').count() > MAX_HIGHLIGHT_LINES {
        return None;
    }

    let grammar = Grammar::detect(hint?)?;
    let configuration = configured(grammar)?;

    let long_lines = source
        .split('\n')
        .map(|line| line.len() > MAX_HIGHLIGHT_LINE_BYTES)
        .collect::<Vec<_>>();
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(configuration, source.as_bytes(), None, |_| None)
        .ok()?;
    let mut rows = vec![Vec::new()];
    let mut styles = Vec::new();
    let mut line = 0;
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => {
                styles.push(capture_style(HIGHLIGHT_NAMES.get(highlight.0)?, palette));
            }
            HighlightEvent::HighlightEnd => {
                styles.pop()?;
            }
            HighlightEvent::Source { start, end } => {
                let text = source.get(start..end)?;
                let style = styles
                    .last()
                    .copied()
                    .unwrap_or_else(|| capture_style("text", palette));
                append_source(
                    &mut rows,
                    text,
                    style,
                    plain_code_style(palette),
                    &long_lines,
                    &mut line,
                );
            }
        }
    }
    Some(rows)
}

pub(super) fn detect_filetype(path: &str) -> Option<&'static str> {
    let basename = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    match basename.as_str() {
        "dockerfile" => return Some("dockerfile"),
        name if name.starts_with("dockerfile.") => return Some("dockerfile"),
        "makefile" | "gnumakefile" => return Some("make"),
        "cargo.lock" => return Some("toml"),
        ".gitignore" | ".gitattributes" | ".gitmodules" => return Some("gitignore"),
        _ => {}
    }

    for (suffix, filetype) in [
        (".html.erb", "erb"),
        (".d.ts", "typescript"),
        (".tar.gz", "gzip"),
    ] {
        if basename.ends_with(suffix) {
            return Some(filetype);
        }
    }

    let extension = basename.rsplit_once('.')?.1;
    match extension {
        "rs" => Some("rust"),
        "py" | "pyw" => Some("python"),
        "go" => Some("go"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" => Some("javascript"),
        "jsx" => Some("javascript"),
        "json" | "jsonc" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "md" | "markdown" | "mdx" => Some("markdown"),
        "sh" | "bash" | "zsh" | "bashrc" | "zshrc" => Some("bash"),
        // `.h` to C, not C++. It is ambiguous in reality, and C is the safer read: a C++
        // header parsed as C loses some highlighting, whereas C parsed as C++ can report a
        // syntax error and lose the block instead.
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" => Some("cpp"),
        "java" => Some("java"),
        "php" => Some("php"),
        "rb" => Some("ruby"),
        _ => None,
    }
}

fn capture_style(capture: &str, palette: &Palette) -> Style {
    let base = Style::new()
        .fg(palette.text.into())
        .bg(palette.background_panel.into());
    if capture == "comment" || capture.starts_with("comment.") {
        return base
            .fg(palette.syntax_comment.into())
            .add_modifier(Modifier::ITALIC);
    }
    if capture == "string" || capture.starts_with("string.") {
        return base.fg(palette.syntax_string.into());
    }
    if matches!(capture, "number" | "boolean")
        || capture.starts_with("number.")
        || capture.starts_with("boolean.")
        || capture == "constant"
        || capture.starts_with("constant.")
    {
        return base.fg(palette.syntax_number.into());
    }
    if capture == "keyword" || capture.starts_with("keyword.") {
        return base
            .fg(palette.syntax_keyword.into())
            .add_modifier(Modifier::ITALIC);
    }
    if capture == "function"
        || capture.starts_with("function.")
        || capture == "method"
        || capture.starts_with("method.")
        || capture == "constructor"
    {
        return base.fg(palette.syntax_function.into());
    }
    if capture == "type" || capture.starts_with("type.") || matches!(capture, "class" | "module") {
        return base.fg(palette.syntax_type.into());
    }
    if capture == "operator" || capture.starts_with("operator.") {
        return base.fg(palette.syntax_operator.into());
    }
    if capture == "punctuation" || capture.starts_with("punctuation.") {
        return base.fg(palette.syntax_punctuation.into());
    }
    base
}

impl Grammar {
    fn detect(hint: &str) -> Option<Self> {
        let hint = hint.split_whitespace().next().unwrap_or(hint);
        let normalized = hint.to_ascii_lowercase();
        Self::named(&normalized).or_else(|| detect_filetype(&normalized).and_then(Self::named))
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "rust" | "rs" => Some(Self::Rust),
            "python" | "py" | "py3" => Some(Self::Python),
            "go" | "golang" => Some(Self::Go),
            "typescript" | "ts" => Some(Self::TypeScript),
            "javascript" | "js" | "jsx" => Some(Self::JavaScript),
            "tsx" => Some(Self::Tsx),
            "json" | "jsonc" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            "markdown" | "md" | "mdx" => Some(Self::Markdown),
            "bash" | "sh" | "shell" | "zsh" => Some(Self::Bash),
            "c" | "h" => Some(Self::C),
            // `c++` as well as `cpp`: a fence commonly says `c++`, and rejecting it would
            // read as a broken highlighter rather than an unknown alias.
            "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" => Some(Self::Cpp),
            "java" => Some(Self::Java),
            "php" => Some(Self::Php),
            "ruby" | "rb" => Some(Self::Ruby),
            _ => None,
        }
    }
}

fn configuration(grammar: Grammar) -> Option<HighlightConfiguration> {
    let (language, name, highlights) = match grammar {
        Grammar::Rust => (
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::Python => (
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::Go => (
            tree_sitter_go::LANGUAGE.into(),
            "go",
            tree_sitter_go::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::TypeScript => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            join_queries(&[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ]),
        ),
        Grammar::JavaScript => (
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY.to_owned(),
        ),
        Grammar::Tsx => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            join_queries(&[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ]),
        ),
        Grammar::Json => (
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::Yaml => (
            tree_sitter_yaml::LANGUAGE.into(),
            "yaml",
            tree_sitter_yaml::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::Toml => (
            tree_sitter_toml_ng::LANGUAGE.into(),
            "toml",
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::Markdown => (
            tree_sitter_md::INLINE_LANGUAGE.into(),
            "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE.to_owned(),
        ),
        // The constant is `HIGHLIGHT_QUERY` in bash, c and cpp and `HIGHLIGHTS_QUERY` in
        // the rest. Upstream's own naming; spelling it per arm is the only way to be right.
        Grammar::Bash => (
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            tree_sitter_bash::HIGHLIGHT_QUERY.to_owned(),
        ),
        Grammar::C => (
            tree_sitter_c::LANGUAGE.into(),
            "c",
            tree_sitter_c::HIGHLIGHT_QUERY.to_owned(),
        ),
        Grammar::Cpp => (
            tree_sitter_cpp::LANGUAGE.into(),
            "cpp",
            // C++ is a superset, and its own query does not restate C's rules; without C's
            // the shared constructs in a C++ block go unhighlighted.
            join_queries(&[
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            ]),
        ),
        Grammar::Java => (
            tree_sitter_java::LANGUAGE.into(),
            "java",
            tree_sitter_java::HIGHLIGHTS_QUERY.to_owned(),
        ),
        // `LANGUAGE_PHP`, not `LANGUAGE_PHP_ONLY`: the former starts outside PHP and enters
        // on `<?php`, which is how a fenced PHP block is actually written.
        Grammar::Php => (
            tree_sitter_php::LANGUAGE_PHP.into(),
            "php",
            tree_sitter_php::HIGHLIGHTS_QUERY.to_owned(),
        ),
        Grammar::Ruby => (
            tree_sitter_ruby::LANGUAGE.into(),
            "ruby",
            tree_sitter_ruby::HIGHLIGHTS_QUERY.to_owned(),
        ),
    };
    make_configuration(language, name, &highlights)
}

fn make_configuration(
    language: Language,
    name: &str,
    highlights: &str,
) -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(language, name, highlights, "", "").ok()
}

fn join_queries(queries: &[&str]) -> String {
    queries.join("\n")
}

fn append_source(
    rows: &mut Vec<Row>,
    source: &str,
    highlighted_style: Style,
    plain_style: Style,
    long_lines: &[bool],
    line: &mut usize,
) {
    for part in source.split_inclusive('\n') {
        let (text, ends_line) = match part.strip_suffix('\n') {
            Some(text) => (text, true),
            None => (part, false),
        };
        let style = if long_lines.get(*line).copied().unwrap_or(false) {
            plain_style
        } else {
            highlighted_style
        };
        if !text.is_empty() {
            push_span(
                rows.last_mut().expect("the first row always exists"),
                text,
                style,
            );
        }
        if ends_line {
            rows.push(Vec::new());
            *line += 1;
        }
    }
}

fn push_span(row: &mut Row, text: &str, style: Style) {
    if let Some(last) = row.last_mut().filter(|span| span.style == style) {
        last.content.to_mut().push_str(text);
    } else {
        row.push(Span::styled(text.to_owned(), style));
    }
}

fn plain_code_style(palette: &Palette) -> Style {
    Style::new()
        .fg(palette.markdown_code_block.into())
        .bg(palette.background_panel.into())
}

#[cfg(test)]
#[path = "highlight_tests.rs"]
mod tests;
