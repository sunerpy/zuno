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

use std::collections::{BinaryHeap, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use axum::extract::{Path as PathParam, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::catalog::{LocationBody, LocationEnvelope};
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

/// How many levels below the session directory `fs/find` descends.
///
/// The walk is bounded rather than exhaustive because the response is bounded: no
/// caller can see past `limit` entries, and an unbounded descent on a deep tree
/// costs the whole process (see [`blocking`]) for results nobody receives.
const FIND_MAX_DEPTH: usize = 16;

/// How many directory entries one `fs/find` examines before it answers with the
/// best matches it has, for the same reason [`FIND_MAX_DEPTH`] exists.
const FIND_MAX_ENTRIES: usize = 20_000;

/// The largest file `fs/read` returns.
///
/// The body is buffered whole, so without a ceiling one request grows the process
/// by the size of whatever file it names.
const READ_MAX_BYTES: u64 = 32 * 1024 * 1024;

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

/// `fs/find`'s answer: the location envelope plus whether a ceiling stopped the walk.
///
/// [`FIND_MAX_DEPTH`] and [`FIND_MAX_ENTRIES`] make an empty `data` ambiguous — no such
/// file, or a walk that gave up before reaching it — and a search endpoint that cannot
/// tell a caller which one it means is reporting an absence it did not establish. The
/// flag is the difference, so a client can say "no matches here" or "narrow the search".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindEnvelope {
    /// Which directory, workspace and project the answer was computed for.
    pub location: LocationBody,
    /// The ranked matches, bounded by the request's `limit`.
    pub data: Vec<Entry>,
    /// `true` when the walk hit [`FIND_MAX_DEPTH`] or [`FIND_MAX_ENTRIES`], so a path
    /// missing from `data` may still exist.
    pub truncated: bool,
}

impl IntoResponse for FindEnvelope {
    fn into_response(self) -> Response {
        axum::Json(self).into_response()
    }
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
    let directory = state.directory().to_owned();
    let (bytes, mime) = blocking(move || read_contained_file(&directory, &path)).await?;
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

/// Resolves `path` inside the sandbox and reads it, refusing anything above
/// [`READ_MAX_BYTES`].
fn read_contained_file(directory: &str, path: &str) -> Result<(Vec<u8>, &'static str), ApiError> {
    let sandbox = Sandbox::open(directory)?;
    let target = sandbox.resolve(Some(path))?;
    let metadata = std::fs::symlink_metadata(&target.real)
        .map_err(|_| ApiError::PathNotFound(path.to_owned()))?;
    if !metadata.is_file() {
        return Err(ApiError::PathNotFound(path.to_owned()));
    }
    if metadata.len() > READ_MAX_BYTES {
        return Err(ApiError::FileTooLarge {
            path: path.to_owned(),
            limit: READ_MAX_BYTES,
        });
    }
    let file =
        std::fs::File::open(&target.real).map_err(|_| ApiError::PathNotFound(path.to_owned()))?;
    let mut bytes = Vec::new();
    // The ceiling is enforced again on the read itself: the size above came from a
    // separate `stat`, and a file that grew in between would otherwise still be
    // buffered whole.
    file.take(READ_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::PathNotFound(path.to_owned()))?;
    if bytes.len() as u64 > READ_MAX_BYTES {
        return Err(ApiError::FileTooLarge {
            path: path.to_owned(),
            limit: READ_MAX_BYTES,
        });
    }
    Ok((bytes, mime_type(&target.real)))
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
    let directory = state.directory().to_owned();
    let entries =
        blocking(move || list_contained_directory(&directory, input.path.as_deref())).await?;
    Ok(state.envelope(entries))
}

/// Lists the direct children of one contained directory.
fn list_contained_directory(directory: &str, path: Option<&str>) -> Result<Vec<Entry>, ApiError> {
    let sandbox = Sandbox::open(directory)?;
    let target = sandbox.resolve(path)?;
    let requested = || path.unwrap_or(".").to_owned();
    let metadata =
        std::fs::metadata(&target.real).map_err(|_| ApiError::PathNotFound(requested()))?;
    if !metadata.is_dir() {
        return Err(ApiError::PathNotFound(requested()));
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
    Ok(entries)
}

/// `GET /api/fs/find` — a bounded fuzzy search rooted at the session directory.
///
/// # Bounded means the walk, not only the answer
///
/// The walk descends at most [`FIND_MAX_DEPTH`] levels, examines at most
/// [`FIND_MAX_ENTRIES`] entries, and never holds more than `limit` candidates, so a
/// large or deep working tree costs a bounded amount of time and memory instead of
/// one allocation per file. Only the ranking of what it did examine is exact.
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
) -> Result<FindEnvelope, ApiError> {
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
    let directory = state.directory().to_owned();
    let needle = needle.trim().to_owned();
    let kind = input.kind;
    let found = blocking(move || search(&directory, &needle, kind, limit)).await?;
    let envelope = state.envelope(found.entries);
    Ok(FindEnvelope {
        location: envelope.location,
        data: envelope.data,
        truncated: found.truncated,
    })
}

/// Runs one filesystem operation off the reactor, inside the filesystem budget.
///
/// `zuno serve` polls this router on a single-threaded runtime
/// (`zuno serve`'s `Builder::new_current_thread`), so a synchronous walk or read
/// here freezes every SSE stream and every live turn in the process until the disk
/// answers. A slow network mount makes that freeze unbounded.
///
/// Moving the work off the reactor also removes the serialization the reactor was
/// providing, which is why every off-reactor handler shares one budget module: see
/// [`super::blocking`] for why the permit is held by the work rather than by the
/// caller, and what that makes the process-wide ceiling for [`READ_MAX_BYTES`]
/// buffers on an endpoint that is unauthenticated unless the operator sets
/// `ZUNO_SERVER_PASSWORD`.
async fn blocking<T, F>(work: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    super::blocking::run(super::blocking::Budget::Filesystem, work).await
}

/// Walks the sandbox breadth-first and returns the best `limit` matches.
///
/// Breadth-first is what makes [`FIND_MAX_ENTRIES`] useful: when the budget runs
/// out the entries already examined are the shallow ones, which is where the paths
/// a caller is searching for usually live.
fn search(
    directory: &str,
    needle: &str,
    filter: Option<EntryFilter>,
    limit: usize,
) -> Result<Found, ApiError> {
    let sandbox = Sandbox::open(directory)?;
    let mut best = BestMatches::new(limit);
    let mut examined = 0_usize;
    let mut truncated = false;
    let mut queue = VecDeque::from([(sandbox.root.clone(), 0_usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        // A subtree this walk cannot open — permission denied, a directory that went
        // away mid-walk — is unexamined, not empty. Reporting the search as complete
        // would let a caller read "not present" out of "not searchable", which is the
        // silent absence `truncated` exists to remove.
        let Ok(reader) = std::fs::read_dir(&directory) else {
            truncated = true;
            continue;
        };
        for item in reader {
            let Ok(item) = item else {
                truncated = true;
                continue;
            };
            if examined >= FIND_MAX_ENTRIES {
                return Ok(Found {
                    entries: best.into_ranked_entries(),
                    truncated: true,
                });
            }
            examined += 1;
            // `file_type` on a `DirEntry` does not follow the link, so a symlink is
            // neither descended into nor reported. That is the search-side half of
            // the sandbox: the walk cannot leave the root at all.
            let Ok(kind) = item.file_type() else {
                truncated = true;
                continue;
            };
            let name = item.file_name();
            if kind.is_dir() && FIND_EXCLUDED.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            let absolute = directory.join(&name);
            let Some(relative) = relative_to(&sandbox.root, &absolute) else {
                truncated = true;
                continue;
            };
            let kind = if kind.is_dir() {
                if depth + 1 < FIND_MAX_DEPTH {
                    queue.push_back((absolute, depth + 1));
                } else {
                    // A directory the walk refuses to enter is an answer this search
                    // cannot give; whatever is inside it is unexamined, not absent.
                    truncated = true;
                }
                EntryKind::Directory
            } else if kind.is_file() {
                EntryKind::File
            } else {
                continue;
            };
            let wanted = match filter {
                Some(EntryFilter::File) => kind == EntryKind::File,
                Some(EntryFilter::Directory) => kind == EntryKind::Directory,
                None => true,
            };
            if !wanted {
                continue;
            }
            if let Some(score) = score(&relative, needle) {
                best.consider(Ranked {
                    score,
                    path: relative,
                    kind,
                });
            }
        }
    }
    Ok(Found {
        entries: best.into_ranked_entries(),
        truncated,
    })
}

/// What one [`search`] examined: its best matches, and whether a ceiling cut it short.
struct Found {
    entries: Vec<Entry>,
    truncated: bool,
}

/// One scored candidate, ordered best-first.
struct Ranked {
    /// The match span, lower being tighter; see [`score`].
    score: usize,
    /// The path relative to the session directory, without a directory suffix.
    path: String,
    /// Whether it is a file or a directory.
    kind: EntryKind,
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| self.path.len().cmp(&other.path.len()))
            .then_with(|| locale_compare(&self.path, &other.path))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Ranked {}

/// The best `limit` candidates seen so far.
///
/// The walk keeps only what the response can carry, so the corpus never lands in
/// memory: a max-heap of `limit` entries gives the same answer the old
/// collect-everything-then-sort did, without holding one entry per file in the tree.
struct BestMatches {
    limit: usize,
    heap: BinaryHeap<Ranked>,
}

impl BestMatches {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::new(),
        }
    }

    fn consider(&mut self, candidate: Ranked) {
        if self.heap.len() < self.limit {
            self.heap.push(candidate);
            return;
        }
        if self
            .heap
            .peek()
            .is_some_and(|worst| candidate.cmp(worst).is_lt())
        {
            let _worst = self.heap.pop();
            self.heap.push(candidate);
        }
    }

    fn into_ranked_entries(self) -> Vec<Entry> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|ranked| Entry {
                path: with_directory_suffix(ranked.path, ranked.kind),
                kind: ranked.kind,
            })
            .collect()
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

    /// The reviewed input: whole-file reads whose callers hang up mid-read.
    ///
    /// `zuno serve` sets no `max_blocking_threads`, so tokio's default of 512 is how
    /// many reads could run at once the moment the work moved off the reactor, and
    /// `ZUNO_SERVER_PASSWORD` is optional, so an unauthenticated caller reaches it.
    ///
    /// A permit held in the *handler's* future does not bound that: a `spawn_blocking`
    /// closure whose `JoinHandle` is dropped runs to completion, so four callers that
    /// disconnect while their reads are in flight measured four free slots with four
    /// 32 MiB buffers still resident, and eight concurrent reads under a four-slot cap.
    /// The permit now lives inside the work, so a disconnect frees nothing until the
    /// read it queued has ended, and the process-wide ceiling really is
    /// `Budget::Filesystem` slots times [`READ_MAX_BYTES`].
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disconnected_caller_holds_its_filesystem_slot_until_its_read_ends() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        use super::super::blocking::Budget;

        const TOKIO_DEFAULT_BLOCKING_THREADS: usize = 512;
        let slots = Budget::Filesystem.size();
        assert!(
            slots < TOKIO_DEFAULT_BLOCKING_THREADS,
            "the bound has to be below the blocking pool to be a bound at all"
        );
        let running = Arc::new(AtomicUsize::new(0));
        let finish = Arc::new(AtomicBool::new(false));
        // Every slot taken by a caller that hangs up the moment its read is queued.
        for _ in 0..slots {
            let running = Arc::clone(&running);
            let finish = Arc::clone(&finish);
            let mut call = std::pin::pin!(blocking(move || {
                running.fetch_add(1, Ordering::SeqCst);
                // The read is held open by the test, with a real-clock ceiling so a
                // regression fails the suite instead of hanging it.
                let ceiling = Instant::now() + Duration::from_secs(30);
                while !finish.load(Ordering::SeqCst) && Instant::now() < ceiling {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            }));
            assert!(
                futures::poll!(&mut call).is_pending(),
                "the read is queued and its caller is waiting"
            );
            // The client hangs up here: the handler future goes out of scope at the end
            // of this iteration, while the `spawn_blocking` closure it queued keeps
            // running. (`drop` on the `Pin<&mut _>` would not do this — it drops the
            // pointer, not the future.)
        }
        let ceiling = Instant::now() + Duration::from_secs(30);
        while running.load(Ordering::SeqCst) < slots {
            assert!(
                Instant::now() < ceiling,
                "the disconnected reads never started"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(
            Budget::Filesystem.available(),
            0,
            "{slots} disconnected callers handed their slots back while their reads \
             still hold their buffers, so {TOKIO_DEFAULT_BLOCKING_THREADS} reads can \
             be resident under a {slots}-slot cap"
        );
        let mut queued = std::pin::pin!(blocking(|| Ok(READ_MAX_BYTES)));
        // A real-clock window, not a poll count: this closure returns immediately once it
        // runs, so a poll count could pass merely because the blocking pool had not been
        // scheduled yet.
        let window = Instant::now() + Duration::from_secs(1);
        while Instant::now() < window {
            assert!(
                futures::poll!(&mut queued).is_pending(),
                "a {READ_MAX_BYTES}-byte read started while every slot was still held \
                 by a disconnected caller's read"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        finish.store(true, Ordering::SeqCst);
        assert_eq!(
            queued
                .await
                .expect("the queued read runs once a slot frees"),
            READ_MAX_BYTES,
            "the bound must queue the work, not drop it"
        );
    }

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

    /// A subtree the walk cannot open must not be reported as a complete search.
    ///
    /// `GET /api/fs/find?query=secret` over a tree containing one directory this
    /// process may not read used to answer `truncated: false`, so a caller could not
    /// tell "no such path" from "one whole subtree was never examined". The same
    /// silent absence covers a directory removed mid-walk and a `file_type` that
    /// fails.
    ///
    /// The input is only constructible when the mode bits actually deny this process;
    /// a root test runner reads the directory anyway, and the test says so and stops
    /// rather than passing on an input it never built.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subtree_marks_the_search_truncated() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("a temporary sandbox root");
        std::fs::write(root.path().join("secret.txt"), b"visible")
            .expect("a matching file in the readable part of the tree");
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).expect("a subdirectory to lock");
        std::fs::write(locked.join("secret-inner.txt"), b"hidden")
            .expect("a matching file inside the locked subtree");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("clearing the subdirectory's mode bits");

        let denied = std::fs::read_dir(&locked).is_err();
        // Restore before any assertion so the temporary directory can be removed.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("restoring the subdirectory's mode bits");
        if !denied {
            // Running with a uid that ignores mode bits: the reviewed input does not
            // exist here, and asserting on it would report coverage that was not run.
            return;
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("clearing the subdirectory's mode bits again");

        let found = search(
            &root.path().to_string_lossy(),
            "secret",
            None,
            DEFAULT_FIND_LIMIT,
        );

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("restoring the subdirectory's mode bits");
        let found = found.expect("an unreadable subtree does not fail the whole search");
        assert!(
            found
                .entries
                .iter()
                .any(|entry| entry.path.ends_with("secret.txt")),
            "the readable part of the tree is still searched"
        );
        assert!(
            found.truncated,
            "a subtree the walk could not open leaves the search incomplete, so a \
             caller must not read `truncated: false` as `not present`"
        );
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
