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
/// Never carries a `./` prefix or a leading separator, and is forward-slashed on the
/// platform where `\` is a separator, which is the normalisation the oracle applies to
/// every `rg` line before it becomes a `RelativePath` (`ripgrep.ts:171-175`). The path is
/// an *identifier* the model feeds straight back into `read` or `edit`, so on Unix — where
/// `\` is an ordinary filename byte — it is the name the file actually has, back-slashes
/// and all. See [`normalize_relative`].
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
    /// Whether the engine knows of a result that `items` does not carry.
    ///
    /// Deliberately *not* "the limit cut the list": it is `true` whenever the engine
    /// saw a further result, whether the limit cut it off or the engine could not
    /// decode the record that carried it. Both readings answer the only question the
    /// model actually asks of this field — "is this all there is?" — and the second is
    /// why `truncated` can be `true` with fewer than `limit` items.
    ///
    /// Distinct from `items.len() == limit`, which claims truncation for a tree that
    /// has exactly `limit` results and misses a dropped record entirely. The oracle's
    /// tools use that weaker test and the tools in `zuno-tools` reproduce it for output
    /// parity; this field is for callers that want the truth.
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

/// Strips a `rg`-style path down to the oracle's `RelativePath` form.
///
/// Reproduces `ripgrep.ts:171-175`, but only with the reductions that cannot rename a
/// file on the platform they run on. Leading `./` repeated any number of times, and any
/// leading `/`, go unconditionally: neither `.` as a whole component nor `/` can occur
/// inside a filename anywhere Zuno runs, so those bytes are always separators `rg` put
/// there. The oracle's remaining step — trimming a leading `\` and rewriting every `\`
/// to `/` — is applied only where `\` *is* a separator, which is Windows.
///
/// The split exists because a `RelativePath` is an *identifier* the model feeds straight
/// back into `read` or `edit`, and on Unix `\` is an ordinary filename byte. Applied
/// there, the rewrite did not merely render a name oddly, it named a **different real
/// file**: a tree holding a nested `a/b.ts` and a flat `a\b.ts` reported `a/b.ts` twice,
/// `back\slash.ts` became a `back/slash.ts` that is not on disk, and `\lead.ts` became
/// `lead.ts`. That is the same class as a lossy U+FFFD name — see [`relative_to`] — in
/// its dangerous direction: the U+FFFD name opens nothing, while an alias opens the
/// *wrong* file, so `grep` reported a hit against a file that does not contain the
/// pattern and a write-capable `edit` keyed on that path would modify a file that never
/// matched. A reduction that can make a path name a file it is not belongs on the deny
/// side only, so this one is kept where `\` cannot be part of a name.
#[must_use]
pub fn normalize_relative(raw: &str) -> String {
    // `cfg!` rather than `#[cfg]` so the Windows arm is compiled, linted and unit-tested
    // on every host; `normalize_relative_with(raw, true)` is how the tests pin it.
    normalize_relative_with(raw, cfg!(windows))
}

/// [`normalize_relative`] with the platform question answered explicitly.
///
/// `windows_separators` says whether `\` in `raw` is a path separator rather than a byte
/// of a filename. It is `cfg!(windows)` in production, because every path either engine
/// normalises came from `rg` walking the local filesystem, and it is both values in the
/// tests so the Windows answer is pinned from any host.
fn normalize_relative_with(raw: &str, windows_separators: bool) -> String {
    let mut rest = raw;
    loop {
        if let Some(stripped) = rest.strip_prefix("./") {
            rest = stripped;
            continue;
        }
        // `.\dotdot..ts` is one legal flat filename on Unix, so this is only a prefix
        // where `\` separates.
        if windows_separators && let Some(stripped) = rest.strip_prefix(".\\") {
            rest = stripped;
            continue;
        }
        break;
    }
    if windows_separators {
        rest.trim_start_matches(['/', '\\']).replace('\\', "/")
    } else {
        rest.trim_start_matches('/').to_owned()
    }
}

/// Renders `path` relative to `root` in the oracle's `RelativePath` form, or `None`
/// when it has no such form.
///
/// `None` is the fail-closed answer for a path that is not valid UTF-8, which is
/// reachable on every supported platform: a latin-1 name from an old archive on
/// Linux or macOS, a lone surrogate in an NTFS name on Windows. A `RelativePath` is an
/// *identifier* the model feeds straight back into `read` or `edit`, so rendering such
/// a path lossily would hand back a name containing U+FFFD that matches no file — the
/// caller has to be told there is no answer rather than given a wrong one. This is the
/// same rule [`crate::ripgrep::Ripgrep::glob`] and `grep` apply to the paths `rg`
/// reports.
///
/// When `path` is not under `root` the whole path is normalised instead, which is
/// more useful to a caller than an empty string; the engines never produce that case
/// because every entry they yield came from walking `root`.
#[must_use]
pub fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    Some(normalize_relative(relative.to_str()?))
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
    fn relative_paths_lose_every_dot_slash_prefix_and_leading_separator() {
        assert_eq!(normalize_relative("./src/a.ts"), "src/a.ts");
        assert_eq!(normalize_relative("././src/a.ts"), "src/a.ts");
        assert_eq!(normalize_relative("/src/a.ts"), "src/a.ts");
        assert_eq!(normalize_relative("src/a.ts"), "src/a.ts");
    }

    #[test]
    fn a_windows_path_gains_forward_slashes_and_the_oracles_whole_rewrite() {
        // The oracle's rewrite in full, pinned from any host. `\` cannot occur in a
        // Windows filename, so on that platform it can only remove separators `rg` put
        // there: it can never make a path name a file other than the one it names.
        assert_eq!(normalize_relative_with(".\\src\\a.ts", true), "src/a.ts");
        assert_eq!(normalize_relative_with(".\\.\\src\\a.ts", true), "src/a.ts");
        assert_eq!(normalize_relative_with("\\src\\a.ts", true), "src/a.ts");
        assert_eq!(normalize_relative_with("src\\a.ts", true), "src/a.ts");
    }

    #[test]
    fn a_back_slash_that_is_a_filename_byte_survives_the_normalisation() {
        // The same three inputs where `\` is part of a name, which is every Unix
        // filesystem. Rewriting `a\b.ts` to `a/b.ts` aliased one real file onto a
        // *different* real file, `back\slash.ts` and `\lead.ts` became names that are
        // not on disk at all, and `.\dotdot..ts` lost a prefix that was its own first
        // two characters.
        assert_eq!(normalize_relative_with("a\\b.ts", false), "a\\b.ts");
        assert_eq!(
            normalize_relative_with("back\\slash.ts", false),
            "back\\slash.ts"
        );
        assert_eq!(normalize_relative_with("\\lead.ts", false), "\\lead.ts");
        assert_eq!(
            normalize_relative_with(".\\dotdot..ts", false),
            ".\\dotdot..ts"
        );
        // And the prefix `rg` really does add is still removed, back-slash or not.
        assert_eq!(normalize_relative_with("./a\\b.ts", false), "a\\b.ts");
        assert_eq!(normalize_relative_with("/a\\b.ts", false), "a\\b.ts");
    }

    #[cfg(unix)]
    #[test]
    fn the_platform_arm_production_takes_is_the_one_this_platform_needs() {
        // `normalize_relative` is what both engines call, so the split is only worth
        // anything if the public entry point picks the arm that matches the filesystem
        // underneath it.
        assert_eq!(normalize_relative("a\\b.ts"), "a\\b.ts");
        assert_eq!(normalize_relative("./a\\b.ts"), "a\\b.ts");
    }

    #[cfg(windows)]
    #[test]
    fn the_platform_arm_production_takes_is_the_one_this_platform_needs() {
        assert_eq!(normalize_relative("a\\b.ts"), "a/b.ts");
        assert_eq!(normalize_relative(".\\a\\b.ts"), "a/b.ts");
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
            relative_to(Path::new("/a/b"), Path::new("/a/b/c/d.ts")).as_deref(),
            Some("c/d.ts")
        );
        assert_eq!(
            relative_to(Path::new("/a/b"), Path::new("/x/y.ts")).as_deref(),
            Some("x/y.ts")
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_to_names_a_back_slashed_file_as_itself() {
        // The other public route to a `RelativePath`, on the same reduction: this
        // answered `a/b.ts` for a flat file called `a\b.ts`, so a caller resolving it
        // against the root opened a different file, or none.
        assert_eq!(
            relative_to(Path::new("/a/b"), Path::new("/a/b/c\\d.ts")).as_deref(),
            Some("c\\d.ts")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_has_no_relative_path_form_rather_than_a_lossy_one() {
        // The same byte sequence the search engines reject, reached through the other
        // public route to a `RelativePath`. `to_string_lossy` answered `bad\u{fffd}.ts`,
        // an identifier naming no file; there is no reader of this helper in the
        // workspace today, so the point is that the next one cannot be handed one.
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("/a/b");
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/a/b/bad\xff.ts"));

        assert_eq!(relative_to(root, odd), None);
    }
}
