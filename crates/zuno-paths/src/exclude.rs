//! The repository-private exclude file, and the one block in it Zuno owns.
//!
//! Zuno writes generated files into the user's worktree: the goal projection
//! under `.zuno/goal/` is the current example, and it is rewritten on every
//! material change. Until something excludes such a path, `git status` reports
//! files the agent produced as the user's uncommitted work. That is not only
//! untidy — a model reads `git status` to decide what this turn changed, so a
//! dirty tree full of generated documents makes its reading of the repository
//! wrong.
//!
//! The exclusion belongs in `info/exclude` rather than in `.gitignore`.
//! `info/exclude` is per-clone, untracked and never shared, so writing it changes
//! nothing the user commits, pushes or reviews. Appending to `.gitignore` edits a
//! file that belongs to *their* project, in a repository they may not want
//! modified at all.
//!
//! # Why git is asked where the file lives
//!
//! [`resolve_exclude_path`] runs `git rev-parse --git-path info/exclude` instead of
//! joining `.git/info/exclude` onto the worktree. In an ordinary clone those agree.
//! In a linked worktree created by `git worktree add` they do not: `.git` is a
//! *file* pointing at a per-worktree administrative directory under
//! `<original>/.git/worktrees/<name>`, while `info/exclude` stays in the shared
//! common directory. Only `rev-parse --git-path` knows the difference, and it also
//! covers a submodule, a `GIT_DIR` override, and a `core.excludesFile`-style
//! relocation. Constructing the path by hand looks like the same thing right up to
//! the moment it writes a file git never reads, at which point the generated files
//! are still dirty and nothing says why.
//!
//! # What a managed block is for
//!
//! [`ensure_managed_block`] maintains exactly one marker-delimited block. Content
//! outside it is preserved byte for byte, so a user's own exclusions are never
//! touched; content inside it is owned by Zuno, so the set of generated paths can
//! change between releases without the file accumulating stale duplicates. The set
//! itself is declared once, in [`crate::generated`]; a host passes
//! [`crate::generated::IGNORE_PATTERNS`] here rather than spelling any path again.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::generated::{GENERATED_PATHS, IGNORE_PATTERNS};
use crate::project::resolve_git_path;

/// The exclude-file path, relative to the git directory, that git resolves for us.
pub const EXCLUDE_GIT_PATH: &str = "info/exclude";

/// The stable fragment of git's own message for a directory outside any repository.
///
/// The distinction between "there is no repository here" and "git refused" is taken
/// from this message rather than from the exit code, because git exits 128 for every
/// fatal — a rejected ownership check included — and reporting a fixable ownership
/// problem as "no repository here" would send a caller looking in the wrong place.
/// The child process is run with the C locale so the message is not translated.
const NOT_A_REPOSITORY: &str = "not a git repository";

/// Opens the block this module owns.
///
/// The marker names Zuno so a human reading their own exclude file can tell who
/// wrote the lines and that editing inside them is pointless.
pub const MANAGED_BLOCK_BEGIN: &str = "# BEGIN zuno managed excludes (generated; do not edit)";

/// Closes the block this module owns.
pub const MANAGED_BLOCK_END: &str = "# END zuno managed excludes";

/// What [`ensure_managed_block`] did to the exclude file.
///
/// The three cases are distinguished because a caller wants to log the first-time
/// write — the one a user might be surprised by — without logging on every turn
/// that re-asserts the same block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExcludeOutcome {
    /// No managed block was present, and one was written.
    ///
    /// Covers both a missing exclude file and an existing file that had no block.
    Created,
    /// A managed block was present with different entries and was rewritten.
    Updated,
    /// The managed block already listed exactly these entries; nothing was written.
    Unchanged,
}

impl ExcludeOutcome {
    /// True when the exclude file on disk was rewritten.
    #[must_use]
    pub const fn changed(self) -> bool {
        !matches!(self, Self::Unchanged)
    }
}

/// A failure while resolving or updating the repository-private exclude file.
///
/// Every variant is a refusal that wrote nothing, which is the property a caller
/// needs: there is no partial state to reconcile after any of them.
#[derive(Debug, thiserror::Error)]
pub enum ExcludeError {
    /// The directory is not inside a git repository, according to git.
    ///
    /// A directory with no repository above it has no private exclude file to
    /// write, and inventing one would leave a file nothing reads. Reported as a
    /// clean typed refusal because running Zuno outside a repository is ordinary,
    /// not exceptional.
    #[error("{} is not inside a git repository", worktree.display())]
    NotARepository {
        /// The directory git was asked about.
        worktree: PathBuf,
    },

    /// `git` could not be started at all.
    ///
    /// A machine without git in `PATH`, or a directory that no longer exists.
    #[error("failed to run git in {}", worktree.display())]
    GitUnavailable {
        /// The directory git would have run in.
        worktree: PathBuf,
        /// Why the process could not be spawned.
        #[source]
        source: io::Error,
    },

    /// `git` ran and did not produce a usable answer.
    ///
    /// `message` is git's own stderr where it wrote any, so a caller can report
    /// what git objected to instead of guessing from the exit code.
    #[error(
        "git rev-parse --git-path {EXCLUDE_GIT_PATH} failed in {}: {message}",
        worktree.display()
    )]
    GitFailed {
        /// The directory git ran in.
        worktree: PathBuf,
        /// The process exit code, absent when a signal ended it.
        status: Option<i32>,
        /// What git reported, or why its answer was unusable.
        message: String,
    },

    /// The filesystem refused to read or replace the exclude file.
    ///
    /// When the exclude file is a symbolic link, `path` is the file the link resolves
    /// to if that is where the failure happened, and the link itself when the link
    /// could not be followed or completed; `source` says which.
    #[error("failed to update the git exclude file {}", path.display())]
    Filesystem {
        /// The path that could not be read, created, followed or replaced.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// An entry spanned more than one line.
    ///
    /// Git exclude patterns are line-delimited, so a pattern cannot contain a
    /// newline: such an entry is not a pattern git could ever match. Rejecting it
    /// also keeps the block's own markers unforgeable, which is what makes
    /// [`ensure_managed_block`] idempotent.
    #[error("exclude entry must be a single line: {entry:?}")]
    InvalidEntry {
        /// The entry that could not be written as one line.
        entry: String,
    },
}

/// The exclude file git itself reads for `worktree`.
///
/// Runs `git rev-parse --git-path info/exclude` inside `worktree` and resolves the
/// answer against it, because git reports a path relative to the directory it ran
/// in when that directory is the worktree root and an absolute one otherwise. See
/// the module documentation for why the path is never assembled by hand.
///
/// The resolved path is not guaranteed to exist; `info/exclude` is created lazily
/// by whoever writes it first.
///
/// Whether `worktree` is a repository at all is git's answer too, not this crate's:
/// a leftover empty `.git` directory is not a repository, so the presence of the
/// marker proves nothing and only git can say.
///
/// # Errors
///
/// [`ExcludeError::GitUnavailable`] when git cannot be started.
/// [`ExcludeError::NotARepository`] when git reports no repository at or above
/// `worktree`. [`ExcludeError::GitFailed`] for any other non-zero exit, carrying
/// git's own message, or when git reports a path this crate cannot represent as
/// text. Nothing is written in any case.
pub fn resolve_exclude_path(worktree: &Path) -> Result<PathBuf, ExcludeError> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", EXCLUDE_GIT_PATH])
        .current_dir(worktree)
        .stdin(Stdio::null())
        // Pin the message locale so the classification below reads git's own words
        // rather than a translation of them.
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .output()
        .map_err(|source| ExcludeError::GitUnavailable {
            worktree: worktree.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if message.to_ascii_lowercase().contains(NOT_A_REPOSITORY) {
            return Err(ExcludeError::NotARepository {
                worktree: worktree.to_path_buf(),
            });
        }
        return Err(ExcludeError::GitFailed {
            worktree: worktree.to_path_buf(),
            status: output.status.code(),
            message,
        });
    }
    let reported = String::from_utf8(output.stdout).map_err(|_| ExcludeError::GitFailed {
        worktree: worktree.to_path_buf(),
        status: output.status.code(),
        message: "git reported an exclude path that is not valid UTF-8".to_owned(),
    })?;
    // `resolve_git_path` is the same trailing-newline-only, absolute-or-relative
    // resolution `Git.repo.discover` uses for `--git-dir`; the answer to
    // `--git-path` has exactly the same shape, so it gets the same treatment.
    Ok(PathBuf::from(resolve_git_path(
        &worktree.to_string_lossy(),
        &reported,
    )))
}

/// Make the repository-private exclude file list exactly `entries` inside the one
/// block Zuno owns.
///
/// Bytes outside the markers are preserved exactly, including the absence of a
/// final newline, so a user's own exclusions and comments survive untouched — and
/// so does the commented template `git init` itself writes into this file. A new
/// block is appended at the end of the file, on its own line.
///
/// # A begin marker with no end marker
///
/// A block can lose its closing line: a merge resolved badly, a line deleted by
/// hand, a copy that was cut short. Such a block is *not* taken to run to the end of
/// the file — that reading deletes every exclusion the user wrote below the orphaned
/// marker, silently, on a call whose whole purpose is tidiness. Instead the block is
/// taken to end after the last consecutive line Zuno itself could have written: a
/// begin marker, an entry passed to this call, or any pattern in
/// [`crate::generated::IGNORE_PATTERNS`]. The first line that is none of those is the
/// user's, and everything from it on is kept. The rewritten block closes itself, so
/// the orphan is healed on this call rather than growing a second block on the next,
/// and stacked orphaned markers collapse into one block.
///
/// The cost of never guessing is deliberate: a line only an older release wrote, or
/// one truncated in the middle, is not recognised and survives below the healed block
/// as an inert extra exclusion the user can delete. That is strictly better than the
/// alternative, which destroys exclusions that were never Zuno's to touch. Inside a
/// block that *is* closed, everything between the markers still belongs to Zuno.
///
/// # Line endings
///
/// The block is rendered with the file's own line ending: CRLF when more of the
/// existing line breaks are `\r\n` than bare `\n`, LF otherwise — including for a new
/// file, a file with no line break at all, and a tie, because LF is what git itself
/// writes here. A block Zuno wrote earlier gets no vote, so a block written with the
/// wrong ending is corrected once and then reported unchanged; without this a CRLF
/// file could never compare equal to the rendered block, and every call rewrote it.
///
/// # How the file is replaced
///
/// The replacement is a same-directory write-then-rename: an interrupted or failed
/// write leaves the temporary file behind for the filesystem to clean up and cannot
/// truncate the user's exclude file. This is the `std::fs` form of what
/// `zuno-atomic-file` provides; that crate is not used here because `zuno-paths` is
/// a foundation crate whose dependencies are deliberately limited to `thiserror` and
/// `url`, and depending on it would pull `uuid` and a Windows binding into
/// everything that resolves a path. On Unix the rename is atomic. On Windows
/// [`std::fs::rename`] replaces an existing destination through `MoveFileEx`, whose
/// publication is not gap-free — a caller that needs that boundary for a file more
/// valuable than an exclude list should use `zuno-atomic-file::replace`. Because the
/// published file is a new one, unusual permissions on a previous exclude file are
/// not carried over; this is a repository-private text file that git itself creates
/// with default permissions.
///
/// When the exclude file is a symbolic link — some setups point `info/exclude` at
/// one file shared by every clone on a machine, which git reads through the link —
/// the write lands on the file the link resolves to, through a chain of links if
/// there is one, and the link is left standing. Renaming over the link would replace
/// it with an ordinary file: the shared exclusions would silently stop applying here,
/// and edits made here would stop reaching the other clones. The temporary is created
/// beside the resolved file, so the rename stays atomic with respect to the file
/// actually being replaced; the target may sit anywhere, outside the repository
/// included, because that is what such a link is for. A link whose target does not
/// exist yet is completed the way `open(2)` completes it, by creating the target —
/// provided its directory exists. A target whose directory is missing usually means
/// an unmounted volume or a path from another machine, and manufacturing a directory
/// tree there is not this function's to do, so it is refused with
/// [`ExcludeError::Filesystem`] naming the link; so is a loop of links. Nothing is
/// written in either case.
///
/// Calling this twice with the same entries writes nothing the second time and
/// reports [`ExcludeOutcome::Unchanged`], so it is safe on every turn.
///
/// # Errors
///
/// [`ExcludeError::InvalidEntry`] when an entry contains a newline, which no git
/// exclude pattern can. Everything [`resolve_exclude_path`] can return, since the
/// path is resolved first. [`ExcludeError::Filesystem`] when the file cannot be
/// read, when its directory cannot be created, when a symbolic link cannot be
/// preserved, or when the replacement fails; in every case the previous contents are
/// still on disk.
pub fn ensure_managed_block(
    worktree: &Path,
    entries: &[&str],
) -> Result<ExcludeOutcome, ExcludeError> {
    for entry in entries {
        if entry.contains('\n') || entry.contains('\r') {
            return Err(ExcludeError::InvalidEntry {
                entry: (*entry).to_owned(),
            });
        }
    }
    let path = resolve_exclude_path(worktree)?;
    let filesystem = |(path, source)| ExcludeError::Filesystem { path, source };
    let Some(content) = read_optional(&path)? else {
        replace_atomically(&path, &render_block(entries, LineEnding::Lf)).map_err(filesystem)?;
        return Ok(ExcludeOutcome::Created);
    };
    let found = managed_block(&content, entries);
    let newline = LineEnding::dominant(&content, found.as_ref());
    let block = render_block(entries, newline);
    let Some(range) = found else {
        let mut next = Vec::with_capacity(content.len() + block.len() + 2);
        next.extend_from_slice(&content);
        if !content.is_empty() && !content.ends_with(b"\n") {
            next.extend_from_slice(newline.as_bytes());
        }
        next.extend_from_slice(&block);
        replace_atomically(&path, &next).map_err(filesystem)?;
        return Ok(ExcludeOutcome::Created);
    };
    if content[range.clone()] == block[..] {
        return Ok(ExcludeOutcome::Unchanged);
    }
    let mut next = Vec::with_capacity(content.len() + block.len());
    next.extend_from_slice(&content[..range.start]);
    next.extend_from_slice(&block);
    next.extend_from_slice(&content[range.end..]);
    replace_atomically(&path, &next).map_err(filesystem)?;
    Ok(ExcludeOutcome::Updated)
}

/// The line-break convention of a text file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    /// `\n`: what git itself writes into this file, and the default for a new one.
    Lf,
    /// `\r\n`: what an editor on Windows, or a checkout with `core.autocrlf`, leaves.
    CrLf,
}

impl LineEnding {
    const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
        }
    }

    /// The convention `content` follows: CRLF when more of its line breaks are `\r\n`
    /// than bare `\n`, LF otherwise, so a file with no line break and a tie both get
    /// LF.
    ///
    /// The bytes inside `managed` — the block Zuno wrote last time — are not counted,
    /// because a block rendered with the wrong ending is the thing being corrected and
    /// must not outvote the lines around it. A file that is nothing but the block
    /// falls back to counting the block, so it keeps the ending it has instead of
    /// flipping.
    fn dominant(content: &[u8], managed: Option<&Range<usize>>) -> Self {
        let verdict = |(crlf, lf): (usize, usize)| {
            if crlf > lf { Self::CrLf } else { Self::Lf }
        };
        let Some(range) = managed else {
            return verdict(count_line_breaks(content));
        };
        let (before_crlf, before_lf) = count_line_breaks(&content[..range.start]);
        let (after_crlf, after_lf) = count_line_breaks(&content[range.end..]);
        let outside = (before_crlf + after_crlf, before_lf + after_lf);
        if outside == (0, 0) {
            verdict(count_line_breaks(content))
        } else {
            verdict(outside)
        }
    }
}

/// How many line breaks in `bytes` are `\r\n` and how many are a bare `\n`.
fn count_line_breaks(bytes: &[u8]) -> (usize, usize) {
    let mut crlf = 0;
    let mut lf = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if index > 0 && bytes[index - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    (crlf, lf)
}

/// The block as it belongs on disk: one marker line, one line per entry, one
/// closing marker line, every line ended with `newline`.
fn render_block(entries: &[&str], newline: LineEnding) -> Vec<u8> {
    let newline = newline.as_bytes();
    let mut block = Vec::new();
    block.extend_from_slice(MANAGED_BLOCK_BEGIN.as_bytes());
    block.extend_from_slice(newline);
    for entry in entries {
        block.extend_from_slice(entry.as_bytes());
        block.extend_from_slice(newline);
    }
    block.extend_from_slice(MANAGED_BLOCK_END.as_bytes());
    block.extend_from_slice(newline);
    block
}

/// The byte range of the managed block, markers and their line breaks included.
///
/// Markers are matched on the trimmed line so an editor that added or stripped
/// trailing whitespace, or a `\r`, cannot orphan a block and cause a duplicate to be
/// appended. The file is treated as bytes rather than text because an exclude file
/// may name a path that is not valid UTF-8, and rewriting it must not re-encode one.
///
/// A closed block runs from the first begin marker to the first end marker after it.
/// A begin marker with no end marker runs only as far as the consecutive lines Zuno
/// could have written — see [`ensure_managed_block`] — so the caller's rewrite closes
/// the block without consuming what follows.
fn managed_block(content: &[u8], entries: &[&str]) -> Option<Range<usize>> {
    let mut begin: Option<usize> = None;
    // End of the last line, from the begin marker on, that still looks like block
    // content; it only advances while that run is unbroken.
    let mut owned_end = 0usize;
    let mut offset = 0usize;
    for line in content.split_inclusive(|byte| *byte == b'\n') {
        let text = line.trim_ascii();
        let next = offset + line.len();
        match begin {
            None => {
                if text == MANAGED_BLOCK_BEGIN.as_bytes() {
                    begin = Some(offset);
                    owned_end = next;
                }
            }
            Some(start) => {
                if text == MANAGED_BLOCK_END.as_bytes() {
                    return Some(start..next);
                }
                if owned_end == offset && is_block_content(text, entries) {
                    owned_end = next;
                }
            }
        }
        offset = next;
    }
    begin.map(|start| start..owned_end)
}

/// Whether a trimmed line is one this module writes into a block: a begin marker, or
/// an entry — from this call, from the pattern set Zuno writes today, or from the
/// registry of named generated directories, since hosts pass different subsets and a
/// block one host left unterminated must be recognised in full by another.
///
/// The registry is checked as well as [`IGNORE_PATTERNS`] because the two differ across
/// releases: a block written before the pattern set became `.zuno/*` plus negations
/// names `.zuno/goal/` and its siblings line by line. Those lines are Zuno's, and an
/// upgrade has to absorb them into the block it rewrites instead of leaving them behind
/// as if a person had typed them.
fn is_block_content(text: &[u8], entries: &[&str]) -> bool {
    let registered = GENERATED_PATHS.iter().map(|generated| generated.pattern);
    text == MANAGED_BLOCK_BEGIN.as_bytes()
        || entries
            .iter()
            .copied()
            .chain(IGNORE_PATTERNS.iter().copied())
            .chain(registered)
            .any(|entry| entry.as_bytes().trim_ascii() == text)
}

/// The current bytes, or `None` when the exclude file does not exist yet.
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, ExcludeError> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ExcludeError::Filesystem {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Publish `contents` at `path` through a sibling temporary file and a rename.
///
/// `path` is followed through any symbolic links first, and the temporary is created
/// beside — and renamed onto — the file at the end of the chain, so a link survives
/// and the rename stays within one directory. [`ensure_managed_block`] describes the
/// cases a link can present and how each is treated.
///
/// Shared with [`crate::generated_dir`], which publishes a generated directory's own
/// `.gitignore` the same way and reports a different error type, so the failure is
/// returned as the path that failed and what the filesystem said rather than as an
/// [`ExcludeError`].
///
/// # Errors
///
/// The path the write was attempted at and the operating system's error, when the
/// destination cannot be resolved, its directory is missing behind a link, the
/// temporary cannot be created or written, or the rename fails. Nothing is left
/// behind in any of those cases: the previous contents stay on disk and the temporary
/// is removed.
pub(crate) fn replace_atomically(path: &Path, contents: &[u8]) -> Result<(), (PathBuf, io::Error)> {
    let filesystem = |path: &Path| {
        let path = path.to_path_buf();
        move |source| (path, source)
    };
    let destination = resolve_destination(path).map_err(filesystem(path))?;
    let parent = destination
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if destination.through_link {
        // The target is wherever the user pointed the link. Creating the file there
        // completes the link; creating directories there invents a place.
        if !parent.is_dir() {
            return Err((
                path.to_path_buf(),
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "it is a symbolic link to {}, whose directory does not exist",
                        destination.path.display()
                    ),
                ),
            ));
        }
    } else {
        fs::create_dir_all(parent).map_err(filesystem(parent))?;
    }
    let temporary = parent.join(temporary_name(&destination.path));
    let published = (|| -> io::Result<()> {
        // `create_new` so a colliding temporary name is an error rather than a
        // silent overwrite of somebody else's in-flight file.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        drop(file);
        fs::rename(&temporary, &destination.path)
    })();
    if published.is_err() {
        let _ignored = fs::remove_file(&temporary);
    }
    published.map_err(filesystem(&destination.path))
}

/// Where a write to a path lands once symbolic links are followed.
struct Destination {
    /// The file to replace: the path itself, or the end of its chain of links.
    path: PathBuf,
    /// Whether at least one link was followed to get there.
    through_link: bool,
}

/// Follow `path` through symbolic links to the file a write must replace.
///
/// Only the final component is followed here. A link in a parent directory is
/// resolved by the operating system for the temporary and the destination alike, so
/// both already land in the same real directory. A relative link target is relative
/// to the directory holding the link, not to the process. A path that does not exist
/// ends the chain, whether it is a fresh exclude file or the missing target of a
/// broken link; the caller decides what to do about that.
///
/// # Errors
///
/// Any failure to inspect or read a link, and a chain longer than the operating
/// system itself would follow, which is a loop or as good as one.
fn resolve_destination(path: &Path) -> io::Result<Destination> {
    // Linux stops at 40 (`MAXSYMLINKS`); a real chain never comes close.
    const MAX_LINKS: usize = 40;
    let mut current = path.to_path_buf();
    let mut through_link = false;
    for _ in 0..MAX_LINKS {
        let is_link = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata.file_type().is_symlink(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if !is_link {
            return Ok(Destination {
                path: current,
                through_link,
            });
        }
        through_link = true;
        let target = fs::read_link(&current)?;
        // `join` keeps an absolute target as it is and anchors a relative one.
        current = match current.parent() {
            Some(directory) => directory.join(target),
            None => target,
        };
    }
    Err(io::Error::other(format!(
        "{} is a chain of more than {MAX_LINKS} symbolic links",
        path.display()
    )))
}

/// A temporary name beside the destination, unique per process, instant and call.
fn temporary_name(path: &Path) -> OsString {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("exclude"))
        .to_os_string();
    name.push(format!(
        ".zuno-tmp.{}.{nanos}.{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    name
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    const GENERATED: &str = ".zuno/goal/";

    fn run(cwd: &Path, args: &[&str]) -> String {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", null_device)
            .env("GIT_CONFIG_SYSTEM", null_device)
            .env("GIT_AUTHOR_NAME", "zuno-paths")
            .env("GIT_AUTHOR_EMAIL", "zuno-paths@example.test")
            .env("GIT_COMMITTER_NAME", "zuno-paths")
            .env("GIT_COMMITTER_EMAIL", "zuno-paths@example.test")
            .output()
            .unwrap_or_else(|error| panic!("spawn {args:?}: {error}"));
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is UTF-8")
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path();
        run(path, &["git", "init", "--initial-branch=main", "."]);
        fs::write(path.join("file.txt"), "hello\n").expect("write file");
        run(path, &["git", "add", "file.txt"]);
        run(path, &["git", "commit", "-m", "initial"]);
        root
    }

    fn assert_same_path(actual: &Path, expected: &Path) {
        assert_eq!(
            actual.canonicalize().expect("canonicalize actual path"),
            expected.canonicalize().expect("canonicalize expected path"),
            "paths identify different filesystem entries: actual={} expected={}",
            actual.display(),
            expected.display()
        );
    }

    /// Write a file at a generated path so `git status` has something to report if
    /// the exclusion did not take effect.
    fn generate(worktree: &Path) {
        let directory = worktree.join(".zuno").join("goal");
        fs::create_dir_all(&directory).expect("create the generated directory");
        fs::write(directory.join("GOAL.md"), "# Goal\n").expect("write the generated document");
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    #[test]
    fn a_plain_repository_resolves_the_exclude_file_inside_its_own_git_directory() {
        let root = repository();

        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");

        assert_same_path(
            path.parent().expect("info directory"),
            &root.path().join(".git").join("info"),
        );
        assert_eq!(path.file_name(), Some(OsStr::new("exclude")));
    }

    /// In a linked worktree `.git` is a file pointing at a per-worktree
    /// administrative directory, while `info/exclude` stays in the shared common
    /// directory. A hand-built `.git/info/exclude` would be a path git never reads,
    /// so this asserts the shared location and then proves git honours it.
    #[test]
    fn a_linked_worktree_resolves_the_shared_common_directory_exclude_file() {
        let root = repository();
        let linked = root.path().join("linked");
        run(
            root.path(),
            &[
                "git",
                "worktree",
                "add",
                linked.to_str().expect("utf8 path"),
                "-b",
                "side",
            ],
        );
        assert!(
            root.path().join("linked").join(".git").is_file(),
            "git worktree add must produce a .git file, not a directory"
        );

        let path = resolve_exclude_path(&linked).expect("resolve the exclude path");
        assert_same_path(
            &path,
            &root.path().join(".git").join("info").join("exclude"),
        );

        let outcome = ensure_managed_block(&linked, &[GENERATED]).expect("write the block");
        assert_eq!(outcome, ExcludeOutcome::Created);
        generate(&linked);
        assert_eq!(
            run(&linked, &["git", "status", "--porcelain"]),
            "",
            "the exclusion must take effect in the worktree that asked for it"
        );
    }

    #[test]
    fn an_excluded_generated_path_leaves_git_status_clean() {
        let root = repository();
        generate(root.path());
        assert!(
            !run(root.path(), &["git", "status", "--porcelain"]).is_empty(),
            "the fixture must be dirty before the exclusion, or it proves nothing"
        );

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("write the block");

        assert_eq!(outcome, ExcludeOutcome::Created);
        assert!(outcome.changed());
        assert_eq!(run(root.path(), &["git", "status", "--porcelain"]), "");
    }

    #[test]
    fn a_second_call_with_the_same_entries_reports_unchanged_and_rewrites_nothing() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        assert_eq!(
            ensure_managed_block(root.path(), &[GENERATED]).expect("first call"),
            ExcludeOutcome::Created
        );
        let after_first = read(&path);

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("second call");

        assert_eq!(outcome, ExcludeOutcome::Unchanged);
        assert!(!outcome.changed());
        assert_eq!(read(&path), after_first);
        assert_eq!(
            after_first.matches(MANAGED_BLOCK_BEGIN).count(),
            1,
            "exactly one managed block, never a second copy:\n{after_first}"
        );
    }

    #[test]
    fn changing_the_entries_rewrites_only_the_managed_block() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(&path, "# mine\nbuild/\n").expect("pre-existing content");
        ensure_managed_block(root.path(), &[GENERATED]).expect("first call");

        let outcome =
            ensure_managed_block(root.path(), &[GENERATED, ".zuno/tmp/"]).expect("changed entries");

        assert_eq!(outcome, ExcludeOutcome::Updated);
        let content = read(&path);
        assert_eq!(
            content,
            format!(
                "# mine\nbuild/\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n.zuno/tmp/\n{MANAGED_BLOCK_END}\n"
            )
        );
        assert_eq!(content.matches(MANAGED_BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn content_outside_the_managed_block_survives_byte_for_byte() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        // No final newline, and content on both sides of where the block will go.
        let before = "# my own exclusions\n*.local\n";
        fs::write(&path, before).expect("pre-existing content");
        ensure_managed_block(root.path(), &[GENERATED]).expect("append the block");
        let with_block = read(&path);
        fs::write(&path, format!("{with_block}trailing-without-newline"))
            .expect("trailing content");

        ensure_managed_block(root.path(), &[".zuno/other/"]).expect("rewrite the block");

        let content = read(&path);
        assert!(
            content.starts_with(before),
            "leading content lost:\n{content}"
        );
        assert!(
            content.ends_with("trailing-without-newline"),
            "trailing content lost:\n{content}"
        );
        assert!(content.contains(".zuno/other/"), "{content}");
        assert!(
            !content.contains(GENERATED),
            "the old entry must be gone from the block:\n{content}"
        );
    }

    /// The tradeoff of never guessing what an orphaned block held: `stale/` may be an
    /// entry an older release wrote or a line the user typed, and nothing in the file
    /// says which. It survives below the healed block as an inert exclusion rather than
    /// being deleted on a guess, and the block is closed so nothing accumulates.
    #[test]
    fn an_unrecognised_line_under_an_orphaned_begin_marker_is_kept_rather_than_guessed_stale() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(&path, format!("# mine\n{MANAGED_BLOCK_BEGIN}\nstale/\n"))
            .expect("a truncated block");

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("heal the block");

        assert_eq!(outcome, ExcludeOutcome::Updated);
        let content = read(&path);
        assert_eq!(
            content,
            format!("# mine\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\nstale/\n")
        );
        assert_eq!(
            ensure_managed_block(root.path(), &[GENERATED]).expect("second call"),
            ExcludeOutcome::Unchanged
        );
        assert_eq!(read(&path), content);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_a_typed_error_that_writes_nothing() {
        let outside = tempfile::tempdir().expect("tempdir");

        let error = ensure_managed_block(outside.path(), &[GENERATED])
            .expect_err("there is no repository to exclude anything from");

        let ExcludeError::NotARepository { worktree } = &error else {
            panic!("expected a typed non-repository refusal, got {error:?}");
        };
        assert_eq!(worktree, outside.path());
        assert!(
            error
                .to_string()
                .contains(&outside.path().display().to_string()),
            "{error}"
        );
        assert_eq!(
            fs::read_dir(outside.path())
                .expect("read the temporary directory")
                .count(),
            0,
            "a refusal must not create a .git directory or an exclude file"
        );
        assert!(
            error.source().is_none(),
            "a directory with no repository is not a wrapped failure"
        );
        assert!(matches!(
            resolve_exclude_path(outside.path()),
            Err(ExcludeError::NotARepository { .. })
        ));
    }

    #[test]
    fn an_entry_spanning_more_than_one_line_is_refused_before_anything_is_written() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");

        let before = fs::read(&path).unwrap_or_default();

        let error = ensure_managed_block(
            root.path(),
            &[&format!("evil/\n{MANAGED_BLOCK_END}\nnot-excluded/")],
        )
        .expect_err("a multi-line entry cannot be a git exclude pattern");

        let ExcludeError::InvalidEntry { entry } = &error else {
            panic!("expected a typed invalid entry, got {error:?}");
        };
        assert!(entry.contains("evil/"), "{entry}");
        assert_eq!(
            fs::read(&path).unwrap_or_default(),
            before,
            "a refused entry must leave the exclude file exactly as it was"
        );
        assert!(!before.is_empty(), "git init writes a template here");
    }

    /// `git init` writes a commented template into `info/exclude`, so the very
    /// first call already has a file to preserve. An empty entry list is the
    /// degenerate case of that, and it must still leave one well-formed block.
    #[test]
    fn an_empty_entry_list_still_maintains_one_well_formed_block() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        let template = read(&path);
        assert!(
            template.contains("exclude-from"),
            "git no longer writes a template; the fixture needs revisiting:\n{template}"
        );

        assert_eq!(
            ensure_managed_block(root.path(), &[]).expect("an empty block"),
            ExcludeOutcome::Created
        );

        assert_eq!(
            read(&path),
            format!("{template}{MANAGED_BLOCK_BEGIN}\n{MANAGED_BLOCK_END}\n"),
            "git's own template must survive byte for byte ahead of the block"
        );
        assert_eq!(
            ensure_managed_block(root.path(), &[]).expect("second empty block"),
            ExcludeOutcome::Unchanged
        );
    }

    #[test]
    fn a_block_missing_its_end_marker_does_not_delete_the_lines_below_it() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        let users_own = "*.local\nbuild/\n# a comment the user wrote\n";
        fs::write(
            &path,
            format!("# mine\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{users_own}"),
        )
        .expect("a block that lost its end marker");

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("heal the block");

        assert_eq!(outcome, ExcludeOutcome::Updated);
        let content = read(&path);
        assert_eq!(
            content,
            format!("# mine\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n{users_own}"),
            "the user's lines below the orphaned marker must survive and the block must close"
        );

        assert_eq!(
            ensure_managed_block(root.path(), &[GENERATED]).expect("second call"),
            ExcludeOutcome::Unchanged
        );
        let again = read(&path);
        assert_eq!(again, content);
        assert_eq!(again.matches(MANAGED_BLOCK_BEGIN).count(), 1, "{again}");
        assert_eq!(again.matches(MANAGED_BLOCK_END).count(), 1, "{again}");
    }

    /// Hosts pass different subsets of the pattern set, and a released Zuno wrote a
    /// different set than this one, so a block left unterminated by either must be
    /// recognised in full or its remaining lines would be mistaken for the user's.
    ///
    /// The fixture is deliberately the union: the directory patterns an older release
    /// listed one by one, and the `.zuno/*`-plus-negations set written today.
    #[test]
    fn an_orphaned_block_is_recognised_by_every_pattern_zuno_writes_not_only_this_calls_entries() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        assert!(
            !IGNORE_PATTERNS.contains(&GENERATED),
            "the fixture assumes a directory pattern only an older release wrote"
        );
        assert!(
            GENERATED_PATHS
                .iter()
                .any(|generated| generated.pattern == GENERATED),
            "the fixture assumes the goal projection is a registered directory"
        );
        let mut truncated = format!("{MANAGED_BLOCK_BEGIN}\n");
        for pattern in GENERATED_PATHS.iter().map(|generated| generated.pattern) {
            truncated.push_str(pattern);
            truncated.push('\n');
        }
        for pattern in IGNORE_PATTERNS {
            truncated.push_str(pattern);
            truncated.push('\n');
        }
        truncated.push_str("mine/\n");
        fs::write(&path, &truncated).expect("a truncated block from another host");

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("heal the block");

        assert_eq!(outcome, ExcludeOutcome::Updated);
        assert_eq!(
            read(&path),
            format!("{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\nmine/\n"),
            "every line Zuno ever wrote is absorbed into the block; the user's is not"
        );
    }

    #[test]
    fn stacked_orphaned_begin_markers_collapse_into_one_closed_block() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(
            &path,
            format!(
                "{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\nmine/\n"
            ),
        )
        .expect("two orphaned blocks");

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("heal the blocks");

        assert_eq!(outcome, ExcludeOutcome::Updated);
        let content = read(&path);
        assert_eq!(
            content,
            format!("{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\nmine/\n")
        );
        assert_eq!(
            ensure_managed_block(root.path(), &[GENERATED]).expect("second call"),
            ExcludeOutcome::Unchanged
        );
        assert_eq!(read(&path), content);
    }

    #[test]
    fn a_crlf_file_gets_a_crlf_block_and_a_second_call_reports_unchanged() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(&path, "# mine\r\nbuild/\r\n").expect("a CRLF exclude file");

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("first call");

        assert_eq!(outcome, ExcludeOutcome::Created);
        let content = read(&path);
        assert_eq!(
            content,
            format!(
                "# mine\r\nbuild/\r\n{MANAGED_BLOCK_BEGIN}\r\n{GENERATED}\r\n{MANAGED_BLOCK_END}\r\n"
            )
        );
        assert!(
            !content.replace("\r\n", "").contains('\n'),
            "no bare line feed may be introduced into a CRLF file:\n{content:?}"
        );
        generate(root.path());
        assert_eq!(
            run(root.path(), &["git", "status", "--porcelain"]),
            "",
            "git must honour the block with CRLF endings"
        );

        assert_eq!(
            ensure_managed_block(root.path(), &[GENERATED]).expect("second call"),
            ExcludeOutcome::Unchanged
        );
        assert_eq!(read(&path), content);
    }

    /// The leftover of the old behaviour: a block written with LF into a CRLF file.
    /// The block itself must not outvote the user's lines, or the file would stay
    /// mixed for ever; it is normalised once and then left alone.
    #[test]
    fn an_lf_block_inside_a_crlf_file_is_normalised_once_and_then_left_alone() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(
            &path,
            format!("# mine\r\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n"),
        )
        .expect("a CRLF file with an LF block");

        let outcome = ensure_managed_block(root.path(), &[GENERATED]).expect("first call");

        assert_eq!(outcome, ExcludeOutcome::Updated);
        let content = read(&path);
        assert_eq!(
            content,
            format!("# mine\r\n{MANAGED_BLOCK_BEGIN}\r\n{GENERATED}\r\n{MANAGED_BLOCK_END}\r\n")
        );
        assert_eq!(
            ensure_managed_block(root.path(), &[GENERATED]).expect("second call"),
            ExcludeOutcome::Unchanged
        );
        assert_eq!(read(&path), content);
    }

    #[test]
    fn a_file_with_no_line_break_at_all_gets_lf_endings() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(&path, "# mine").expect("a file with no line break");

        ensure_managed_block(root.path(), &[GENERATED]).expect("append the block");

        assert_eq!(
            read(&path),
            format!("# mine\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n")
        );
    }

    #[test]
    fn a_tie_between_line_endings_resolves_to_lf() {
        let root = repository();
        let path = resolve_exclude_path(root.path()).expect("resolve the exclude path");
        fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
        fs::write(&path, "# mine\r\nbuild/\n").expect("one line break of each kind");

        ensure_managed_block(root.path(), &[GENERATED]).expect("append the block");

        assert_eq!(
            read(&path),
            format!("# mine\r\nbuild/\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n")
        );
    }

    /// Symbolic links are created through the Unix API, so these run on Unix only. The
    /// resolution under test is written against `std::fs` and compiles on every target.
    #[cfg(unix)]
    mod symlinks {
        use std::os::unix::fs::symlink;

        use super::*;

        /// Replace the exclude file git created with a link to `target`.
        fn link_exclude_to(root: &Path, target: &Path) -> PathBuf {
            let path = resolve_exclude_path(root).expect("resolve the exclude path");
            fs::create_dir_all(path.parent().expect("info directory")).expect("create info");
            if fs::symlink_metadata(&path).is_ok() {
                fs::remove_file(&path).expect("remove the file git created");
            }
            symlink(target, &path).expect("link the exclude file");
            path
        }

        fn assert_is_link_to(path: &Path, target: &Path) {
            let metadata = fs::symlink_metadata(path).expect("inspect the link");
            assert!(
                metadata.file_type().is_symlink(),
                "{} must still be a symbolic link",
                path.display()
            );
            assert_eq!(fs::read_link(path).expect("read the link"), target);
        }

        fn names_in(directory: &Path) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
                .map(|entry| {
                    entry
                        .expect("directory entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }

        /// A failed or finished publish must not leave a temporary beside the link.
        fn assert_no_temporary_left_in(directory: &Path) {
            let names = names_in(directory);
            assert!(
                names.iter().all(|name| !name.contains(".zuno-tmp.")),
                "temporary left behind in {}: {names:?}",
                directory.display()
            );
        }

        #[test]
        fn a_symlinked_exclude_file_keeps_its_link_and_the_target_receives_the_block() {
            let root = repository();
            let shared = tempfile::tempdir().expect("a directory outside the repository");
            let target = shared.path().join("exclude");
            fs::write(&target, "# shared\nbuild/\n").expect("the shared file");
            let path = link_exclude_to(root.path(), &target);

            let outcome =
                ensure_managed_block(root.path(), &[GENERATED]).expect("write through the link");

            assert_eq!(outcome, ExcludeOutcome::Created);
            assert_is_link_to(&path, &target);
            assert_eq!(
                read(&target),
                format!(
                    "# shared\nbuild/\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n"
                )
            );
            assert_eq!(names_in(shared.path()), ["exclude"]);
            assert_no_temporary_left_in(path.parent().expect("info directory"));
            generate(root.path());
            assert_eq!(
                run(root.path(), &["git", "status", "--porcelain"]),
                "",
                "git reads the exclude file through the link"
            );

            assert_eq!(
                ensure_managed_block(root.path(), &[GENERATED]).expect("second call"),
                ExcludeOutcome::Unchanged
            );
            assert_is_link_to(&path, &target);
        }

        #[test]
        fn a_chain_of_symlinks_is_followed_to_the_file_at_its_end() {
            let root = repository();
            let shared = tempfile::tempdir().expect("a directory outside the repository");
            let target = shared.path().join("exclude");
            fs::write(&target, "# shared\n").expect("the shared file");
            // A relative link, resolved against its own directory and not the process's.
            let middle = shared.path().join("current");
            symlink("exclude", &middle).expect("the middle link");
            let path = link_exclude_to(root.path(), &middle);

            let outcome =
                ensure_managed_block(root.path(), &[GENERATED]).expect("write through the chain");

            assert_eq!(outcome, ExcludeOutcome::Created);
            assert_is_link_to(&path, &middle);
            assert_is_link_to(&middle, Path::new("exclude"));
            assert_eq!(
                read(&target),
                format!("# shared\n{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n")
            );
            assert_eq!(names_in(shared.path()), ["current", "exclude"]);
        }

        #[test]
        fn a_broken_symlink_whose_directory_exists_is_completed_by_creating_its_target() {
            let root = repository();
            let shared = tempfile::tempdir().expect("a directory outside the repository");
            let target = shared.path().join("exclude");
            let path = link_exclude_to(root.path(), &target);
            assert!(!target.exists(), "the fixture needs the target absent");

            let outcome =
                ensure_managed_block(root.path(), &[GENERATED]).expect("complete the link");

            assert_eq!(outcome, ExcludeOutcome::Created);
            assert_is_link_to(&path, &target);
            assert_eq!(
                read(&target),
                format!("{MANAGED_BLOCK_BEGIN}\n{GENERATED}\n{MANAGED_BLOCK_END}\n")
            );
            assert_eq!(names_in(shared.path()), ["exclude"]);
            generate(root.path());
            assert_eq!(run(root.path(), &["git", "status", "--porcelain"]), "");
        }

        #[test]
        fn a_broken_symlink_into_a_missing_directory_is_refused_without_writing_anything() {
            let root = repository();
            let shared = tempfile::tempdir().expect("a directory outside the repository");
            let target = shared.path().join("missing").join("exclude");
            let path = link_exclude_to(root.path(), &target);

            let error = ensure_managed_block(root.path(), &[GENERATED])
                .expect_err("there is no directory to create the target in");

            let ExcludeError::Filesystem {
                path: reported,
                source,
            } = &error
            else {
                panic!("expected a filesystem refusal naming the link, got {error:?}");
            };
            assert_eq!(
                reported, &path,
                "the error names the exclude path the user knows"
            );
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
            assert!(
                source.to_string().contains(&target.display().to_string()),
                "the cause names the link's target: {source}"
            );
            assert_is_link_to(&path, &target);
            assert!(
                names_in(shared.path()).is_empty(),
                "nothing may be created at the target"
            );
            assert_no_temporary_left_in(path.parent().expect("info directory"));
        }

        #[test]
        fn a_loop_of_symlinks_is_refused_without_writing_anything() {
            let root = repository();
            let path = link_exclude_to(root.path(), Path::new("exclude"));

            let error = ensure_managed_block(root.path(), &[GENERATED])
                .expect_err("a link to itself resolves to nothing");

            assert!(
                matches!(error, ExcludeError::Filesystem { .. }),
                "{error:?}"
            );
            assert_is_link_to(&path, Path::new("exclude"));
            assert_no_temporary_left_in(path.parent().expect("info directory"));
        }
    }
}
