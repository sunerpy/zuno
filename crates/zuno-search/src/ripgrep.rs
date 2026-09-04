//! Thin, cancellable adapter over the official `rg` executable.
//!
//! Zuno owns request validation and result shaping, but deliberately delegates
//! walking, ignore semantics, glob matching, regex execution, binary detection,
//! and filesystem traversal to ripgrep itself.

use crate::cancel::Cancellation;
use crate::error::SearchError;
use crate::types::{
    Entry, GlobRequest, GrepRequest, MAX_MATCH_TEXT, MAX_SUBMATCHES, Match, SearchResults,
    Submatch, normalize_relative, truncate_utf16,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// Oldest ripgrep major whose CLI and JSON stream Zuno accepts.
pub const MINIMUM_RIPGREP_MAJOR: u64 = 14;

/// Explicit exclusion appended after user globs so `.git` internals never surface.
const GIT_EXCLUDE_GLOB: &str = "!**/.git/**";
/// Maximum accepted size for one `rg --json` record.
///
/// A record over this is dropped unparsed, which is what bounds the transient cost of
/// one record: a `serde_json::Value` plus, for a base64 `bytes` field, its decoded
/// copy. The *aggregate* is already bounded by [`MAX_STDOUT_BYTES`], so this constant
/// does not decide how much decoding one search can cost — it only decides how large a
/// single record may be before Zuno stops answering for it. One mebibyte is above the
/// matching lines real minified, bundled and vendored files carry and two orders of
/// magnitude below the stream cap, so the ordinary bundle is answered completely
/// instead of being answered with a hole. Every drop sets `truncated`, so an answer
/// that lost a record never claims to be complete.
///
/// The cost this cap admits is transient because everything a record *retains* is
/// separately capped at [`MAX_MATCH_TEXT`] per value: `Match::text` and every
/// `Submatch::text`. Without the second of those, one 300 KiB line that is entirely one
/// match retained 300 KiB per record, so raising this constant raised retained bytes by
/// the same factor — 100 such files measured 30.7 MB of retained submatch text and a
/// ~94 MB peak RSS. Retention per record is now at most
/// `(1 + MAX_SUBMATCHES) * MAX_MATCH_TEXT` code units regardless of what this cap
/// admits.
const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// The most stdout Zuno buffers from one `rg` run before it abandons the search.
///
/// `rg`'s output is unbounded: a pattern matching most lines of a large tree, or
/// `--files` over a repository with a million paths, emits hundreds of megabytes.
/// Everything downstream — the record cap, the sort, and `limit` — runs on the whole
/// stream, because a stable order cannot be decided from a prefix of a parallel walk,
/// so this is the only place the size can be bounded. Going over it fails the one
/// search instead of letting one tool call exhaust the process every session shares.
const MAX_STDOUT_BYTES: usize = 64 * 1024 * 1024;
/// The most stderr Zuno keeps from one `rg` run.
///
/// stderr is only classified and reported, never parsed, so an over-long one is
/// truncated rather than failing the search.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// The buffer one stream reader fills per `read` call.
const READ_CHUNK_BYTES: usize = 32 * 1024;
/// Cancellation polling interval while a child process is live.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// How long one failed system discovery is reused before the next call re-probes.
///
/// A negative result must not last for the process lifetime: ripgrep is a backend
/// dependency of `glob` and `grep` only, and a user who installs it mid-session has
/// to get those tools working without restarting Zuno. It must not be re-probed on
/// literally every call either, or a model looping on `grep` with no `rg` installed
/// would spawn one `rg --version` per tool call. Five seconds is longer than the
/// burst of search calls one turn issues and far shorter than any human install, so
/// the recheck is effectively immediate for the user and negligible in process cost.
const DISCOVERY_RETRY_COOLDOWN: Duration = Duration::from_secs(5);

static SYSTEM_RIPGREP: DiscoveryCache = DiscoveryCache::new();

/// The process-wide result of resolving and version-checking `rg` on `PATH`.
///
/// A success is kept for the process lifetime because session remounts must not
/// spawn `rg --version` repeatedly. A failure is kept only for
/// [`DISCOVERY_RETRY_COOLDOWN`], which is what makes a missing `rg` recoverable
/// without a restart while still bounding the probe rate.
struct DiscoveryCache {
    state: Mutex<Option<CachedDiscovery>>,
}

/// What a previous probe concluded, and until when that conclusion is reused.
enum CachedDiscovery {
    /// Resolved and accepted; reused for the process lifetime.
    Ready(Discovery),
    /// Failed; reused until `retry_at`, then re-probed.
    Failed {
        failure: DiscoveryFailure,
        retry_at: Instant,
    },
}

impl DiscoveryCache {
    const fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Resolve through the cache, probing only when nothing usable is cached.
    ///
    /// `probe` runs while the lock is held so that concurrent first callers make one
    /// probe between them, which is the behaviour the previous `OnceLock` had. `now`
    /// is a parameter rather than read here so the cooldown is testable without
    /// sleeping, and `probe` is a parameter so the caching behaviour is testable
    /// without depending on the host's `rg`.
    fn resolve(
        &self,
        now: Instant,
        probe: &dyn Fn() -> Result<Discovery, DiscoveryFailure>,
    ) -> Result<Discovery, DiscoveryFailure> {
        // A panicking probe must not make ripgrep permanently unavailable, so the
        // poisoned guard is taken rather than propagated.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.as_ref() {
            Some(CachedDiscovery::Ready(discovery)) => return Ok(discovery.clone()),
            Some(CachedDiscovery::Failed { failure, retry_at }) if now < *retry_at => {
                return Err(failure.clone());
            }
            _ => {}
        }
        let probed = probe();
        *state = Some(match &probed {
            Ok(discovery) => CachedDiscovery::Ready(discovery.clone()),
            Err(failure) => CachedDiscovery::Failed {
                failure: failure.clone(),
                // A clock too near the end of its range to hold the deadline
                // re-probes immediately rather than panicking.
                retry_at: now.checked_add(DISCOVERY_RETRY_COOLDOWN).unwrap_or(now),
            },
        });
        probed
    }
}

/// Resolve the process-wide `rg`, honouring the discovery cache.
fn system_discovery() -> Result<Discovery, SearchError> {
    SYSTEM_RIPGREP
        .resolve(Instant::now(), &discover_system)
        .map_err(DiscoveryFailure::into_search_error)
}

#[derive(Debug, Clone)]
struct Discovery {
    program: PathBuf,
    version: String,
}

#[derive(Debug, Clone)]
enum DiscoveryFailure {
    Missing,
    Probe { program: PathBuf, message: String },
    Version { program: PathBuf, found: String },
}

impl DiscoveryFailure {
    fn into_search_error(self) -> SearchError {
        let message = match self {
            Self::Missing => format!(
                "ripgrep (`rg`) is required for glob and grep; install ripgrep {MINIMUM_RIPGREP_MAJOR} or newer and ensure `rg` is on PATH"
            ),
            Self::Probe { program, message } => format!(
                "failed to inspect ripgrep at {}: {message}",
                program.display()
            ),
            Self::Version { program, found } => format!(
                "ripgrep at {} reports unsupported version `{found}`; Zuno requires major version {MINIMUM_RIPGREP_MAJOR} or newer",
                program.display()
            ),
        };
        SearchError::Unavailable { message }
    }
}

/// A search engine backed exclusively by one official `rg` executable.
#[derive(Debug, Clone)]
pub struct Ripgrep {
    program: PathBuf,
    version: Option<String>,
    discovery: DiscoveryPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryPolicy {
    Explicit,
    DeferredSystem,
}

impl Ripgrep {
    /// Resolve and validate the process-wide `rg` on `PATH`.
    ///
    /// A successful discovery is cached because session remounts must not spawn
    /// `rg --version` repeatedly. Zuno's bootstrap process fixes the command
    /// environment before this value is first read. A failed discovery is cached
    /// only for a short cooldown, so ripgrep installed mid-session is picked up
    /// without restarting Zuno.
    pub fn discover() -> Result<Self, SearchError> {
        let discovery = system_discovery()?;
        Ok(Self {
            program: discovery.program,
            version: Some(discovery.version),
            discovery: DiscoveryPolicy::Explicit,
        })
    }

    /// Defer resolving and version-checking the process-wide `rg` until a search runs.
    ///
    /// This keeps ripgrep an optional dependency of the `glob` and `grep` tools
    /// instead of an unrelated startup requirement. The first invocation still
    /// uses the same cached discovery and minimum-version validation as
    /// [`Self::discover`], including its recheck of an earlier failure.
    #[must_use]
    pub fn deferred_system() -> Self {
        Self {
            program: PathBuf::from("rg"),
            version: None,
            discovery: DiscoveryPolicy::DeferredSystem,
        }
    }

    /// Use an explicit program without consulting `PATH`.
    ///
    /// Intended for tests, packaged companion binaries, and embedders that already
    /// validated the executable.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            version: None,
            discovery: DiscoveryPolicy::Explicit,
        }
    }

    /// The executable this engine invokes.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Version reported during system discovery, when available.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// List files matching one glob.
    ///
    /// Blocks the calling thread for as long as `rg` runs, which is seconds on a large
    /// tree. An async caller must hand this to a blocking thread rather than call it on
    /// a reactor: a single-threaded runtime otherwise stops making progress, including
    /// on the task that would raise `cancel`.
    pub fn glob(
        &self,
        request: &GlobRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Entry>, SearchError> {
        validate_root(&request.cwd)?;
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }

        // `--no-messages` is what separates "`rg` refused this invocation" from "the
        // walk could not enter every directory", and it is `rg`'s own separation of the
        // two rather than Zuno's: it suppresses exactly the diagnostics about opening
        // and reading paths (permission denied, dangling symlink, filesystem loop) and
        // deliberately keeps the ones about pattern syntax. Without it, `rg --files`
        // writes nothing on stdout when the glob matches nothing, so one unreadable
        // directory anywhere under the root made the commonest `glob` outcome
        // indistinguishable from a rejected pattern. Matching on the wording instead
        // would be a locale- and platform-dependent guess at a rendered message.
        //
        // `--null` is what makes the output parseable at all. A path is an identifier
        // the model feeds straight back into `read` and `edit`, and a newline is a
        // legal byte in a filename on every platform Zuno supports, so splitting
        // `--files` output on newlines turned one real path into two paths that were
        // never on disk. NUL is the one byte no path may contain, and `rg` has
        // terminated `--files` output with it since long before the minimum accepted
        // major.
        let mut args = vec![
            "--no-config".to_owned(),
            "--files".to_owned(),
            "--null".to_owned(),
            "--no-messages".to_owned(),
        ];
        if request.hidden {
            args.push("--hidden".to_owned());
        }
        if request.follow {
            args.push("--follow".to_owned());
        }
        args.push(format!("--glob={}", request.pattern));
        args.push(format!("--glob={GIT_EXCLUDE_GLOB}"));
        args.push(".".to_owned());

        let output = self.run(
            &request.cwd,
            &args,
            cancel,
            &[Pattern::Glob(&request.pattern)],
        )?;
        // Bytes, not text, and only the chunks that are valid UTF-8. `rg` emits a path
        // exactly as the operating system gave it, so a latin-1 name from an old
        // tarball is not valid UTF-8; `from_utf8_lossy` would answer `bad\u{fffd}.ts`,
        // which is a `RelativePath` naming no file on disk. There is no lossy
        // rendering of a byte string that still names the same file, so such a path is
        // dropped and the drop is what `truncated` reports — the same rule `grep`
        // applies to a match record whose path cannot be named.
        let mut items = Vec::new();
        let mut unnameable = 0usize;
        for chunk in output.stdout.split(|byte| *byte == b'\0') {
            if chunk.is_empty() {
                continue;
            }
            match std::str::from_utf8(chunk) {
                Ok(path) => items.push(Entry::file(normalize_relative(path))),
                Err(_) => unnameable = unnameable.saturating_add(1),
            }
        }
        items.sort();
        let truncated = items.len() > request.limit || unnameable > 0;
        items.truncate(request.limit);
        Ok(SearchResults { items, truncated })
    }

    /// Search file contents for one regex.
    ///
    /// Blocks the calling thread for as long as `rg` runs, which is seconds on a large
    /// tree. An async caller must hand this to a blocking thread rather than call it on
    /// a reactor: a single-threaded runtime otherwise stops making progress, including
    /// on the task that would raise `cancel`.
    pub fn grep(
        &self,
        request: &GrepRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Match>, SearchError> {
        validate_root(&request.cwd)?;
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }

        let mut args = vec![
            "--no-config".to_owned(),
            "--json".to_owned(),
            "--hidden".to_owned(),
            "--no-messages".to_owned(),
        ];
        // `rg` writes every match before Zuno can sort or truncate, so a per-file cap
        // is what keeps a broad pattern over a large tree from streaming an unbounded
        // number of records. One more than `limit` is the largest-answer-preserving
        // cap: the sorted result keeps at most `limit` lines from any single file, and
        // the extra match is what still proves the result was truncated.
        args.push(format!("--max-count={}", request.limit.saturating_add(1)));
        if let Some(include) = &request.include {
            args.push(format!("--glob={include}"));
        }
        args.push(format!("--glob={GIT_EXCLUDE_GLOB}"));
        args.push("--".to_owned());
        args.push(request.pattern.clone());
        args.push(request.file.clone().unwrap_or_else(|| ".".to_owned()));

        // The include is a glob, so it is handed to the classifier as one: a rejected
        // `include` is the model's to correct exactly like a rejected regex.
        let mut patterns = vec![Pattern::Regex(&request.pattern)];
        if let Some(include) = &request.include {
            patterns.push(Pattern::Glob(include));
        }

        let output = self.run(&request.cwd, &args, cancel, &patterns)?;
        // `rg --json` is a UTF-8 contract: every byte string it cannot render as UTF-8
        // arrives base64-encoded in a `bytes` field, so an invalid byte here means a
        // corrupt stream, and decoding lossily keeps the intact records around it
        // parseable.
        let stdout = into_lossy_string(output.stdout);
        let mut items = Vec::new();
        let mut undecodable: Option<String> = None;
        let mut unnameable = 0usize;
        // Only read when it can change the outcome, so the ordinary walk pays nothing
        // for a second parse of every non-match record.
        let count_searches = request.file.is_some() && output.tolerated_error;
        let mut searched: Option<u64> = None;
        for line in stdout.lines() {
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_RECORD_BYTES {
                // Not parsed at all: the cap bounds decode work, and only
                // [`MAX_MATCH_TEXT`] of such a line would ever be shown anyway.
                undecodable.get_or_insert_with(|| {
                    format!("JSON record exceeded {MAX_RECORD_BYTES} bytes")
                });
                continue;
            }
            match parse_match(line) {
                Ok(Record::Match(found)) => items.push(found),
                // A match whose path is not valid UTF-8 is a *different fact* from a
                // record Zuno could not read: the pattern demonstrably matched, so
                // this record still answers "is the pattern in the tree". It is
                // therefore deliberately kept out of `undecodable`, which the
                // loud-when-empty rule below reads — folding it in there turned a tree
                // whose matches all live in files with legacy names into a hard,
                // non-correctable tool failure with no result and no advice.
                Ok(Record::Unnameable) => unnameable = unnameable.saturating_add(1),
                Ok(Record::Other) => {
                    if count_searches {
                        searched = summary_searches(line).or(searched);
                    }
                }
                // Destructured rather than rendered so the reason is reported once,
                // and so a failure this decoder does not produce today would still
                // reach the caller instead of being counted as a dropped record.
                Err(SearchError::Ripgrep { message }) => {
                    undecodable.get_or_insert(message);
                }
                Err(other) => return Err(other),
            }
        }
        // `--no-messages` suppresses the diagnostic for a path `rg` could not open, and
        // a `--json` run emits its summary either way, so a request that named one file
        // cannot be told apart from a file that simply has no match by output alone.
        // The summary's own count can: exiting 2 having searched no file at all means
        // the named path was never read, and calling that "no matches" would report a
        // pattern absent from a file nobody opened. A tree walk (`file` is `None`) is
        // deliberately not judged this way — an otherwise empty tree beside one
        // unreadable directory is a legitimate empty result, not a failure.
        if let Some(file) = &request.file
            && output.tolerated_error
            && searched == Some(0)
        {
            return Err(SearchError::Rejected {
                message: format!("ripgrep searched no file for the requested path {file}"),
            });
        }
        // A record Zuno cannot decode costs that one line rather than the search: the
        // cause is a property of the tree, not of the query, so failing would leave
        // the model no call that works. A run where *nothing* decoded is a different
        // thing and stays loud, because reporting it as an empty result would tell the
        // model the pattern is absent from the tree.
        //
        // Only a record Zuno could not *read* reaches this rule. A match it read and
        // could not *name* does not: that record settles the question the model asked,
        // and a tree can hold nothing but files with legacy names, so failing there
        // would refuse a search over an ordinary property of the filesystem.
        if items.is_empty()
            && let Some(message) = undecodable
        {
            return Err(SearchError::Ripgrep { message });
        }
        items.sort_by(|left, right| {
            left.entry
                .path
                .cmp(&right.entry.path)
                .then(left.line.cmp(&right.line))
                .then(left.offset.cmp(&right.offset))
        });
        // A dropped record is still proof that a further match exists, so the result
        // may not present itself as complete. `items.len() > limit` cannot express
        // that: the drop happens before the count, and the per-file `--max-count` means
        // `rg` never emitted the matches that would have taken the dropped record's
        // place — so a file holding many matches can answer with fewer than `limit`
        // items and still have more to give.
        let truncated = items.len() > request.limit || undecodable.is_some() || unnameable > 0;
        items.truncate(request.limit);
        Ok(SearchResults { items, truncated })
    }

    fn run(
        &self,
        cwd: &Path,
        args: &[String],
        cancel: &dyn Cancellation,
        patterns: &[Pattern<'_>],
    ) -> Result<RipgrepOutput, SearchError> {
        let program = self.execution_program()?;
        let mut child = Command::new(&program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| SearchError::Spawn {
                program: program.clone(),
                source,
            })?;
        let stdout = child
            .stdout
            .take()
            .expect("piped stdout exists immediately after spawn");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr exists immediately after spawn");
        let overflowed = Arc::new(AtomicBool::new(false));
        let stdout_reader = read_stream(stdout, MAX_STDOUT_BYTES, Some(Arc::clone(&overflowed)));
        let stderr_reader = read_stream(stderr, MAX_STDERR_BYTES, None);

        let status = wait_for_exit(&mut child, cancel, &overflowed)?;
        let stdout = join_stream(stdout_reader, "stdout")?;
        let stderr = join_stream(stderr_reader, "stderr")?;
        // Re-read once both readers are done: a child that exits before its reader
        // reaches the byte that went over the cap leaves supervision nothing to see,
        // and parsing the truncated tail would silently drop matches.
        if overflowed.load(Ordering::SeqCst) {
            return Err(stdout_limit_exceeded());
        }
        let code = status.code();
        classify_status(code, &stderr, patterns, !stdout.is_empty())?;
        Ok(RipgrepOutput {
            stdout,
            tolerated_error: code == Some(2),
        })
    }

    fn execution_program(&self) -> Result<PathBuf, SearchError> {
        match self.discovery {
            DiscoveryPolicy::Explicit => Ok(self.program.clone()),
            DiscoveryPolicy::DeferredSystem => {
                system_discovery().map(|discovery| discovery.program)
            }
        }
    }
}

fn discover_system() -> Result<Discovery, DiscoveryFailure> {
    let program = which::which("rg").map_err(|_| DiscoveryFailure::Missing)?;
    let output = Command::new(&program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| DiscoveryFailure::Probe {
            program: program.clone(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(DiscoveryFailure::Probe {
            program,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let Some(version) = accepted_version(&first) else {
        return Err(DiscoveryFailure::Version {
            program,
            found: first,
        });
    };
    let version = version.to_owned();
    Ok(Discovery { program, version })
}

/// The version a `rg --version` first line reports, when Zuno accepts it.
///
/// Split out from [`discover_system`] so the gate is testable against real output
/// shapes without an old binary on the host: the probe spawns a process, this does
/// not. Anything that is not `ripgrep <major>[.…]` with a major of at least
/// [`MINIMUM_RIPGREP_MAJOR`] is rejected, including a build-metadata suffix's own
/// tokens, which are dropped rather than parsed.
fn accepted_version(first_line: &str) -> Option<&str> {
    let version = first_line
        .trim()
        .strip_prefix("ripgrep ")?
        .split_whitespace()
        .next()?;
    let major = version.split('.').next()?.parse::<u64>().ok()?;
    (major >= MINIMUM_RIPGREP_MAJOR).then_some(version)
}

fn validate_root(root: &Path) -> Result<(), SearchError> {
    let metadata = std::fs::metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SearchError::RootMissing {
                root: root.to_path_buf(),
            }
        } else {
            SearchError::Ripgrep {
                message: format!("could not inspect search root {}: {error}", root.display()),
            }
        }
    })?;
    if !metadata.is_dir() {
        return Err(SearchError::RootNotDirectory {
            root: root.to_path_buf(),
        });
    }
    Ok(())
}

/// Reads one child stream in a thread, keeping at most `cap` bytes of it.
///
/// Deliberately not `read_to_end`: `rg`'s output is unbounded (see
/// [`MAX_STDOUT_BYTES`]). Reading continues past the cap so the child never blocks
/// writing into a full pipe — that would deadlock the wait below — and `overflowed`,
/// when the caller wants to hear about it, tells supervision to kill the child rather
/// than let it finish producing output that is already being discarded.
fn read_stream<R>(
    mut reader: R,
    cap: usize,
    overflowed: Option<Arc<AtomicBool>>,
) -> thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK_BYTES];
        let mut seen = 0usize;
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            seen = seen.saturating_add(read);
            if kept.len() < cap {
                let keep = read.min(cap - kept.len());
                kept.extend_from_slice(&chunk[..keep]);
            }
            if seen > cap
                && let Some(flag) = &overflowed
            {
                flag.store(true, Ordering::SeqCst);
            }
        }
        Ok(kept)
    })
}

/// Renders a captured stream as text without copying the valid-UTF-8 case.
///
/// The buffer reaches [`MAX_STDOUT_BYTES`], so `from_utf8_lossy(..).into_owned()`
/// would double the peak. Invalid UTF-8 only arrives on a corrupt stream — `rg`
/// base64-encodes every byte sequence it cannot render as UTF-8 — and is still
/// decoded lossily so the intact records around it parse.
fn into_lossy_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// The failure a run that outgrew [`MAX_STDOUT_BYTES`] reports.
///
/// Phrased for the model, because narrowing the call is the only thing that changes
/// the outcome.
fn stdout_limit_exceeded() -> SearchError {
    SearchError::TooBroad {
        message: format!(
            "ripgrep produced more than {} MiB of output; narrow the search with a more specific pattern, path, or include filter",
            MAX_STDOUT_BYTES / (1024 * 1024)
        ),
    }
}

fn join_stream(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, SearchError> {
    reader
        .join()
        .map_err(|_| SearchError::Ripgrep {
            message: format!("{name} reader panicked"),
        })?
        .map_err(|error| SearchError::Ripgrep {
            message: format!("reading {name} failed: {error}"),
        })
}

fn wait_for_exit(
    child: &mut Child,
    cancel: &dyn Cancellation,
    overflowed: &AtomicBool,
) -> Result<ExitStatus, SearchError> {
    loop {
        if cancel.is_cancelled() {
            terminate(child);
            return Err(SearchError::Cancelled);
        }
        // Killing on overflow is what bounds the *time* the run costs: the reader
        // keeps draining, so otherwise a search whose output is already being thrown
        // away would still run to completion.
        if overflowed.load(Ordering::SeqCst) {
            terminate(child);
            return Err(stdout_limit_exceeded());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(CANCEL_POLL),
            Err(error) => {
                terminate(child);
                return Err(SearchError::Ripgrep {
                    message: format!("waiting for process failed: {error}"),
                });
            }
        }
    }
}

/// Stops a child Zuno has decided not to wait for, reaping it so no zombie is left.
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Debug, Clone, Copy)]
enum Pattern<'a> {
    Glob(&'a str),
    Regex(&'a str),
}

/// Turns one `rg` exit into a typed failure, or accepts it.
///
/// `produced_output` is whether the run wrote anything at all to stdout, which is how
/// a rejected invocation is told apart from a partial walk: `rg` exits 2 for both.
fn classify_status(
    code: Option<i32>,
    stderr: &[u8],
    patterns: &[Pattern<'_>],
    produced_output: bool,
) -> Result<(), SearchError> {
    let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
    if code == Some(2) {
        if let Some(error) = classify_pattern(&stderr, patterns) {
            return Err(error);
        }
        // Exit 2 with a diagnostic and an empty stdout means `rg` rejected the
        // invocation before it searched anything. Returning `Ok` here rendered that
        // as "no matches", which tells the model its pattern is absent from the tree
        // when in fact its filter was thrown out. A partial walk (some output plus a
        // diagnostic) still succeeds with what it did reach.
        // Typed as `Rejected` rather than as an opaque backend failure: nothing in the
        // invocation but the call's own pattern, include glob and path is variable, so
        // a refusal is the model's to correct, and `map_search_error` reads exactly
        // that predicate when it chooses between `InvalidArgs` and the deliberately
        // non-retryable `Failed`.
        if !produced_output && !stderr.is_empty() {
            return Err(SearchError::Rejected { message: stderr });
        }
    }
    // ripgrep: 0 = match, 1 = no match, 2 = partial filesystem/glob error.
    if !matches!(code, Some(0..=2)) {
        return Err(SearchError::Ripgrep {
            message: if stderr.is_empty() {
                format!("process exited with code {code:?}")
            } else {
                stderr
            },
        });
    }
    Ok(())
}

/// The typed failure one of `patterns` explains, if `stderr` names it.
///
/// Every pattern the invocation carried is considered, not only the one the caller
/// thinks of as primary: `grep` passes a regex *and*, when the call had one, an
/// `include` glob, and either can be the thing `rg` rejected.
fn classify_pattern(stderr: &str, patterns: &[Pattern<'_>]) -> Option<SearchError> {
    // Structural attribution first. `rg` quotes the glob it rejected, and the glob is
    // caller-supplied text that `rg` echoes onto stderr, so every substring rule below
    // is satisfiable by the caller's own `include`: `include = "[regex parse error"`
    // used to come back as `InvalidPattern { pattern: "needle" }` for a call whose
    // regex was fine. It settles the attribution before any wording rule runs. Not a
    // proof of authorship, only a stronger signal than wording: `rg` echoes the regex
    // too, so a regex whose own text contains `error parsing glob '<the include>'` still
    // reaches this rule first. Contrived, and the failure is a misnamed pattern rather
    // than a wrong answer, so it is left as the better of two heuristics.
    let quoted = patterns.iter().copied().find_map(|pattern| match pattern {
        Pattern::Glob(glob) if stderr.contains(&format!("error parsing glob '{glob}'")) => {
            Some(SearchError::InvalidGlob {
                pattern: glob.to_owned(),
                message: stderr.to_owned(),
            })
        }
        _ => None,
    });
    if quoted.is_some() {
        return quoted;
    }
    // Otherwise the wording rules decide. Regex is tried before glob because `rg`'s own
    // regex diagnostic ("regex parse error") also contains the words a glob diagnostic
    // uses ("unclosed character class"), so ordering it the other way would blame a
    // perfectly good `include` for a broken pattern.
    patterns.iter().copied().find_map(|pattern| match pattern {
        Pattern::Regex(pattern) if is_invalid_regex(stderr) => Some(SearchError::InvalidPattern {
            pattern: pattern.to_owned(),
            message: stderr.to_owned(),
        }),
        Pattern::Glob(pattern) if is_invalid_glob(stderr) => Some(SearchError::InvalidGlob {
            pattern: pattern.to_owned(),
            message: stderr.to_owned(),
        }),
        _ => None,
    })
}

fn is_invalid_regex(stderr: &str) -> bool {
    stderr.contains("regex parse error")
        || stderr.contains("error parsing regex")
        // `rg: compiled regex exceeds size limit of 104857600` parses cleanly and then
        // fails to compile, so neither wording above catches it. The conjunction is
        // deliberate: "exceeds size limit" alone also describes limits that have
        // nothing to do with the pattern, and blaming one of those on the regex would
        // send the model round a loop editing a pattern that was fine.
        || (stderr.contains("regex") && stderr.contains("exceeds size limit"))
}

fn is_invalid_glob(stderr: &str) -> bool {
    stderr.contains("invalid glob")
        || stderr.contains("error parsing glob")
        || stderr.contains("unclosed character class")
}

struct RipgrepOutput {
    /// Exactly the bytes `rg` wrote, up to [`MAX_STDOUT_BYTES`].
    ///
    /// Deliberately not decoded here: `grep`'s stream is a UTF-8 JSON contract and
    /// `glob`'s is a list of operating-system paths, and a path is an identifier that
    /// has no lossy rendering. Each entry point decides, so one of them cannot force a
    /// replacement character into the other's output.
    stdout: Vec<u8>,
    /// Whether `rg` exited 2 — an error it reported and [`classify_status`] accepted.
    ///
    /// Kept because "accepted" only means the run was not *wholly* refused; a caller
    /// that asked about one named path still has to decide whether that path was among
    /// the things `rg` failed on.
    tolerated_error: bool,
}

/// What one `rg --json` line contributed.
///
/// The three outcomes are kept apart because the caller owes each a different answer:
/// a match is a result, a non-match record may still carry the summary's counts, and a
/// match whose path cannot be named is a result that exists and cannot be listed. Only
/// a line that violates the JSON contract is an `Err`, and the caller drops that one
/// record rather than the search.
#[derive(Debug)]
enum Record {
    /// A match Zuno can both read and name.
    Match(Match),
    /// A `begin`, `end`, `context` or `summary` record.
    Other,
    /// A match whose path is not valid UTF-8, so it has no `RelativePath` form.
    ///
    /// Distinct from an `Err` on purpose: the record proved the pattern is present, so
    /// it must not be able to make an otherwise successful search fail.
    Unnameable,
}

/// Decode one `rg --json` line and ignore non-match records.
///
/// `rg` reports a path, a line, or a match as `text` when it is valid UTF-8 and as
/// base64 `bytes` when it is not, so both forms are accepted: a latin-1 comment or a
/// stray byte in one matched line is a property of the repository, and the match is
/// still the answer the model asked for. Only a record that violates the JSON contract
/// outright is rejected, and the caller drops that record rather than the search.
fn parse_match(line: &str) -> Result<Record, SearchError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| SearchError::Ripgrep {
            message: format!("invalid JSON output: {error}"),
        })?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("match") {
        return Ok(Record::Other);
    }
    let data = value.get("data").ok_or_else(|| SearchError::Ripgrep {
        message: "match record had no data".to_owned(),
    })?;
    let path_data = data.get("path").ok_or_else(|| SearchError::Ripgrep {
        message: "match record had no path".to_owned(),
    })?;
    // The `text` form only, deliberately. `rg` reports a path that is not valid UTF-8
    // as base64 `bytes`, and rendering those lossily hands the model a path containing
    // U+FFFD as a `RelativePath` — an identifier it is expected to feed straight back
    // into `read` or `edit`, and which names no file on disk. Reported as `Unnameable`
    // rather than as an error: the record is a real match, so the caller drops it and
    // sets `truncated`, and it can never turn a search that found something into a
    // failure.
    let Some(path) = decode_text(path_data) else {
        return Ok(Record::Unnameable);
    };
    let text = decode_data(data.get("lines")).ok_or_else(|| SearchError::Ripgrep {
        message: "match record had no line text".to_owned(),
    })?;
    let line_number = data
        .get("line_number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "match record had no line number".to_owned(),
        })?;
    let offset = data
        .get("absolute_offset")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "match record had no absolute offset".to_owned(),
        })?;
    let submatches = data
        .get("submatches")
        .and_then(serde_json::Value::as_array)
        .map(|found| {
            found
                .iter()
                .take(MAX_SUBMATCHES)
                .filter_map(|entry| {
                    Some(Submatch {
                        // Capped like the line it is quoted from. `start` and `end` are
                        // offsets into `Match::text`, which is already cut at
                        // `MAX_MATCH_TEXT`, so a submatch longer than that cap could
                        // never be shown whole anyway — and leaving it uncapped meant
                        // one 300 KiB single-match line retained 300 KiB per record,
                        // scaling with the record cap instead of with what is readable.
                        text: truncate_utf16(&decode_data(entry.get("match"))?, MAX_MATCH_TEXT),
                        start: usize::try_from(
                            entry.get("start").and_then(serde_json::Value::as_u64)?,
                        )
                        .ok()?,
                        end: usize::try_from(entry.get("end").and_then(serde_json::Value::as_u64)?)
                            .ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Record::Match(Match {
        entry: Entry::file(normalize_relative(path)),
        line: line_number,
        offset,
        text: truncate_utf16(&text, MAX_MATCH_TEXT),
        submatches,
    }))
}

/// The `text` form of one `rg --json` "arbitrary data" object, and only that form.
///
/// Used where the value is an *identifier* rather than display text — a path the model
/// feeds back into another tool — because there is no lossy rendering of a byte string
/// that still names the same file.
fn decode_text(data: &serde_json::Value) -> Option<&str> {
    data.get("text").and_then(serde_json::Value::as_str)
}

/// The number of files `rg --json`'s summary record reports having searched.
///
/// `None` for every other record, so a caller can fold it over the stream and be left
/// with the summary's count or with nothing.
fn summary_searches(line: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("summary") {
        return None;
    }
    value
        .get("data")?
        .get("stats")?
        .get("searches")
        .and_then(serde_json::Value::as_u64)
}

/// The text of one `rg --json` "arbitrary data" object, decoding the `bytes` form.
///
/// A value that is not valid UTF-8 arrives base64-encoded and is rendered lossily,
/// which keeps the match reportable. Used for the matched line and its submatches,
/// which are content: the replacement characters mean a submatch offset into such a
/// line is `rg`'s offset into the raw bytes, not into this rendering, and the line
/// number is unaffected. Deliberately *not* used for the path — see [`decode_text`].
fn decode_data(data: Option<&serde_json::Value>) -> Option<String> {
    let data = data?;
    if let Some(text) = data.get("text").and_then(serde_json::Value::as_str) {
        return Some(text.to_owned());
    }
    let encoded = data.get("bytes").and_then(serde_json::Value::as_str)?;
    Some(String::from_utf8_lossy(&decode_base64(encoded)?).into_owned())
}

/// Decodes one standard-alphabet, padded base64 value.
///
/// Hand-rolled rather than pulled in as a dependency because `rg`'s `bytes` fields are
/// the only encoded values Zuno reads, and a malformed one must be a `None` this
/// caller can drop rather than a panic.
fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let body = encoded.trim_end_matches('=');
    let padding = encoded.len() - body.len();
    // Standard base64 comes in four-character groups, carries at most two padding
    // characters, and never contains `=` inside the body.
    if padding > 2 || body.contains('=') || !encoded.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in body.bytes() {
        accumulator = (accumulator << 6) | u32::from(base64_sextet(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((accumulator >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// The six bits one base64 character stands for.
fn base64_sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::NeverCancelled;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A program name no host provides, so any spawn would fail loudly and visibly.
    const ABSENT_PROGRAM: &str = "zuno-definitely-not-a-real-ripgrep";

    fn discovered(version: &str) -> Discovery {
        Discovery {
            program: PathBuf::from("rg"),
            version: version.to_owned(),
        }
    }

    #[test]
    fn system_discovery_reports_a_supported_version() {
        let engine = Ripgrep::discover().expect("test host provides supported rg");
        assert!(engine.program().is_file());
        let major = engine
            .version()
            .expect("discovery records version")
            .split('.')
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .expect("major version");
        assert!(major >= MINIMUM_RIPGREP_MAJOR);
    }

    #[test]
    fn deferred_system_construction_does_not_require_discovery() {
        let engine = Ripgrep::deferred_system();
        assert_eq!(engine.program(), Path::new("rg"));
        assert_eq!(engine.version(), None);
        assert_eq!(engine.discovery, DiscoveryPolicy::DeferredSystem);
    }

    #[test]
    fn a_non_match_record_is_skipped() {
        let begin = r#"{"type":"begin","data":{"path":{"text":"./a.ts"}}}"#;
        assert!(matches!(
            parse_match(begin).expect("begin parses"),
            Record::Other
        ));
    }

    #[test]
    fn a_match_record_is_reshaped() {
        let line = r#"{"type":"match","data":{"path":{"text":"./src/a.ts"},"lines":{"text":"alpha needle here\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":6,"end":12}]}}"#;
        let Record::Match(found) = parse_match(line).expect("record parses") else {
            panic!("the record is a match");
        };
        assert_eq!(found.entry.path, "src/a.ts");
        assert_eq!(found.line, 1);
        assert_eq!(found.submatches[0].start, 6);
    }

    #[test]
    fn malformed_json_is_typed() {
        assert!(matches!(
            parse_match("{not json").expect_err("malformed"),
            SearchError::Ripgrep { .. }
        ));
    }

    #[test]
    fn invalid_pattern_diagnostics_are_classified() {
        assert!(is_invalid_regex("regex parse error:\n  unclosed group"));
        assert!(is_invalid_regex("error parsing regex: bad"));
        assert!(is_invalid_glob("invalid glob pattern"));
        assert!(is_invalid_glob("error parsing glob: bad"));
    }

    #[test]
    fn a_cached_successful_discovery_is_never_reprobed() {
        let probes = AtomicUsize::new(0);
        let probe = || -> Result<Discovery, DiscoveryFailure> {
            probes.fetch_add(1, Ordering::SeqCst);
            Ok(discovered("14.1.1"))
        };
        let cache = DiscoveryCache::new();
        let start = Instant::now();

        let first = cache.resolve(start, &probe).expect("the probe succeeds");
        let much_later = cache
            .resolve(start + DISCOVERY_RETRY_COOLDOWN * 100, &probe)
            .expect("the cached success is reused");

        assert_eq!(first.version, "14.1.1");
        assert_eq!(much_later.program, first.program);
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cached_failed_discovery_is_reprobed_after_the_cooldown() {
        let probes = AtomicUsize::new(0);
        let probe = || -> Result<Discovery, DiscoveryFailure> {
            if probes.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(DiscoveryFailure::Missing)
            } else {
                Ok(discovered("14.1.1"))
            }
        };
        let cache = DiscoveryCache::new();
        let start = Instant::now();

        let missing = cache
            .resolve(start, &probe)
            .expect_err("ripgrep is absent on the first probe");
        assert!(matches!(
            missing.into_search_error(),
            SearchError::Unavailable { .. }
        ));

        // Inside the cooldown the failure is reused, so a model looping on `grep`
        // cannot make Zuno spawn one `rg --version` per call.
        assert!(
            cache
                .resolve(start + DISCOVERY_RETRY_COOLDOWN / 2, &probe)
                .is_err()
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1);

        // Once it elapses the next call re-probes and sees the freshly installed
        // binary, with no process restart.
        let installed = cache
            .resolve(start + DISCOVERY_RETRY_COOLDOWN, &probe)
            .expect("ripgrep installed mid-session becomes usable");
        assert_eq!(installed.version, "14.1.1");
        assert_eq!(probes.load(Ordering::SeqCst), 2);

        // And that success is cached for the process lifetime like any other.
        cache
            .resolve(start + DISCOVERY_RETRY_COOLDOWN * 100, &probe)
            .expect("the recovered success is cached");
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_supported_version_line_is_accepted() {
        assert_eq!(accepted_version("ripgrep 14.1.1"), Some("14.1.1"));
        assert_eq!(accepted_version("ripgrep 15.0.0"), Some("15.0.0"));
        // The exact shape `rg --version` prints when built from a revision.
        assert_eq!(
            accepted_version("ripgrep 15.1.0 (rev af60c2de9d)"),
            Some("15.1.0")
        );
        assert_eq!(
            accepted_version("ripgrep 14.1.1 (rev abcdef)"),
            Some("14.1.1")
        );
    }

    #[test]
    fn an_unsupported_or_unrecognised_version_line_is_rejected() {
        assert_eq!(accepted_version("ripgrep 13.0.0"), None);
        assert_eq!(accepted_version(""), None);
        assert_eq!(accepted_version("grep (GNU grep) 3.11"), None);
        assert_eq!(accepted_version("rg 14.1.1"), None);
        assert_eq!(accepted_version("ripgrep vNEXT"), None);
    }

    #[test]
    fn an_unsupported_version_is_unavailable_rather_than_model_correctable() {
        let error = DiscoveryFailure::Version {
            program: PathBuf::from("/usr/bin/rg"),
            found: "ripgrep 13.0.0".to_owned(),
        }
        .into_search_error();

        assert!(matches!(error, SearchError::Unavailable { .. }));
        assert!(!error.is_model_correctable());
    }

    #[test]
    fn a_missing_search_root_is_typed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = dir.path().join("no-such-directory");

        let error = validate_root(&missing).expect_err("a missing root fails");

        assert!(matches!(&error, SearchError::RootMissing { root } if root == &missing));
        assert!(!error.is_model_correctable());
    }

    #[test]
    fn a_search_root_that_is_a_file_is_typed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let file = dir.path().join("a.ts");
        std::fs::write(&file, "needle\n").expect("a fixture file");

        let error = validate_root(&file).expect_err("a file root fails");

        assert!(matches!(&error, SearchError::RootNotDirectory { root } if root == &file));
        assert!(error.is_model_correctable());
    }

    #[test]
    fn a_directory_search_root_is_accepted() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(validate_root(dir.path()).is_ok());
    }

    #[test]
    fn glob_rejects_a_missing_root_before_spawning() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = dir.path().join("no-such-directory");
        let engine = Ripgrep::new(ABSENT_PROGRAM);

        let error = engine
            .glob(&GlobRequest::new(&missing, "**/*.ts", 10), &NeverCancelled)
            .expect_err("a missing root fails");

        // `RootMissing` rather than `Spawn` is the proof that validation runs before
        // any process is started.
        assert!(matches!(&error, SearchError::RootMissing { root } if root == &missing));
    }

    #[test]
    fn grep_rejects_a_missing_root_before_spawning() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = dir.path().join("no-such-directory");
        let engine = Ripgrep::new(ABSENT_PROGRAM);

        let error = engine
            .grep(&GrepRequest::new(&missing, "needle", 10), &NeverCancelled)
            .expect_err("a missing root fails");

        assert!(matches!(&error, SearchError::RootMissing { root } if root == &missing));
    }

    #[test]
    fn grep_rejects_a_file_root_before_spawning() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let file = dir.path().join("a.ts");
        std::fs::write(&file, "needle\n").expect("a fixture file");
        let engine = Ripgrep::new(ABSENT_PROGRAM);

        let error = engine
            .grep(&GrepRequest::new(&file, "needle", 10), &NeverCancelled)
            .expect_err("a file root fails");

        assert!(matches!(&error, SearchError::RootNotDirectory { root } if root == &file));
    }

    #[test]
    fn a_rejected_include_glob_is_classified_even_though_grep_also_carries_a_regex() {
        // The invocation `grep` builds carries both patterns. While only the regex was
        // classified, this stderr matched no arm, exit 2 was accepted, and a search
        // that never ran was rendered as "no matches".
        let stderr = "error parsing glob '[': unclosed character class; missing ']'";
        let patterns = [Pattern::Regex("needle"), Pattern::Glob("[")];

        let error = classify_pattern(stderr, &patterns).expect("the include is classified");

        assert!(matches!(&error, SearchError::InvalidGlob { pattern, .. } if pattern == "["));
        assert!(error.is_model_correctable());
    }

    #[test]
    fn a_rejected_regex_is_classified_ahead_of_a_valid_include() {
        let stderr = "regex parse error:\n  unclosed group";
        let patterns = [Pattern::Regex("(unclosed"), Pattern::Glob("*.ts")];

        let error = classify_pattern(stderr, &patterns).expect("the regex is classified");

        assert!(
            matches!(&error, SearchError::InvalidPattern { pattern, .. } if pattern == "(unclosed")
        );
    }

    #[test]
    fn a_diagnostic_naming_neither_pattern_is_left_to_the_status_rules() {
        // A walk diagnostic: it explains neither pattern, so the status rules decide.
        assert!(
            classify_pattern(
                "rg: ./locked: Permission denied (os error 13)",
                &[Pattern::Regex("needle"), Pattern::Glob("*.ts")]
            )
            .is_none()
        );
    }

    #[test]
    fn a_regex_over_the_compiled_size_limit_is_attributed_to_that_regex() {
        // Verbatim from ripgrep 15.1.0 for `a{1000}{1000}{100}`. It is a property of
        // the model's own pattern, so it must not fall through to the opaque,
        // not-correctable backend failure.
        let error = classify_pattern(
            "compiled regex exceeds size limit of 104857600",
            &[Pattern::Regex("a{1000}{1000}{100}")],
        )
        .expect("the regex is classified");

        assert!(matches!(&error, SearchError::InvalidPattern { pattern, .. }
                if pattern == "a{1000}{1000}{100}"));
        assert!(error.is_model_correctable());
    }

    #[test]
    fn a_diagnostic_that_quotes_the_include_glob_is_never_blamed_on_the_regex() {
        // Verbatim from ripgrep 15.1.0 for `--glob=[regex exceeds size limit`. Every
        // wording rule below is satisfiable by text the *caller* supplies, because `rg`
        // echoes the offending glob onto stderr: this stderr contains both "regex" and
        // "exceeds size limit", so the substring rules answered
        // `InvalidPattern { pattern: "needle" }` for a call whose regex was fine.
        for include in ["[regex exceeds size limit", "[regex parse error"] {
            let stderr = format!(
                "rg: error parsing glob '{include}': unclosed character class; missing ']'"
            );
            let error =
                classify_pattern(&stderr, &[Pattern::Regex("needle"), Pattern::Glob(include)])
                    .expect("the quoted glob is classified");

            assert!(
                matches!(&error, SearchError::InvalidGlob { pattern, .. } if pattern == include),
                "the failure must name the glob rg quoted: {error:?}"
            );
        }
    }

    #[test]
    fn a_broken_regex_is_still_blamed_on_the_regex_when_the_include_is_valid() {
        // The other direction of the same rule, and the reason glob cannot simply win
        // outright: `rg`'s regex diagnostic contains the words a *glob* diagnostic uses
        // ("unclosed character class"), so a rule that preferred globs would blame a
        // perfectly good `include` for the model's broken pattern. Verbatim from
        // ripgrep 15.1.0 for `-- '[abc'` with `--glob=*.ts`.
        let stderr = "regex parse error:\n    (?:[abc)\n       ^\nerror: unclosed character class";

        let error = classify_pattern(stderr, &[Pattern::Regex("[abc"), Pattern::Glob("*.ts")])
            .expect("the regex is classified");

        assert!(
            matches!(&error, SearchError::InvalidPattern { pattern, .. } if pattern == "[abc"),
            "unexpected attribution: {error:?}"
        );
    }

    #[test]
    fn a_size_limit_diagnostic_that_names_no_regex_is_not_blamed_on_the_pattern() {
        // The conjunction is deliberate: "exceeds size limit" alone also describes
        // limits that have nothing to do with the pattern, and mislabelling one of
        // those would send the model round a loop editing a regex that was fine.
        assert!(!is_invalid_regex("File larger than the size limit"));
        assert!(!is_invalid_regex("exceeds size limit"));
        assert!(is_invalid_regex(
            "compiled regex exceeds size limit of 104857600"
        ));
    }

    #[test]
    fn a_refused_invocation_is_typed_as_the_models_to_correct() {
        // Exit 2, a diagnostic, and nothing on stdout: `rg` threw the invocation out
        // before it looked at a file. `SearchError::Ripgrep` would have reached
        // `map_search_error` as the non-retryable `ToolError::Failed`.
        let error = classify_status(
            Some(2),
            b"rg: something the classifier does not recognise",
            &[Pattern::Regex("needle")],
            false,
        )
        .expect_err("a refused invocation is a failure");

        assert!(matches!(&error, SearchError::Rejected { .. }));
        assert!(error.is_model_correctable());
    }

    #[test]
    fn an_over_broad_search_is_typed_as_the_models_to_correct() {
        // The message tells the model to narrow the call, so the taxonomy has to agree
        // that narrowing the call is available to it.
        let error = stdout_limit_exceeded();

        assert!(matches!(&error, SearchError::TooBroad { .. }));
        assert!(error.is_model_correctable());
        assert!(error.to_string().contains("narrow the search"));
    }

    #[test]
    fn a_path_reported_as_base64_bytes_is_not_turned_into_a_lossy_identifier() {
        // `bad\xff.txt` as `rg --json` reports it. The path is an identifier the model
        // feeds back into `read` and `edit`, so a U+FFFD rendering of it names no file
        // on disk; the record is rejected here and dropped by the caller instead.
        let record = r#"{"type":"match","data":{"path":{"bytes":"YmFk/y50eHQ="},"lines":{"text":"needle odd\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":0,"end":6}]}}"#;

        // `Unnameable`, deliberately not an `Err`. The record proves the pattern is
        // present, so it costs `truncated` in the caller; making it an error let a tree
        // whose matches all live in files with legacy names fail the whole search.
        assert!(matches!(
            parse_match(record).expect("an unnameable path is not a decode failure"),
            Record::Unnameable
        ));
    }

    #[test]
    fn a_line_reported_as_base64_bytes_still_becomes_a_match() {
        // Exactly what `rg --json` emits for a matching line that is not valid UTF-8.
        let record = r#"{"type":"match","data":{"path":{"text":"./bad.txt"},"lines":{"bytes":"bmVlZGxlIP/+IGJhZAo="},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":0,"end":6}]}}"#;

        let Record::Match(found) =
            parse_match(record).expect("an undecodable line is not a failure")
        else {
            panic!("the record is a match");
        };

        assert_eq!(found.entry.path, "bad.txt");
        assert!(found.text.starts_with("needle "));
        assert!(
            found.text.contains('\u{fffd}'),
            "the invalid bytes are replaced rather than rejected: {:?}",
            found.text
        );
    }

    #[test]
    fn base64_decoding_covers_every_padding_length_and_rejects_junk() {
        assert_eq!(decode_base64("bmVlZGxl").as_deref(), Some(&b"needle"[..]));
        assert_eq!(decode_base64("YWI=").as_deref(), Some(&b"ab"[..]));
        assert_eq!(decode_base64("YQ==").as_deref(), Some(&b"a"[..]));
        assert_eq!(decode_base64("//8=").as_deref(), Some(&[0xff, 0xff][..]));
        assert_eq!(decode_base64("YQ="), None, "a group that is not four wide");
        assert_eq!(
            decode_base64("Y!=="),
            None,
            "a character outside the alphabet"
        );
        assert_eq!(decode_base64("YQ==YQ=="), None, "padding inside the body");
    }

    /// A reader whose consumption is observable, so the drain past the cap can be
    /// asserted: a reader that simply stopped at the cap would deadlock a real child
    /// on a full pipe instead of bounding the search.
    struct CountingReader {
        remaining: usize,
        consumed: Arc<AtomicUsize>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let count = buf.len().min(self.remaining);
            buf[..count].fill(b'x');
            self.remaining -= count;
            self.consumed.fetch_add(count, Ordering::SeqCst);
            Ok(count)
        }
    }

    #[test]
    fn a_stream_past_the_cap_is_truncated_flagged_and_still_drained() {
        let consumed = Arc::new(AtomicUsize::new(0));
        let overflowed = Arc::new(AtomicBool::new(false));
        let reader = CountingReader {
            remaining: 4 * READ_CHUNK_BYTES,
            consumed: Arc::clone(&consumed),
        };

        let kept = join_stream(
            read_stream(reader, 1024, Some(Arc::clone(&overflowed))),
            "stdout",
        )
        .expect("the reader finishes");

        assert_eq!(kept.len(), 1024, "only the cap is buffered");
        assert!(overflowed.load(Ordering::SeqCst));
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            4 * READ_CHUNK_BYTES,
            "the pipe the child is still writing to must be drained"
        );
    }

    #[test]
    fn a_stream_within_the_cap_is_kept_whole_and_not_flagged() {
        let overflowed = Arc::new(AtomicBool::new(false));
        let reader = std::io::Cursor::new(vec![b'x'; 1024]);

        let kept = join_stream(
            read_stream(reader, 1024, Some(Arc::clone(&overflowed))),
            "stdout",
        )
        .expect("the reader finishes");

        assert_eq!(kept.len(), 1024);
        assert!(
            !overflowed.load(Ordering::SeqCst),
            "exactly the cap is not an overflow"
        );
    }
}
