//! The embedded engine: one walk, no subprocess, no downloaded binary.
//!
//! # Why this reproduces `rg` rather than approximating it
//!
//! The oracle shells out to a ripgrep binary it downloads at runtime
//! (`packages/core/src/ripgrep/binary.ts:88-121`) and passes a fixed flag set. Every
//! one of those flags has an exact counterpart here, because `ignore` and
//! `grep-searcher` are the crates ripgrep itself is built from:
//!
//! | oracle flag | here |
//! |---|---|
//! | `--files` | [`ignore::Walk`] yielding files only |
//! | `--glob=<pattern>` | [`OverrideBuilder::add`] — a **whitelist**, see below |
//! | `--glob=!**/.git/**` | the same builder, added last so it wins |
//! | `--hidden` (grep only) | [`WalkBuilder::hidden(false)`] |
//! | `--follow` | [`WalkBuilder::follow_links`] |
//! | `--json` | [`Match`] built directly, no serialisation round trip |
//! | `--no-messages` | per-entry and per-file errors are skipped |
//! | `--no-config` | nothing reads `RIPGREP_CONFIG_PATH` |
//!
//! # The one semantic that surprises everyone
//!
//! `--glob` is **not** a post-filter. In ripgrep — and therefore in
//! [`ignore::overrides`] — an override match has *higher precedence than every
//! ignore file and than the hidden-file rule* (`ignore-0.4.33/src/dir.rs:511-522`,
//! and `walk.rs:481`: "if the path hasn't been whitelisted and it is hidden, then
//! the path is skipped"). So the oracle's `glob` tool, which always passes
//! `--glob=<pattern>`, **returns gitignored and hidden files** when the pattern
//! names them. Verified against the real binary:
//!
//! ```text
//! $ opencode debug rg files --glob '**/*.ts'
//! .hidden_file.ts       <- hidden, returned: the glob whitelisted it
//! ignored.ts            <- listed in .gitignore, returned: same reason
//! src/a.ts
//! ```
//!
//! while `.hidden_dir/e.ts` and `node_modules/pkg/f.ts` are absent from that same
//! run: their *parent directories* were not whitelisted by `**/*.ts`, so the walk
//! pruned them before reaching the children. That asymmetry between a file-level and
//! a directory-level exclusion is load-bearing, and it falls out of using the real
//! walker instead of writing a filter.
//!
//! # Ordering
//!
//! The walk is single-threaded and sorted ([`WalkBuilder::sort_by_file_path`] with
//! [`Path`]'s own `Ord`), which is exactly what `rg --sort=path` does. The oracle
//! passes no `--sort`, so **its** order is whatever its parallel walk produces and
//! differs between two runs of the same command over the same tree; see the crate
//! docs for the measurement. Sorting is therefore not a change to an order anything
//! could have depended on, and it buys three things the oracle does not have: a
//! stable result under truncation, correct grouping in `grep`'s output (all matches
//! for a file are adjacent, so a path can never head two separate groups), and a
//! diffable transcript.

use crate::cancel::{CANCEL_POLL_INTERVAL, Cancellation};
use crate::error::SearchError;
use crate::types::{
    Entry, GlobRequest, GrepRequest, MAX_MATCH_TEXT, MAX_SUBMATCHES, Match, SearchResults,
    Submatch, relative_to, truncate_utf16,
};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{Walk, WalkBuilder};
use std::path::Path;

/// The exclusion the oracle appends to every invocation.
///
/// `packages/core/src/ripgrep.ts:166`, `:198`, `:227`. Added last so that in the
/// gitignore "last match wins" ordering it beats any include pattern: a caller
/// asking for `**/*` still does not get the object database.
pub const GIT_EXCLUDE_GLOB: &str = "!**/.git/**";

/// The search engine that needs no external binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmbeddedEngine;

impl EmbeddedEngine {
    /// Lists files matching a glob.
    ///
    /// # Errors
    ///
    /// [`SearchError::RootMissing`] or [`SearchError::RootNotDirectory`] when `cwd`
    /// is not a directory, [`SearchError::InvalidGlob`] when the pattern will not
    /// compile, [`SearchError::Cancelled`] when the signal fires mid-walk.
    pub fn glob(
        &self,
        request: &GlobRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Entry>, SearchError> {
        let root = check_root(&request.cwd)?;
        let overrides = build_overrides(root, &[request.pattern.as_str(), GIT_EXCLUDE_GLOB])?;
        let walk = walker(root, overrides, !request.hidden, request.follow);

        let mut items = Vec::new();
        let mut truncated = false;
        for (index, result) in walk.enumerate() {
            poll(index, cancel)?;
            let Some(entry) = accept_file(result) else {
                continue;
            };
            if items.len() == request.limit {
                truncated = true;
                break;
            }
            items.push(Entry::file(relative_to(root, entry.path())));
        }

        Ok(SearchResults { items, truncated })
    }

    /// Searches file contents for a regex.
    ///
    /// # Errors
    ///
    /// [`SearchError::RootMissing`] or [`SearchError::RootNotDirectory`] when `cwd`
    /// is not a directory, [`SearchError::InvalidPattern`] when the regex will not
    /// compile, [`SearchError::InvalidGlob`] when `include` will not compile,
    /// [`SearchError::Cancelled`] when the signal fires mid-search.
    pub fn grep(
        &self,
        request: &GrepRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Match>, SearchError> {
        let root = check_root(&request.cwd)?;
        let matcher = build_matcher(&request.pattern)?;
        let mut searcher = build_searcher();

        let mut items = Vec::new();
        let mut truncated = false;

        if let Some(file) = &request.file {
            // An explicit path bypasses ignore rules entirely, exactly as it does
            // for `rg <pattern> <path>`. Unused by the `grep` tool, which always
            // passes a directory; kept because the oracle's `GrepInput` has it.
            let path = root.join(file);
            let entry = Entry::file(relative_to(root, &path));
            let outcome = search_one(
                &mut searcher,
                &matcher,
                &path,
                entry,
                request.limit,
                cancel,
                &mut items,
            );
            if outcome.cancelled {
                return Err(SearchError::Cancelled);
            }
            truncated = items.len() > request.limit;
            items.truncate(request.limit);
            return Ok(SearchResults { items, truncated });
        }

        let mut globs = Vec::new();
        if let Some(include) = &request.include {
            globs.push(include.as_str());
        }
        globs.push(GIT_EXCLUDE_GLOB);
        let overrides = build_overrides(root, &globs)?;
        // The oracle passes `--hidden` unconditionally for grep (`ripgrep.ts:224`).
        let walk = walker(root, overrides, false, false);

        for (index, result) in walk.enumerate() {
            poll(index, cancel)?;
            let Some(entry) = accept_file(result) else {
                continue;
            };
            let relative = Entry::file(relative_to(root, entry.path()));
            let outcome = search_one(
                &mut searcher,
                &matcher,
                entry.path(),
                relative,
                request.limit,
                cancel,
                &mut items,
            );
            if outcome.cancelled {
                return Err(SearchError::Cancelled);
            }
            if items.len() > request.limit {
                truncated = true;
                break;
            }
        }

        items.truncate(request.limit);
        Ok(SearchResults { items, truncated })
    }
}

fn poll(index: usize, cancel: &dyn Cancellation) -> Result<(), SearchError> {
    if index.is_multiple_of(CANCEL_POLL_INTERVAL) && cancel.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    Ok(())
}

fn check_root(root: &Path) -> Result<&Path, SearchError> {
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => Ok(root),
        Ok(_) => Err(SearchError::RootNotDirectory {
            root: root.to_path_buf(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(SearchError::RootMissing {
                root: root.to_path_buf(),
            })
        }
        Err(source) => Err(SearchError::Read {
            path: root.to_path_buf(),
            source,
        }),
    }
}

fn build_overrides(root: &Path, globs: &[&str]) -> Result<Override, SearchError> {
    let mut builder = OverrideBuilder::new(root);
    for glob in globs {
        builder
            .add(glob)
            .map_err(|error| SearchError::InvalidGlob {
                pattern: (*glob).to_owned(),
                message: error.to_string(),
            })?;
    }
    builder.build().map_err(|error| SearchError::InvalidGlob {
        pattern: globs.join(" "),
        message: error.to_string(),
    })
}

/// Builds the walk.
///
/// `skip_hidden` carries [`WalkBuilder::hidden`]'s sense, which is the **inverse** of
/// the oracle's `--hidden` flag: `hidden(true)` means "skip hidden paths". The
/// parameter is named for the builder's polarity rather than the flag's so a call
/// site cannot silently mean the opposite of what it reads.
fn walker(root: &Path, overrides: Override, skip_hidden: bool, follow: bool) -> Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .overrides(overrides)
        .hidden(skip_hidden)
        .follow_links(follow)
        .sort_by_file_path(Path::cmp);
    builder.build()
}

/// Keeps regular files and silently drops everything else, which is `--no-messages`
/// plus `--files`: a directory is a traversal step, not a result, and an unreadable
/// entry is a diagnostic the oracle suppresses.
fn accept_file(result: Result<ignore::DirEntry, ignore::Error>) -> Option<ignore::DirEntry> {
    let entry = result.ok()?;
    entry
        .file_type()
        .is_some_and(|kind| kind.is_file())
        .then_some(entry)
}

fn build_matcher(pattern: &str) -> Result<RegexMatcher, SearchError> {
    RegexMatcherBuilder::new()
        // Rejects a pattern that could match a line terminator, which is what rg
        // does without `-U`; the oracle never passes `-U`.
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|error| SearchError::InvalidPattern {
            pattern: pattern.to_owned(),
            message: error.to_string(),
        })
}

fn build_searcher() -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .multi_line(false)
        // rg's default for an implicitly-discovered file: stop at the first NUL
        // rather than emit binary noise as matches.
        .binary_detection(BinaryDetection::quit(0))
        .build()
}

struct Outcome {
    cancelled: bool,
}

/// Drops one trailing `\n`, the line terminator [`build_searcher`] configures.
///
/// A preceding `\r` is deliberately left in place: the oracle never passes `--crlf`,
/// so ripgrep treats the carriage return as line content, and a pattern anchored with
/// `$` does not match before it.
fn strip_terminator(bytes: &[u8]) -> &[u8] {
    match bytes.split_last() {
        Some((b'\n', rest)) => rest,
        _ => bytes,
    }
}

fn search_one(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    entry: Entry,
    limit: usize,
    cancel: &dyn Cancellation,
    out: &mut Vec<Match>,
) -> Outcome {
    let mut collector = Collector {
        entry,
        matcher,
        out,
        limit,
        cancel,
        cancelled: false,
    };
    // A file that cannot be read is skipped, not fatal: `--no-messages`.
    let _ = searcher.search_path(matcher, path, &mut collector);
    Outcome {
        cancelled: collector.cancelled,
    }
}

struct Collector<'a> {
    entry: Entry,
    matcher: &'a RegexMatcher,
    out: &'a mut Vec<Match>,
    limit: usize,
    cancel: &'a dyn Cancellation,
    cancelled: bool,
}

impl Sink for &mut Collector<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        if self.cancel.is_cancelled() {
            self.cancelled = true;
            return Ok(false);
        }

        let bytes = matched.bytes();
        // Must be the line WITHOUT its terminator — what `grep-searcher` itself
        // matched against (`lines::without_terminator`, `searcher/core.rs:120`).
        // Searching the terminated slice loses every submatch of a `$`-anchored
        // pattern, because the regex crate has no "before a final newline" rule.
        let searchable = strip_terminator(bytes);
        let mut submatches = Vec::new();
        self.matcher
            .find_iter(searchable, |found| {
                if submatches.len() >= MAX_SUBMATCHES {
                    return false;
                }
                let (start, end) = (found.start(), found.end());
                submatches.push(Submatch {
                    text: String::from_utf8_lossy(&bytes[start..end]).into_owned(),
                    start,
                    end,
                });
                true
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;

        self.out.push(Match {
            entry: self.entry.clone(),
            line: matched.line_number().unwrap_or_default(),
            offset: matched.absolute_byte_offset(),
            text: truncate_utf16(&String::from_utf8_lossy(bytes), MAX_MATCH_TEXT),
            submatches,
        });

        // One past the limit is enough to know the results are truncated, and stops
        // the walk from reading a whole tree to build a list nobody will see.
        Ok(self.out.len() <= self.limit)
    }
}
