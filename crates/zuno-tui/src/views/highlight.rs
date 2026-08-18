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
}

pub(super) fn spans(hint: Option<&str>, source: &str, palette: &Palette) -> Option<Vec<Row>> {
    if source.len() > MAX_HIGHLIGHT_BYTES || source.split('\n').count() > MAX_HIGHLIGHT_LINES {
        return None;
    }

    let grammar = Grammar::detect(hint?)?;
    let mut configuration = configuration(grammar)?;
    configuration.configure(&HIGHLIGHT_NAMES);

    let long_lines = source
        .split('\n')
        .map(|line| line.len() > MAX_HIGHLIGHT_LINE_BYTES)
        .collect::<Vec<_>>();
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&configuration, source.as_bytes(), None, |_| None)
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
