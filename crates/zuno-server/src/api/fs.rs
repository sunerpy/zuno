//! The three read-only filesystem operations: `fs/read`, `fs/list`, `fs/find`.
//!
//! # The one place a defect here is a security defect
//!
//! Every path in this module is attacker-controlled. `GET /api/fs/read/{*path}`
//! takes the whole remainder of the URL, and `fs/list` takes a `path` query. The
//! server is loopback-bound and may be unauthenticated, so a containment bug is
//! an arbitrary-file-read primitive against whatever the process can open —
//! `~/.aws/credentials`, `/etc/shadow`, the user's `auth.json`.
//!
//! So containment is enforced in [`Sandbox::resolve`] and nowhere else. Both
//! handlers that take a path go through it; there is no second path that reaches
//! `std::fs` with a caller-supplied component.
//!
//! ## Two stages, because one is not enough
//!
//! Ported from `packages/core/src/filesystem.ts:66-72`, which does exactly this
//! and for the same reason:
//!
//! 1. **Lexical** — `path::absolute`-style resolution against the root, then
//!    [`contains`]. This rejects `..`, an absolute path, and a UNC/`//host` form
//!    before any syscall touches the target.
//! 2. **Real** — `canonicalize` the resolved target and check containment again
//!    against the canonicalized root. This is what rejects a *symlink* pointing
//!    out of the root, which stage 1 cannot see because lexically the link is a
//!    perfectly ordinary child.
//!
//! Dropping stage 2 is the classic mistake: `ln -s /etc/shadow inside/link` is
//! lexically contained and reads `/etc/shadow`.
//!
//! ## Deliberately stricter than the oracle
//!
//! Upstream turns a containment failure into an `Effect.die`, which the HTTP
//! layer renders as an opaque `500 UnknownError` with a random `ref`. That is a
//! *looser* answer than this port gives: it tells the caller nothing, and it is
//! indistinguishable from "the disk is broken". This port answers **`403` with
//! `path_escaped_root`** and names the violation. The task's rule is explicit —
//! where upstream is looser, be stricter and say so — so this divergence is
//! intentional and recorded, and it is the only intentional divergence in this
//! module.
//!
//! `fs/find` takes no path at all; it is rooted at the session directory by
//! construction, so it cannot be pointed elsewhere. That matches
//! `filesystem/search.ts`, where the index is built with the location as its
//! base.

use std::path::{Component, Path, PathBuf};

use axum::extract::{Path as PathParam, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::catalog::LocationEnvelope;
use super::error::ApiError;
use super::state::ApiState;

/// How many entries `fs/find` returns when the caller does not say.
///
/// `filesystem/search.ts:193` — `input.limit ?? 50`.
const DEFAULT_FIND_LIMIT: usize = 50;

/// Directories `fs/find` never descends into.
///
/// Upstream reaches the same outcome through ripgrep, which honours `.gitignore`
/// and skips `.git`. This port does not shell out, so the exclusions are named
/// here; see the module note in [`find`] on where that leaves ranking parity.
const FIND_EXCLUDED: &[&str] = &[".git", "node_modules", "target", ".jj", ".hg", ".svn"];

/// One directory entry, upstream's `FileSystem.Entry`
/// (`packages/schema/src/filesystem.ts:14-18`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The `/`-separated path relative to the session directory. A directory carries
    /// a trailing slash, which is upstream's marker rather than a cosmetic choice
    /// (`filesystem.ts:100`).
    pub path: String,
    /// `file` or `directory`; upstream drops every other kind.
    #[serde(rename = "type")]
    pub kind: EntryKind,
}

/// The two entry kinds upstream reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
}

/// `GET /api/fs/list` query.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// The directory to list, relative to the session directory. Absent means
    /// the session directory itself (`filesystem.ts:67` — `input ?? "."`).
    path: Option<String>,
}

/// `GET /api/fs/find` query.
///
/// `query` is required by upstream's schema, and a missing one is a **400 before
/// the handler runs** (`protocol/src/groups/fs.ts:13-18`). Modelling it as
/// `Option` here and rejecting it in the handler reproduces that status with a
/// message that names the same key, which is what the differential compares.
#[derive(Debug, Deserialize)]
pub struct FindQuery {
    /// The needle.
    query: Option<String>,
    /// Restrict to files or to directories.
    #[serde(rename = "type")]
    kind: Option<EntryFilter>,
    /// Cap on returned entries. Upstream requires a positive integer.
    limit: Option<String>,
}

/// The `type` filter `fs/find` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EntryFilter {
    /// Only files.
    File,
    /// Only directories.
    Directory,
}

/// A session directory and the containment rule that guards it.
struct Sandbox {
    /// The session directory as configured, un-canonicalized. Stage 1 resolves
    /// against this so that the relative paths reported back to the caller are
    /// the ones they asked about rather than canonicalized aliases.
    root: PathBuf,
    /// The canonicalized session directory. Stage 2 compares against this.
    real_root: PathBuf,
}

/// What a resolved, contained path looks like.
struct Resolved {
    /// The lexically resolved target, still expressed under [`Sandbox::root`].
    absolute: PathBuf,
    /// The canonicalized target, proven to be inside [`Sandbox::real_root`].
    real: PathBuf,
}

impl Sandbox {
    /// Opens the sandbox for a session directory.
    ///
    /// # Errors
    /// Returns [`ApiError::FilesystemUnavailable`] when the session directory
    /// itself cannot be canonicalized, because with no trustworthy root there is
    /// no containment check to make and serving the request anyway would be
    /// serving it unguarded.
    fn open(root: &str) -> Result<Self, ApiError> {
        let root = PathBuf::from(root);
        let real_root = root
            .canonicalize()
            .map_err(|_| ApiError::FilesystemUnavailable)?;
        Ok(Self { root, real_root })
    }

    /// Resolves a caller-supplied relative path inside the sandbox.
    ///
    /// # Errors
    /// Returns [`ApiError::PathEscapedRoot`] when either containment stage fails,
    /// and [`ApiError::PathNotFound`] when the target does not exist. The escape
    /// check runs *before* the existence check so that probing for files outside
    /// the root cannot be used to tell "absent" from "present but forbidden".
    fn resolve(&self, input: Option<&str>) -> Result<Resolved, ApiError> {
        let requested = input.unwrap_or(".");
        let absolute = lexically_resolve(&self.root, Path::new(requested));
        if !contains(&self.root, &absolute) {
            return Err(ApiError::PathEscapedRoot);
        }
        let real = absolute
            .canonicalize()
            .map_err(|_| ApiError::PathNotFound(requested.to_owned()))?;
        if !contains(&self.real_root, &real) {
            return Err(ApiError::PathEscapedRoot);
        }
        Ok(Resolved { absolute, real })
    }
}

/// Resolves `input` against `root` the way `path.resolve` does, folding `.` and
/// `..` lexically and letting an absolute `input` replace the root entirely.
///
/// Folding `..` here rather than leaving it in the path is what makes stage 1 of
/// the sandbox a real check: `contains` on an unfolded `root/../etc` would be
/// satisfied by the literal prefix while the path still points outside.
fn lexically_resolve(root: &Path, input: &Path) -> PathBuf {
    let mut resolved = if input.is_absolute() {
        PathBuf::new()
    } else {
        root.to_path_buf()
    };
    for component in input.components() {
        match component {
            Component::Prefix(prefix) => {
                resolved = PathBuf::from(prefix.as_os_str());
            }
            Component::RootDir => {
                let prefix = resolved
                    .components()
                    .next()
                    .filter(|first| matches!(first, Component::Prefix(_)))
                    .map(|first| PathBuf::from(first.as_os_str()))
                    .unwrap_or_default();
                resolved = prefix;
                resolved.push(Component::RootDir.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => resolved.push(part),
        }
    }
    resolved
}

/// Whether `child` is `parent` or lives beneath it.
///
/// Ported from `packages/core/src/fs-util.ts:270-273`. Both sides must already be
/// lexically normalized; [`lexically_resolve`] and `canonicalize` are the two
/// callers that guarantee that.
fn contains(parent: &Path, child: &Path) -> bool {
    if child == parent {
        return true;
    }
    child.starts_with(parent)
}

/// `GET /api/fs/read/{*path}` — the file's bytes, with a guessed content type.
///
/// The response is deliberately **not** wrapped in the `{location, data}`
/// envelope: upstream declares this operation's success as raw
/// `Uint8Array` (`protocol/src/groups/fs.ts:22-24`) and answers with the file
/// itself.
///
/// # Errors
/// Returns [`ApiError::PathEscapedRoot`] for a path that leaves the session
/// directory, and [`ApiError::PathNotFound`] for a missing target or one that is
/// not a regular file.
pub async fn read(
    State(state): State<ApiState>,
    PathParam(path): PathParam<String>,
) -> Result<Response, ApiError> {
    let sandbox = Sandbox::open(state.directory())?;
    let target = sandbox.resolve(Some(&path))?;
    let metadata = std::fs::symlink_metadata(&target.real)
        .map_err(|_| ApiError::PathNotFound(path.clone()))?;
    if !metadata.is_file() {
        return Err(ApiError::PathNotFound(path));
    }
    let bytes = std::fs::read(&target.real).map_err(|_| ApiError::PathNotFound(path.clone()))?;
    let mime = mime_type(&target.real);
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_str(mime)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        )],
        bytes,
    )
        .into_response())
}

/// `GET /api/fs/list` — the direct children of one directory.
///
/// Directories sort before files and each group sorts by path, which is
/// `filesystem.ts:106` verbatim; a caller rendering a tree depends on it.
///
/// # Errors
/// Returns [`ApiError::PathEscapedRoot`] for an escaping path and
/// [`ApiError::PathNotFound`] when the target is missing or is not a directory.
pub async fn list(
    State(state): State<ApiState>,
    Query(input): Query<ListQuery>,
) -> Result<LocationEnvelope<Vec<Entry>>, ApiError> {
    let sandbox = Sandbox::open(state.directory())?;
    let target = sandbox.resolve(input.path.as_deref())?;
    let metadata = std::fs::metadata(&target.real).map_err(|_| {
        ApiError::PathNotFound(input.path.clone().unwrap_or_else(|| ".".to_owned()))
    })?;
    if !metadata.is_dir() {
        return Err(ApiError::PathNotFound(
            input.path.unwrap_or_else(|| ".".to_owned()),
        ));
    }
    let mut entries = Vec::new();
    let reader = std::fs::read_dir(&target.real).map_err(|_| ApiError::FilesystemUnavailable)?;
    for item in reader {
        let item = item.map_err(|_| ApiError::FilesystemUnavailable)?;
        let kind = match item.file_type() {
            Ok(kind) if kind.is_dir() => EntryKind::Directory,
            Ok(kind) if kind.is_file() => EntryKind::File,
            // Upstream drops everything that is neither, symlinks included
            // (`filesystem.ts:95`).
            Ok(_) | Err(_) => continue,
        };
        let absolute = target.absolute.join(item.file_name());
        let Some(relative) = relative_to(&sandbox.root, &absolute) else {
            continue;
        };
        entries.push(Entry {
            path: with_directory_suffix(relative, kind),
            kind,
        });
    }
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| locale_compare(&left.path, &right.path))
    });
    Ok(state.envelope(entries))
}

/// `GET /api/fs/find` — a bounded fuzzy search rooted at the session directory.
///
/// # Ranking is shape parity, not byte parity
///
/// Upstream ranks with `fff` when the native module loads and `fuzzysort`
/// otherwise (`filesystem/search.ts:101-230`) — two different scorers, so
/// upstream does not agree with itself across hosts. This port matches on
/// subsequence and orders by match tightness, then path length, then path, which
/// is deterministic and close, but it is **not** claimed to reproduce either
/// scorer's exact order. The differential therefore compares the operation's
/// rejected-request body rather than asserting a ranking neither upstream build
/// shares.
///
/// # Errors
/// Returns [`ApiError::MissingQueryKey`] when `query` is absent and
/// [`ApiError::InvalidQueryValue`] for a `limit` that is not a positive integer,
/// which are the two request shapes upstream's schema rejects with a 400.
pub async fn find(
    State(state): State<ApiState>,
    Query(input): Query<FindQuery>,
) -> Result<LocationEnvelope<Vec<Entry>>, ApiError> {
    let Some(needle) = input.query else {
        return Err(ApiError::MissingQueryKey("query"));
    };
    let limit = match input.limit {
        None => DEFAULT_FIND_LIMIT,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or(ApiError::InvalidQueryValue("limit"))?,
    };
    let sandbox = Sandbox::open(state.directory())?;
    let needle = needle.trim().to_owned();
    let mut scored = Vec::new();
    collect(&sandbox.root, &sandbox.root, &mut scored);
    let mut matched = scored
        .into_iter()
        .filter(|entry| match input.kind {
            Some(EntryFilter::File) => entry.kind == EntryKind::File,
            Some(EntryFilter::Directory) => entry.kind == EntryKind::Directory,
            None => true,
        })
        .filter_map(|entry| score(&entry.path, &needle).map(|score| (score, entry)))
        .collect::<Vec<_>>();
    matched.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.path.len().cmp(&right.1.path.len()))
            .then_with(|| locale_compare(&left.1.path, &right.1.path))
    });
    let data = matched
        .into_iter()
        .take(limit)
        .map(|(_, entry)| Entry {
            path: with_directory_suffix(entry.path, entry.kind),
            kind: entry.kind,
        })
        .collect();
    Ok(state.envelope(data))
}

/// One candidate before it is scored, holding the separator-free relative path.
struct Candidate {
    /// The path relative to the session directory, without a directory suffix.
    path: String,
    /// Whether it is a file or a directory.
    kind: EntryKind,
}

/// Walks `directory` depth-first, appending every file and directory beneath the
/// sandbox root and never following a symlink out of it.
fn collect(root: &Path, directory: &Path, out: &mut Vec<Candidate>) {
    let Ok(reader) = std::fs::read_dir(directory) else {
        return;
    };
    for item in reader.flatten() {
        // `file_type` on a `DirEntry` does not follow the link, so a symlink is
        // neither descended into nor reported. That is the search-side half of
        // the sandbox: the walk cannot leave the root at all.
        let Ok(kind) = item.file_type() else {
            continue;
        };
        let name = item.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() && FIND_EXCLUDED.contains(&name.as_ref()) {
            continue;
        }
        let absolute = directory.join(item.file_name());
        let Some(relative) = relative_to(root, &absolute) else {
            continue;
        };
        if kind.is_dir() {
            out.push(Candidate {
                path: relative,
                kind: EntryKind::Directory,
            });
            collect(root, &absolute, out);
        } else if kind.is_file() {
            out.push(Candidate {
                path: relative,
                kind: EntryKind::File,
            });
        }
    }
}

/// Scores a subsequence match, lower being tighter.
///
/// The score is the span of the haystack the needle's characters occupy, so a
/// contiguous hit beats a scattered one. `None` means the needle is not a
/// subsequence at all. An empty needle matches everything with the best score,
/// which is how upstream's scorers treat it too.
fn score(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    let mut best: Option<usize> = None;
    let haystack: Vec<char> = haystack.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    for start in 0..haystack.len() {
        if haystack[start] != needle[0] {
            continue;
        }
        let mut cursor = start;
        let mut index = 0;
        while cursor < haystack.len() && index < needle.len() {
            if haystack[cursor] == needle[index] {
                index += 1;
            }
            cursor += 1;
        }
        if index == needle.len() {
            let span = cursor - start;
            best = Some(best.map_or(span, |current: usize| current.min(span)));
        }
    }
    best
}

/// The `/`-joined path of `absolute` relative to `root`, or `None` when it is not
/// beneath it.
fn relative_to(root: &Path, absolute: &Path) -> Option<String> {
    let relative = absolute.strip_prefix(root).ok()?;
    let joined = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!joined.is_empty()).then_some(joined)
}

/// `String.prototype.localeCompare` for ASCII paths: case-insensitive first, with
/// case as the tie-break.
///
/// `filesystem.ts:106` sorts with `localeCompare`, which is **not** byte order, and
/// the difference is observable: in a directory holding `alpha.txt` and `Cargo.toml`
/// the oracle lists `alpha.txt` first, where a byte comparison puts `Cargo.toml`
/// first because `C` (0x43) precedes `a` (0x61). Caught by the live differential
/// against 1.18.12, not by reading the source.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let folded = left.to_lowercase().cmp(&right.to_lowercase());
    if folded == std::cmp::Ordering::Equal {
        left.cmp(right)
    } else {
        folded
    }
}

/// Appends the wire-format `/` separator to a directory path, as upstream does.
fn with_directory_suffix(path: String, kind: EntryKind) -> String {
    match kind {
        EntryKind::Directory => path + "/",
        EntryKind::File => path,
    }
}

/// A content type guessed from the extension, mirroring `fs-util.ts`'s table
/// closely enough for the browser to render what upstream renders and defaulting
/// to `application/octet-stream` for anything unknown.
fn mime_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "log" | "toml" | "lock" | "rs" | "py" | "sh" | "env" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" | "jsonc" => "application/json",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "yaml" | "yml" => "application/yaml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_resolution_folds_parent_components_before_containment_runs() {
        let root = Path::new("/work/repo");
        assert_eq!(
            lexically_resolve(root, Path::new("sub/../file.txt")),
            PathBuf::from("/work/repo/file.txt")
        );
        assert_eq!(
            lexically_resolve(root, Path::new("../outside.txt")),
            PathBuf::from("/work/outside.txt")
        );
        assert_eq!(
            lexically_resolve(root, Path::new("/etc/hostname")),
            PathBuf::from("/etc/hostname")
        );
    }

    #[test]
    fn containment_rejects_a_sibling_that_shares_a_name_prefix() {
        assert!(contains(Path::new("/work/repo"), Path::new("/work/repo")));
        assert!(contains(
            Path::new("/work/repo"),
            Path::new("/work/repo/src/lib.rs")
        ));
        assert!(!contains(
            Path::new("/work/repo"),
            Path::new("/work/repo-evil/secret")
        ));
        assert!(!contains(Path::new("/work/repo"), Path::new("/work")));
    }

    #[test]
    fn listing_order_is_case_insensitive_like_locale_compare() {
        let mut paths = vec!["Cargo.toml", "alpha.txt", "beta.txt", "Build.md"];
        paths.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(
            paths,
            vec!["alpha.txt", "beta.txt", "Build.md", "Cargo.toml"],
            "byte order would put every capital first; the oracle does not"
        );
    }

    #[test]
    fn subsequence_score_prefers_the_tighter_span() {
        assert_eq!(score("alpha.txt", "alpha"), Some(5));
        assert_eq!(score("a-l-p-h-a.txt", "alpha"), Some(9));
        assert_eq!(score("beta.txt", "alpha"), None);
        assert_eq!(score("anything", ""), Some(0));
    }

    #[test]
    fn wire_paths_use_forward_slashes_on_every_host() {
        let root = Path::new("root");
        let nested = root.join("nested").join("deep.txt");
        assert_eq!(
            relative_to(root, &nested).as_deref(),
            Some("nested/deep.txt")
        );
        assert_eq!(
            with_directory_suffix("nested".to_owned(), EntryKind::Directory),
            "nested/"
        );
    }
}
