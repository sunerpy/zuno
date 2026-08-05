//! The result and request shapes, mirrored from the oracle's schema.
//!
//! [`Entry`], [`Submatch`] and [`Match`] are field-for-field the oracle's
//! `FileSystem.Entry`, `FileSystem.Submatch` and `FileSystem.Match`
//! (`packages/schema/src/filesystem.ts:14-33`), because `opencode debug rg search`
//! serialises exactly those and the differential test compares against it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The per-match text length the transport caps at, in UTF-16 code units.
///
/// The oracle slices `match.lines.text` at 2000 in `ripgrep.ts:267`, and a JS
/// `String.length` counts UTF-16 code units, so the cap is applied in those units
/// rather than bytes or scalar values. Ports that use bytes disagree with the oracle
/// on the first non-ASCII line long enough to matter.
pub const MAX_MATCH_TEXT: usize = 2_000;

/// The number of submatches the transport keeps per line.
///
/// `MAX_SUBMATCHES` in `ripgrep.ts:20`.
pub const MAX_SUBMATCHES: usize = 100;

/// What a walked path is.
///
/// Both search entry points only ever yield files, matching `rg --files`; the
/// variant exists because the oracle's `Entry.type` is on the wire and the
/// differential compares the serialised shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

/// A path a search produced, relative to the search root.
///
/// The path is always forward-slashed and never carries a `./` prefix, which is the
/// normalisation the oracle applies to every `rg` line before it becomes a
/// `RelativePath` (`ripgrep.ts:171-175`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Entry {
    /// The path relative to the search root.
    pub path: String,
    /// What the path is.
    #[serde(rename = "type")]
    pub kind: EntryKind,
}

impl Entry {
    /// A file entry at `path`.
    #[must_use]
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::File,
        }
    }
}

/// One occurrence of the pattern inside a matched line.
///
/// `start` and `end` are byte offsets into [`Match::text`], which is how `rg --json`
/// reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submatch {
    /// The matched text.
    pub text: String,
    /// The byte offset of the match within the line.
    pub start: usize,
    /// The byte offset just past the match within the line.
    pub end: usize,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    /// The file the line is in.
    pub entry: Entry,
    /// The 1-based line number.
    pub line: u64,
    /// The byte offset of the start of the line within the file.
    pub offset: u64,
    /// The line's text, **including its terminator**, capped at [`MAX_MATCH_TEXT`].
    ///
    /// The terminator is present because `rg --json` includes it in `lines.text` and
    /// the oracle stores it verbatim. It is therefore visible in `grep`'s rendered
    /// output, which is why that output has a blank line between matches.
    pub text: String,
    /// Every occurrence on the line, capped at [`MAX_SUBMATCHES`].
    pub submatches: Vec<Submatch>,
}

/// A request to list paths matching a glob.
#[derive(Debug, Clone)]
pub struct GlobRequest {
    /// The directory to walk. Results are relative to it.
    pub cwd: PathBuf,
    /// The glob to match paths against.
    ///
    /// Passed to the engine as an **override whitelist**, exactly as the oracle
    /// passes it to `rg` as `--glob=<pattern>`. That is not merely a filter: an
    /// override match has higher precedence than any ignore file and than the
    /// hidden-file rule, so a pattern that names an ignored or hidden path returns
    /// it. See the crate docs.
    pub pattern: String,
    /// The most results to return.
    pub limit: usize,
    /// Whether to include hidden paths that the pattern did not explicitly whitelist.
    ///
    /// `false` is `rg`'s default and what the oracle's `glob` uses; `true` is
    /// `--hidden`.
    pub hidden: bool,
    /// Whether to follow symbolic links. `rg`'s default is `false`.
    pub follow: bool,
}

impl GlobRequest {
    /// A request with the oracle's `glob` tool defaults: visible files only, no
    /// symlink following.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, pattern: impl Into<String>, limit: usize) -> Self {
        Self {
            cwd: cwd.into(),
            pattern: pattern.into(),
            limit,
            hidden: false,
            follow: false,
        }
    }
}

/// A request to search file contents for a regex.
#[derive(Debug, Clone)]
pub struct GrepRequest {
    /// The directory to walk. Results are relative to it.
    pub cwd: PathBuf,
    /// The regex to search for.
    pub pattern: String,
    /// A single file to restrict the search to, relative to `cwd`.
    pub file: Option<String>,
    /// A glob restricting which files are searched.
    ///
    /// Like [`GlobRequest::pattern`] this becomes an override whitelist, so an
    /// `include` that names an ignored file searches it.
    pub include: Option<String>,
    /// The most matching lines to return.
    pub limit: usize,
}

impl GrepRequest {
    /// A request with the oracle's `grep` tool defaults.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, pattern: impl Into<String>, limit: usize) -> Self {
        Self {
            cwd: cwd.into(),
            pattern: pattern.into(),
            file: None,
            include: None,
            limit,
        }
    }

    /// Restricts the search to files matching `include`.
    #[must_use]
    pub fn with_include(mut self, include: Option<String>) -> Self {
        self.include = include;
        self
    }
}

/// Results plus whether the limit cut them short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResults<T> {
    /// The results, at most `limit` of them, in a deterministic order.
    pub items: Vec<T>,
    /// Whether more results existed beyond the limit.
    ///
    /// Distinct from `items.len() == limit`: this is `true` only when the engine
    /// actually saw a further result. The oracle's tools use the weaker
    /// `len() == limit` test and so claim truncation when a tree has exactly `limit`
    /// results; the tools in `oc-tools` reproduce that weaker test for output
    /// parity, and keep this field for callers that want the truth.
    pub truncated: bool,
}

impl<T> SearchResults<T> {
    /// An empty, untruncated result.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            truncated: false,
        }
    }

    /// How many results there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no results.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Strips the `./` prefixes and back-slashes a `rg`-style path into the oracle's
/// `RelativePath` form.
///
/// Reproduces `ripgrep.ts:171-175` literally: leading `./` or `.\` repeated any
/// number of times, then any leading separators, then every back-slash becomes a
/// forward slash.
#[must_use]
pub fn normalize_relative(raw: &str) -> String {
    let mut rest = raw;
    loop {
        if let Some(stripped) = rest.strip_prefix("./") {
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix(".\\") {
            rest = stripped;
        } else {
            break;
        }
    }
    let rest = rest.trim_start_matches(['/', '\\']);
    rest.replace('\\', "/")
}

/// Renders `path` relative to `root` in the oracle's `RelativePath` form.
///
/// When `path` is not under `root` the whole path is normalised instead, which is
/// more useful to a caller than an empty string; the engines never produce that case
/// because every entry they yield came from walking `root`.
#[must_use]
pub fn relative_to(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    normalize_relative(&relative.to_string_lossy())
}

/// Truncates `text` to `limit` UTF-16 code units, appending `...` when it cut.
///
/// The unit is UTF-16 because the oracle's cap is a JS `String.length` comparison;
/// see [`MAX_MATCH_TEXT`]. The cut is moved back to a scalar boundary so the result
/// is always valid UTF-8, which is the one place this cannot be byte-identical to a
/// JS slice: JS may split a surrogate pair, and Rust has no way to represent that.
#[must_use]
pub fn truncate_utf16(text: &str, limit: usize) -> String {
    let mut units = 0usize;
    for (index, ch) in text.char_indices() {
        if units >= limit {
            let mut out = String::with_capacity(index + 3);
            out.push_str(&text[..index]);
            out.push_str("...");
            return out;
        }
        units += ch.len_utf16();
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_lose_every_dot_slash_prefix_and_gain_forward_slashes() {
        assert_eq!(normalize_relative("./src/a.ts"), "src/a.ts");
        assert_eq!(normalize_relative("././src/a.ts"), "src/a.ts");
        assert_eq!(normalize_relative(".\\src\\a.ts"), "src/a.ts");
        assert_eq!(normalize_relative("/src/a.ts"), "src/a.ts");
        assert_eq!(normalize_relative("src/a.ts"), "src/a.ts");
    }

    #[test]
    fn a_dot_prefixed_file_name_is_not_mistaken_for_a_dot_slash_prefix() {
        assert_eq!(normalize_relative(".gitignore"), ".gitignore");
        assert_eq!(normalize_relative("./.gitignore"), ".gitignore");
    }

    #[test]
    fn truncation_counts_utf16_units_and_marks_the_cut() {
        let short = "abc";
        assert_eq!(truncate_utf16(short, 2_000), "abc");

        let long = "a".repeat(2_500);
        let cut = truncate_utf16(&long, 2_000);
        assert_eq!(cut.len(), 2_003);
        assert!(cut.ends_with("..."));
    }

    #[test]
    fn an_astral_character_counts_as_two_units_exactly_as_in_js() {
        // "𝄞" is one scalar value, two UTF-16 code units. A byte-counting or
        // char-counting port would place the cut somewhere else.
        let text = "𝄞".repeat(3);
        assert_eq!(truncate_utf16(&text, 4), "𝄞𝄞...");
        assert_eq!(truncate_utf16(&text, 6), text);
    }

    #[test]
    fn a_match_serialises_into_the_oracles_field_names() {
        let value = serde_json::to_value(Match {
            entry: Entry::file("src/a.ts"),
            line: 3,
            offset: 12,
            text: "needle\n".to_owned(),
            submatches: vec![Submatch {
                text: "needle".to_owned(),
                start: 0,
                end: 6,
            }],
        })
        .expect("a match serialises");

        assert_eq!(value["entry"]["path"], "src/a.ts");
        assert_eq!(value["entry"]["type"], "file");
        assert_eq!(value["line"], 3);
        assert_eq!(value["offset"], 12);
        assert_eq!(value["text"], "needle\n");
        assert_eq!(value["submatches"][0]["end"], 6);
    }

    #[test]
    fn relative_to_falls_back_to_the_whole_path_when_it_escapes_the_root() {
        assert_eq!(
            relative_to(Path::new("/a/b"), Path::new("/a/b/c/d.ts")),
            "c/d.ts"
        );
        assert_eq!(
            relative_to(Path::new("/a/b"), Path::new("/x/y.ts")),
            "x/y.ts"
        );
    }
}
